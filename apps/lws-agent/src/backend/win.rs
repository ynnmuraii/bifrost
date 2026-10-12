//! `platform-win` backend: `WH_KEYBOARD_LL`/`WH_MOUSE_LL` capture on a dedicated
//! thread plus `SendInput` injection (spike KVM-010/011).
//!
//! The adapter has no `begin_capture`/`end_capture` of its own: capture is
//! `Capture::start` (installs the hooks) and `set_mode` (suppresses or passes
//! through), injection is `Injector::inject`. This module is the thin translation
//! between those and the agent's vocabulary.
//!
//! Known gaps of this slice, stated rather than hidden:
//!
//! * `EndCapture` drops the capture object, which uninstalls the hooks; re-arming
//!   creates them again. There is no local replay of a buffer held during
//!   `TEMPORARY`, because this agent forwards events as it captures them and never
//!   keeps one (the «не потерять нажатие в Preparing» path needs the buffer that
//!   KVM-013/026 adds).
//! * `ReleaseKeys` injects key/button ups for exactly the entries core sends. The
//!   `GetAsyncKeyState` pre-check the documents ask for at spike KVM-011 is not
//!   called here; the injector's own ledger decides what it still holds.

use std::time::Duration;

use input_model::{CaptureKind, HidKey, InputEvent, MouseButton};
use platform_win::capture::{Capture, CaptureConfig, CaptureError, CaptureEvent};
use platform_win::event::EmergencyCombo;
use platform_win::inject::Injector;

use crate::backend::{Event, Injected, Ready, Released, TakeoverPlan, POLL_TICK};
use crate::takeover::Detector;

/// The Windows input path.
pub struct Win {
    ready: Ready,
    capture: Option<Capture>,
    injector: Injector,
    emergency: EmergencyCombo,
    /// The receiver-takeover detector, when `[input].takeover_detection` armed it.
    ///
    /// It is a *second*, observe-only capture: while this node is a receiver its own
    /// keyboard and mouse still work, and the low-level hooks are the only user-mode place
    /// that sees them without suppressing them. Injected events never reach the detector —
    /// the hook drops `LLKHF_INJECTED`/`LLMHF_INJECTED` and our `dwExtraInfo` tag before
    /// anything is queued (`platform-win`, «Injected-event loop prevention»).
    detector: Option<(Detector, Capture)>,
    plan: Option<TakeoverPlan>,
}

impl Win {
    /// Builds the Windows backend. Always succeeds; capture hooks are installed on
    /// the first `BeginCapture`, so an agent that never captures never suppresses.
    pub fn new(
        ready: &Ready,
        emergency_hotkey: &str,
        plan: Option<TakeoverPlan>,
    ) -> Result<Self, String> {
        let emergency = parse_emergency_hotkey(emergency_hotkey);
        tracing::info!(
            vk = emergency.vk,
            ctrl = emergency.ctrl,
            alt = emergency.alt,
            shift = emergency.shift,
            "emergency combination armed in the capture hook"
        );
        Ok(Win {
            ready: ready.clone(),
            capture: None,
            injector: Injector::new(),
            emergency,
            detector: None,
            plan,
        })
    }

    pub fn ready(&self) -> Ready {
        self.ready.clone()
    }

    pub fn begin_capture(&mut self, kind: CaptureKind) -> Result<(), String> {
        match &self.capture {
            Some(capture) => {
                capture.set_mode(kind);
                Ok(())
            }
            None => {
                let config = CaptureConfig {
                    mode: kind,
                    emergency: self.emergency,
                    ..CaptureConfig::default()
                };
                match Capture::start(config) {
                    Ok(capture) => {
                        self.capture = Some(capture);
                        Ok(())
                    }
                    Err(error) => Err(capture_error(&error)),
                }
            }
        }
    }

    pub fn end_capture(&mut self) -> Result<Released, String> {
        // Dropping the capture stops its thread and uninstalls both hooks, so
        // nothing of this session can keep suppressing input (S1/S2/S3).
        if self.capture.take().is_none() {
            tracing::debug!("EndCapture with no active capture");
        }
        self.disarm_detector();
        Ok(self.release_session())
    }

    /// Stops watching for physical input: the lease that made it meaningful is over.
    fn disarm_detector(&mut self) {
        if let Some((detector, capture)) = self.detector.take() {
            tracing::info!(
                triggers = detector.triggers(),
                injected_seen = capture.stats().injected_seen,
                "receiver takeover detector disarmed"
            );
        }
    }

    /// Prepares injection — and arms the receiver-takeover detector.
    ///
    /// `PrepareInject` is the moment a lease makes this node the receiver
    /// (`docs/05-protocol.md`), which is exactly when «physical input on the receiver ends
    /// the lease» starts to mean something (rule 9). The detector is an observe-only
    /// capture: no suppression, no forwarding, one trigger.
    pub fn prepare_inject(&mut self) -> Result<(), String> {
        if let Some(plan) = self.plan {
            if self.detector.is_none() {
                let config = CaptureConfig {
                    mode: CaptureKind::Observe,
                    emergency: self.emergency,
                    ..CaptureConfig::default()
                };
                match Capture::start(config) {
                    Ok(capture) => {
                        tracing::info!(
                            threshold_px = plan.threshold_px,
                            "receiver takeover detector armed (observe-only hooks)"
                        );
                        self.detector = Some((Detector::new(plan.threshold_px), capture));
                    }
                    Err(error) => {
                        // Not fatal: the lease still works, the user just keeps the local
                        // pointer until core or the emergency key ends it.
                        tracing::warn!(
                            %error,
                            "cannot arm the receiver takeover detector; a land on this \
                             machine will not end the lease"
                        );
                    }
                }
            }
        }
        Ok(())
    }

    pub fn inject(&mut self, events: &[InputEvent]) -> Injected {
        let status = self.injector.inject(events);
        Injected {
            applied: status.applied,
            os_error: status
                .error
                .as_ref()
                .map(|error| error.to_string())
                .unwrap_or_default(),
            // `platform-win` reports the injected count but not the resulting
            // cursor position; the source keeps its own virtual position.
            actual: None,
        }
    }

    pub fn release_session(&mut self) -> Released {
        // `ReleaseSession` ends the receiver side of a lease as surely as `EndCapture`.
        self.disarm_detector();
        let keys_before = self.injector.ledger().key_count();
        let buttons_before = self.injector.ledger().button_count();
        let status = self.injector.release_all();
        if !status.is_complete() {
            tracing::warn!(%status, "some synthetic state stayed pressed");
        }
        // What the ledger kept afterwards is what a later attempt has to retry.
        let keys_after = self.injector.ledger().key_count();
        let buttons_after = self.injector.ledger().button_count();
        Released {
            keys: keys_before.saturating_sub(keys_after) as u32,
            buttons: buttons_before.saturating_sub(buttons_after) as u32,
        }
    }

    pub fn release_keys(&mut self, keys: &[HidKey], buttons: &[MouseButton]) -> Released {
        let mut events = Vec::with_capacity(keys.len() + buttons.len());
        events.extend(keys.iter().map(|key| InputEvent::Key {
            key: *key,
            state: input_model::KeyState::Up,
        }));
        events.extend(buttons.iter().map(|button| InputEvent::Button {
            button: *button,
            state: input_model::KeyState::Up,
        }));
        let status = self.injector.inject(&events);
        if !status.is_complete() {
            tracing::warn!(%status, "recovery release was not fully applied");
        }
        Released {
            keys: keys.len() as u32,
            buttons: buttons.len() as u32,
        }
    }

    /// Win32 monitors in logical (DIP) coordinates (KVM-040).
    ///
    /// A failure is reported as an empty set so core can fall back to the
    /// `[[topology.displays]]` snapshot.
    pub fn displays(&mut self) -> Vec<crate::displays::RawDisplay> {
        match platform_win::displays::enumerate() {
            Ok(monitors) => monitors
                .into_iter()
                .map(|monitor| crate::displays::RawDisplay {
                    id: monitor.id,
                    logical: monitor.logical,
                    native: Some(monitor.native),
                    scale: monitor.scale,
                    primary: monitor.primary,
                })
                .collect(),
            Err(error) => {
                tracing::warn!(%error, "cannot enumerate Win32 monitors");
                Vec::new()
            }
        }
    }

    pub async fn next_event(&mut self) -> Option<Event> {
        if let Some(capture) = &self.capture {
            match capture.try_recv() {
                Some(CaptureEvent::Input(event)) => return Some(Event::Input(event)),
                Some(CaptureEvent::Emergency) => return Some(Event::Emergency),
                Some(CaptureEvent::QueueOverflow) => return Some(Event::QueueOverflow),
                None => {}
            }
        }
        if let Some((detector, capture)) = &mut self.detector {
            match capture.try_recv() {
                // Physical input on the receiver: the detector decides whether this one
                // crossed the threshold. Its events are never forwarded — this node is
                // injecting here, not capturing for someone else.
                Some(CaptureEvent::Input(event)) => {
                    if let Some(trigger) = detector.observe(&event) {
                        tracing::warn!(
                            reason = trigger.reason(),
                            distance_px = ?distance_of(trigger),
                            "physical input on the receiver; ending the lease"
                        );
                        return Some(Event::LocalTakeover {
                            reason: trigger.reason(),
                            distance_px: distance_of(trigger),
                        });
                    }
                }
                // The emergency combination works on a receiver too (rule 4): this capture
                // is the only place it can be seen while the source capture is not running.
                Some(CaptureEvent::Emergency) => return Some(Event::Emergency),
                Some(CaptureEvent::QueueOverflow) => {
                    // The detector's queue is a counter, not a stream: a dropped batch can
                    // miss a takeover, but ending the session over it would be worse.
                    tracing::debug!("receiver takeover detector queue overflowed");
                }
                None => {}
            }
        }
        tokio::time::sleep(POLL_TICK).await;
        None
    }

    /// The detector's own view of injected events, for the KVM-035 test.
    #[cfg(test)]
    fn injected_seen(&self) -> u64 {
        self.detector
            .as_ref()
            .map(|(_, capture)| capture.stats().injected_seen)
            .unwrap_or(0)
    }
}

fn capture_error(error: &CaptureError) -> String {
    format!("{error}")
}

/// The distance of a motion trigger, for the log and the event.
fn distance_of(trigger: crate::takeover::Trigger) -> Option<f64> {
    match trigger {
        crate::takeover::Trigger::Motion { distance_px } => Some(distance_px),
        crate::takeover::Trigger::Key | crate::takeover::Trigger::Button => None,
    }
}

/// `input.emergency_hotkey` → the hook's virtual-key combination.
///
/// The combination is a plain config string («Ctrl+Alt+Shift+F12»); this parses the
/// documented shape (one non-modifier key plus any of Ctrl/Alt/Shift) so the agent
/// does not have to link the `config` crate, whose `Hotkey::parse` owns the
/// validated form. An unparsable string keeps the default combination and is
/// logged: the emergency path must not be silently missing.
fn parse_emergency_hotkey(text: &str) -> EmergencyCombo {
    let default = EmergencyCombo::default();
    if text.trim().is_empty() {
        return default;
    }
    let mut combo = EmergencyCombo {
        vk: 0,
        ctrl: false,
        alt: false,
        shift: false,
    };
    let mut key: Option<u16> = None;
    for part in text.split('+').map(str::trim).filter(|p| !p.is_empty()) {
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => combo.ctrl = true,
            "alt" => combo.alt = true,
            "shift" => combo.shift = true,
            other => match virtual_key(other) {
                Some(vk) => key = Some(vk),
                None => {
                    tracing::warn!(
                        hotkey = text,
                        part = other,
                        "unparsable emergency hotkey part; keeping Ctrl+Alt+Shift+F12"
                    );
                    return default;
                }
            },
        }
    }
    match key {
        Some(vk) => {
            combo.vk = vk;
            combo
        }
        None => default,
    }
}

/// Virtual-key codes for the keys the emergency combination can name.
fn virtual_key(name: &str) -> Option<u16> {
    if let Some(rest) = name.strip_prefix('f') {
        if let Ok(index) = rest.parse::<u16>() {
            if (1..=24).contains(&index) {
                // VK_F1 = 0x70.
                return Some(0x70 + index - 1);
            }
        }
    }
    if name.len() == 1 {
        let ch = name.chars().next()?;
        if ch.is_ascii_alphabetic() {
            return Some(u16::from(ch.to_ascii_uppercase() as u8));
        }
        if ch.is_ascii_digit() {
            return Some(u16::from(ch as u8));
        }
    }
    match name {
        "escape" | "esc" => Some(0x1B),
        "space" => Some(0x20),
        "tab" => Some(0x09),
        _ => None,
    }
}

const _: () = {
    // Documents the tick reuse: the Windows backend never blocks longer than one
    // poll tick, which is what keeps the session loop a single `select!`.
    assert!(POLL_TICK.as_millis() <= Duration::from_millis(10).as_millis());
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_hotkey_maps_to_ctrl_alt_shift_f12() {
        let combo = parse_emergency_hotkey("Ctrl+Alt+Shift+F12");
        assert_eq!(
            combo,
            EmergencyCombo {
                vk: 0x7B,
                ctrl: true,
                alt: true,
                shift: true,
            }
        );
    }

    #[test]
    fn other_keys_and_case_are_understood() {
        assert_eq!(parse_emergency_hotkey("ctrl+shift+a").vk, 0x41);
        assert!(parse_emergency_hotkey("Ctrl+Alt+Shift+F1").vk == 0x70);
    }

    #[test]
    fn an_unknown_part_keeps_the_safe_default() {
        let combo = parse_emergency_hotkey("Ctrl+Alt+Shift+Hyper");
        assert_eq!(combo, EmergencyCombo::default());
    }

    /// The dry half of KVM-035's proof: an event this process injected must not end a lease.
    ///
    /// The detector runs exactly as it does on a receiver — an observe-only low-level hook —
    /// and the injection is the production one, which stamps every record with
    /// [`platform_win::INJECT_TAG`]. The assertion that matters is the pair: the hook *saw*
    /// the injected event and dropped it (`injected_seen` grew, so the test is not passing
    /// because nothing arrived), and no `LocalTakeover` was raised. Without the injected
    /// filtering in `platform-win` this test fails on its first poll.
    ///
    /// It injects one Shift tap (harmless anywhere) and therefore needs an interactive
    /// Windows desktop, which is why it is `#[ignore]`d:
    ///
    /// ```text
    /// cargo test -p lws-agent an_injected_event_does_not_take_over -- --ignored --nocapture
    /// ```
    #[tokio::test]
    #[ignore = "needs an interactive Windows desktop; injects one Shift tap"]
    async fn an_injected_event_does_not_take_over() {
        let ready = Ready {
            capabilities: Vec::new(),
            platform: "platform-win".to_owned(),
            session_kind: "windows".to_owned(),
            backend: ::protocol::ipc::BackendInfo::default(),
        };
        let mut win = Win::new(
            &ready,
            "Ctrl+Alt+Shift+F12",
            Some(TakeoverPlan { threshold_px: 12 }),
        )
        .expect("the Windows backend");
        win.prepare_inject().expect("arming the detector");
        // The hook thread installs asynchronously; give it a moment before injecting.
        tokio::time::sleep(Duration::from_millis(500)).await;

        let shift = HidKey::keyboard(0xE1);
        let injected = win.inject(&[
            InputEvent::Key {
                key: shift,
                state: input_model::KeyState::Down,
            },
            InputEvent::Key {
                key: shift,
                state: input_model::KeyState::Up,
            },
        ]);
        assert!(
            injected.os_error.is_empty(),
            "the injection must go out: {injected:?}"
        );

        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        while std::time::Instant::now() < deadline {
            if let Some(event) = win.next_event().await {
                panic!(
                    "an injected event raised {event:?}: the takeover detector must ignore \\
                        our own injections"
                );
            }
        }

        let seen = win.injected_seen();
        win.end_capture().expect("disarming the detector");
        assert!(
            seen > 0,
            "the hook saw no injected event at all: the test proves nothing"
        );
        println!("injected events seen and dropped: {seen}");
    }
}
