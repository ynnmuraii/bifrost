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

use std::time::{Duration, Instant};

use input_model::{CaptureKind, HidKey, InputEvent, MouseButton};
use platform_win::capture::{Capture, CaptureConfig, CaptureError, CaptureEvent, CaptureStats};
use platform_win::event::EmergencyCombo;
use platform_win::inject::Injector;

use crate::backend::{ClipboardChange, Event, Injected, Ready, Released, TakeoverPlan, POLL_TICK};
use crate::takeover::Detector;
use input_model::RepeatFilter;

/// Source motion state, shared by the capture adapter and deterministic session tests.
pub(crate) struct SourceMotion {
    position: Option<input_model::Position>,
    scale: f64,
}

impl SourceMotion {
    pub(crate) fn new(position: Option<input_model::Position>, scale: f64) -> Self {
        Self { position, scale }
    }

    pub(crate) fn normalize(&mut self, kind: CaptureKind, event: InputEvent) -> Option<InputEvent> {
        match event {
            InputEvent::PointerRelative { .. } if kind == CaptureKind::Observe => None,
            InputEvent::PointerRelative { dx, dy } => {
                let position = self.position.as_mut()?;
                position.x += f64::from(dx) / self.scale;
                position.y += f64::from(dy) / self.scale;
                // This source accumulator deliberately extends beyond its physical screens.
                Some(InputEvent::PointerAbsolute(*position))
            }
            InputEvent::PointerAbsolute(native) => {
                let logical =
                    input_model::Position::new(native.x / self.scale, native.y / self.scale);
                self.position = Some(logical);
                Some(InputEvent::PointerAbsolute(logical))
            }
            event => Some(event),
        }
    }

    pub(crate) fn seed_event(&self) -> Option<InputEvent> {
        self.position.map(InputEvent::PointerAbsolute)
    }
}

/// The Windows input path.
pub struct Win {
    ready: Ready,
    capture: Option<Capture>,
    source_motion: Option<SourceMotion>,
    source_monitors: Vec<platform_win::displays::Monitor>,
    pending_source_seed: bool,
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
    /// Source-side autorepeat filter: the OS repeats a held key for us, and those repeats
    /// must not reach the wire (`docs/05-protocol.md`: the receiver repeats, not the source).
    repeats: RepeatFilter,
    /// The local clipboard: the notification window of `platform_win::clipboard` plus the
    /// change counter. `None` when the window could not be created, in which case this
    /// backend has no clipboard and says so.
    ///
    /// # Echo suppression (KVM-041)
    ///
    /// A set of the clipboard moves `GetClipboardSequenceNumber`, and the notification the
    /// OS delivers for that set is indistinguishable from a user's copy *except* by the
    /// counter. So the value right after this backend's own `SetClipboardData` is remembered
    /// in `applied_sequence`, and the read that carries it is swallowed: exactly the payload
    /// this node applied from core, never someone else's copy. (Core has its own echo history
    /// on `(origin_node_id, origin_version)`; this is the same rule one hop earlier, at the
    /// OS edge, where only the platform's signal exists.)
    clipboard: Option<platform_win::clipboard::Clipboard>,
    /// `GetClipboardSequenceNumber` right after this backend's own write.
    applied_sequence: Option<u32>,
    last_capture_report: Option<Instant>,
    last_capture_stats: CaptureStats,
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
        // The listener is a message-only window on its own thread; a failure here costs the
        // clipboard and nothing else, so it is reported in the capability rather than
        // refused (ADR-008: the clipboard is a separate feature from the KVM).
        let clipboard = match platform_win::clipboard::Clipboard::listen() {
            Ok(clipboard) => Some(clipboard),
            Err(error) => {
                tracing::warn!(
                    %error,
                    "the clipboard listener window could not be created: this session will not \
                     do the clipboard"
                );
                None
            }
        };
        let mut ready = ready.clone();
        ready.set_text_clipboard(clipboard.is_some());
        Ok(Win {
            ready,
            capture: None,
            source_motion: None,
            source_monitors: Vec::new(),
            pending_source_seed: false,
            injector: Injector::new(),
            emergency,
            detector: None,
            plan,
            repeats: RepeatFilter::new(),
            clipboard,
            applied_sequence: None,
            last_capture_report: None,
            last_capture_stats: CaptureStats::default(),
        })
    }

    pub fn ready(&self) -> Ready {
        self.ready.clone()
    }

    /// Whether this backend's clipboard listener is up.
    pub fn clipboard_available(&self) -> bool {
        self.clipboard.is_some()
    }

    /// The next clipboard change the *user* made, if the listener queued one.
    ///
    /// Non-blocking on purpose: the session polls this from its own tick, and its watchdog and
    /// deadline live on that tick too. The *notification* never blocks; the read itself is
    /// Win32 work with a retry loop (`OpenClipboard` refuses while another process holds the
    /// clipboard), so it runs on a blocking thread. That keeps the runtime thread — the one
    /// the 5 ms poll tick, the IPC frames and the core-silence watchdog share — free: a busy
    /// loop must not be able to delay the `Pong`.
    pub async fn clipboard_change(&mut self) -> Option<ClipboardChange> {
        let clipboard = self.clipboard.as_ref()?;
        clipboard.wait(Duration::ZERO)?;
        let snapshot =
            match tokio::task::spawn_blocking(platform_win::clipboard::read_clipboard).await {
                Ok(Ok(snapshot)) => snapshot,
                Ok(Err(error)) => {
                    tracing::warn!(%error, "the clipboard could not be read; ignoring the change");
                    return None;
                }
                Err(error) => {
                    tracing::warn!(%error, "the clipboard read task failed; ignoring the change");
                    return None;
                }
            };
        if Some(snapshot.sequence) == self.applied_sequence {
            // Our own write, arriving as a notification. The next one to be noticed carries a
            // larger counter, so a stale value here can never swallow a later change.
            self.applied_sequence = None;
            tracing::debug!(
                sequence = snapshot.sequence,
                bytes = snapshot.text.len(),
                "swallowing the clipboard update this node applied"
            );
            return None;
        }
        if !snapshot.has_text {
            // An image-only or empty item: nothing this agent can offer.
            tracing::debug!(sequence = snapshot.sequence, "the clipboard holds no text");
            return None;
        }
        tracing::info!(
            sequence = snapshot.sequence,
            bytes = snapshot.text.len(),
            markers = snapshot.markers.len(),
            "local clipboard change"
        );
        Some(ClipboardChange {
            text: snapshot.text,
            markers: snapshot.markers,
        })
    }

    /// Owns the local selection: `Some` puts `text` on it, `None` empties it.
    ///
    /// The write is Win32 work with the same `OpenClipboard` retry loop as the read, so it
    /// runs on a blocking thread: the session's poll tick and its watchdog must not wait
    /// for another process's clipboard.
    pub async fn set_clipboard(&mut self, text: Option<&str>) -> Result<(), String> {
        let Some(clipboard) = self.clipboard.as_ref() else {
            return Err("CLIPBOARD_DISABLED: this session has no clipboard listener".to_owned());
        };
        let write = text.map(str::to_owned);
        tokio::task::spawn_blocking(move || {
            platform_win::clipboard::write_clipboard(write.as_deref())
        })
        .await
        .map_err(|error| {
            format!("CLIPBOARD_APPLY_FAILED: the clipboard write task failed: {error}")
        })?
        .map_err(|error| format!("CLIPBOARD_APPLY_FAILED: the clipboard was not taken: {error}"))?;
        // The counter after our own write is what the notification will carry.
        self.applied_sequence = Some(clipboard.sequence());
        tracing::info!(
            bytes = text.map(str::len).unwrap_or(0),
            released = text.is_none(),
            sequence = self.applied_sequence,
            "clipboard set"
        );
        Ok(())
    }

    pub fn begin_capture(
        &mut self,
        kind: CaptureKind,
        capture_injected: bool,
    ) -> Result<(), String> {
        match &self.capture {
            Some(capture) => {
                capture.set_capture_injected(capture_injected);
                capture.set_mode(kind);
            }
            None => {
                let monitors = platform_win::displays::enumerate()
                    .map_err(|error| format!("cannot seed source motion: {error}"))?;
                let native = Capture::cursor_position()
                    .map_err(|error| format!("cannot seed source motion: {error}"))?;
                let scale = monitor_scale(&monitors, native).ok_or_else(|| {
                    "cannot seed source motion: cursor is outside known monitors".to_owned()
                })?;
                let motion = SourceMotion::new(
                    Some(input_model::Position::new(
                        native.x / scale,
                        native.y / scale,
                    )),
                    scale,
                );
                let config = CaptureConfig {
                    mode: kind,
                    emergency: self.emergency,
                    capture_injected,
                    ..CaptureConfig::default()
                };
                let capture = Capture::start(config).map_err(|error| capture_error(&error))?;
                self.source_motion = Some(motion);
                self.source_monitors = monitors;
                // Core needs a baseline before the first delta, including manual capture
                // that had no preceding Observe motion.
                self.pending_source_seed = true;
                self.capture = Some(capture);
            }
        }
        self.last_capture_stats = self
            .capture
            .as_ref()
            .map(Capture::stats)
            .unwrap_or_default();
        self.last_capture_report = Some(Instant::now());
        tracing::info!(
            ?kind,
            capture_injected,
            raw_input_ok = self.last_capture_stats.raw_input_ok,
            "Windows keyboard/mouse capture started"
        );
        Ok(())
    }

    pub fn end_capture(&mut self) -> Result<Released, String> {
        self.report_capture_stats(true);
        // Dropping the capture stops its thread and uninstalls both hooks, so
        // nothing of this session can keep suppressing input (S1/S2/S3).
        if self.capture.take().is_none() {
            tracing::debug!("EndCapture with no active capture");
        }
        self.source_motion = None;
        self.source_monitors.clear();
        self.pending_source_seed = false;
        self.last_capture_report = None;
        self.last_capture_stats = CaptureStats::default();
        // The keys held now are released locally; their ups never reach this filter, and a
        // stale pressed set would eat the next real press.
        self.repeats.clear();
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
        if let Some(capture) = &self.capture {
            capture.set_capture_injected(false);
        }
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
    ///
    /// `EnumDisplayMonitors` is a synchronous GDI enumeration — on a loaded machine it
    /// takes long enough to matter, and the session answers `QueryDisplays` on the same
    /// task the heartbeat `Pong` rides, so the enumeration runs on a blocking thread.
    /// The call is per-desktop and every thread of this process shares one desktop, so
    /// the result is the same off the runtime thread.
    pub async fn displays(&mut self) -> Vec<crate::displays::RawDisplay> {
        let enumerated = match tokio::task::spawn_blocking(platform_win::displays::enumerate).await
        {
            Ok(Ok(monitors)) => monitors,
            Ok(Err(error)) => {
                tracing::warn!(%error, "cannot enumerate Win32 monitors");
                return Vec::new();
            }
            Err(error) => {
                tracing::warn!(%error, "the monitor enumeration task failed");
                return Vec::new();
            }
        };
        enumerated
            .into_iter()
            .map(|monitor| crate::displays::RawDisplay {
                id: monitor.id,
                logical: monitor.logical,
                native: Some(monitor.native),
                scale: monitor.scale,
                primary: monitor.primary,
            })
            .collect()
    }

    pub async fn next_event(&mut self) -> Option<Event> {
        self.report_capture_stats(false);
        if self.pending_source_seed {
            self.pending_source_seed = false;
            if let Some(event) = self
                .source_motion
                .as_ref()
                .and_then(SourceMotion::seed_event)
            {
                return Some(Event::Input(event));
            }
        }
        if let Some(capture) = &self.capture {
            match capture.try_recv() {
                Some(CaptureEvent::Input(event)) => {
                    // Windows has no per-event repeat flag: a down for a key already in the
                    // pressed set is the OS repeating it. Dropped here, at capture.
                    if self.repeats.is_repeat(&event, false) {
                        tracing::trace!(?event, "dropping an OS autorepeat");
                        return None;
                    }
                    let motion = self.source_motion.as_mut()?;
                    if let InputEvent::PointerAbsolute(native) = event {
                        if let Some(scale) = monitor_scale(&self.source_monitors, native) {
                            motion.scale = scale;
                        }
                    }
                    return motion.normalize(capture.mode(), event).map(Event::Input);
                }
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
    /// Emit changed hook counters at most once a second, including the effective
    /// capture mode and injected-event opt-in. This makes a silent hook/filter/mapping
    /// failure distinguishable from an idle source without logging every key event.
    fn report_capture_stats(&mut self, force: bool) {
        let Some(capture) = self.capture.as_ref() else {
            return;
        };
        let now = Instant::now();
        if !force
            && self
                .last_capture_report
                .is_some_and(|last| now.duration_since(last) < Duration::from_secs(1))
        {
            return;
        }
        let stats = capture.stats();
        let previous = self.last_capture_stats;
        if stats != previous {
            tracing::info!(
                mode = ?stats.mode,
                capture_injected = stats.capture_injected,
                hook_calls = stats.hook_calls.saturating_sub(previous.hook_calls),
                keyboard_events = stats
                    .keyboard_hook_events
                    .saturating_sub(previous.keyboard_hook_events),
                injected = stats
                    .keyboard_injected_seen
                    .saturating_sub(previous.keyboard_injected_seen),
                filtered = stats.keyboard_filtered.saturating_sub(previous.keyboard_filtered),
                mapped = stats.keyboard_mapped.saturating_sub(previous.keyboard_mapped),
                mouse_hook_moves = stats
                    .hook_mouse_moves
                    .saturating_sub(previous.hook_mouse_moves),
                raw_mouse_moves = stats
                    .raw_mouse_moves
                    .saturating_sub(previous.raw_mouse_moves),
                raw_absolute = stats.raw_absolute.saturating_sub(previous.raw_absolute),
                raw_synth_suppressed = stats
                    .raw_synth_suppressed
                    .saturating_sub(previous.raw_synth_suppressed),
                recenter_count = stats.recenters.saturating_sub(previous.recenters),
                unmapped = stats.unmapped_keys.saturating_sub(previous.unmapped_keys),
                suppressed = stats
                    .keyboard_suppressed
                    .saturating_sub(previous.keyboard_suppressed),
                pushes = stats.captured.saturating_sub(previous.captured),
                dropped = stats.dropped.saturating_sub(previous.dropped),
                "Windows capture hook counters"
            );
        }
        self.last_capture_report = Some(now);
        self.last_capture_stats = stats;
    }
}

fn monitor_scale(
    monitors: &[platform_win::displays::Monitor],
    point: input_model::Position,
) -> Option<f64> {
    monitors
        .iter()
        .find(|monitor| {
            let [x, y, width, height] = monitor.native.map(f64::from);
            point.x >= x && point.x < x + width && point.y >= y && point.y < y + height
        })
        .map(|monitor| monitor.scale)
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

    /// Does Windows repeat a *held injected* key? (KVM-036 decides the receiver engine on it.)
    ///
    /// `docs/03-architecture.md`: «на X11 и Windows он останавливает собственный repeat engine»,
    /// which assumes the OS repeats a held `SendInput` key. This test answers that with a
    /// measurement instead of an assumption: an observe-only capture with
    /// `capture_injected` on (the one place that flag is legitimate — it is not the agent's
    /// capture path) counts the key-downs the OS itself produces for one injected `F24`
    /// held down for 1.5 s. `F24` types nothing anywhere.
    #[tokio::test]
    #[ignore = "needs an interactive Windows desktop; holds F24 down for 1.5 s"]
    async fn an_injected_key_does_or_does_not_autorepeat() {
        let config = CaptureConfig {
            mode: CaptureKind::Observe,
            capture_injected: true,
            ..CaptureConfig::default()
        };
        let capture = Capture::start(config).expect("a capture");
        let mut injector = Injector::new();
        tokio::time::sleep(Duration::from_millis(400)).await;
        let f24 = HidKey::keyboard(0x04); // 'a': a key the injector and the hook both map
        let status = injector.inject(&[InputEvent::Key {
            key: f24,
            state: input_model::KeyState::Down,
        }]);
        println!("inject status: {status:?}");
        let deadline = std::time::Instant::now() + Duration::from_millis(1000);
        let mut downs = 0u32;
        while std::time::Instant::now() < deadline {
            if let Some(CaptureEvent::Input(InputEvent::Key {
                key,
                state: input_model::KeyState::Down,
            })) = capture.try_recv()
            {
                if key == f24 {
                    downs += 1;
                }
            } else {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
        injector.inject(&[InputEvent::Key {
            key: f24,
            state: input_model::KeyState::Up,
        }]);
        println!("F24 downs the OS produced for one injected down: {downs}");
        println!("stats: {:?}", capture.stats());
        drop(capture);
        assert!(downs >= 1, "even the injected down itself must be seen");
    }

    #[tokio::test]
    #[ignore = "needs an interactive Windows desktop; bounded mouse motion and Shift tap"]
    async fn rearmed_remote_backend_forwards_motion_and_keyboard() {
        use windows::Win32::UI::Input::KeyboardAndMouse::{
            SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP,
            KEYEVENTF_SCANCODE, MOUSEEVENTF_MOVE, MOUSEINPUT,
        };
        let ready = Ready {
            capabilities: Vec::new(),
            platform: "platform-win".to_owned(),
            session_kind: "windows".to_owned(),
            backend: ::protocol::ipc::BackendInfo::default(),
        };
        let mut win = Win::new(&ready, "Ctrl+Alt+Shift+F12", None).expect("backend");
        win.begin_capture(CaptureKind::Observe, true)
            .expect("observe");
        win.begin_capture(CaptureKind::Temporary, true)
            .expect("prepare");
        win.end_capture().expect("abort");
        win.begin_capture(CaptureKind::Temporary, true)
            .expect("rearm");
        win.begin_capture(CaptureKind::Remote, true)
            .expect("remote");
        let source = win.source_motion.as_ref().expect("source motion seed");
        let seed = source.position.expect("seeded position");
        let expected_motion =
            input_model::Position::new(seed.x + 3.0 / source.scale, seed.y - 2.0 / source.scale);
        let key = |flags| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wScan: 0x2a,
                    dwFlags: flags,
                    ..Default::default()
                },
            },
        };
        let input = [
            INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT {
                        dx: 3,
                        dy: -2,
                        dwFlags: MOUSEEVENTF_MOVE,
                        ..Default::default()
                    },
                },
            },
            key(KEYEVENTF_SCANCODE),
            key(KEYEVENTF_SCANCODE | KEYEVENTF_KEYUP),
        ];
        // SAFETY: bounded untagged dev input; Remote suppresses the local tap.
        assert_eq!(
            unsafe { SendInput(&input, std::mem::size_of::<INPUT>() as i32) },
            3
        );
        let deadline = Instant::now() + Duration::from_secs(1);
        let (mut motion, mut down, mut up) = (false, false, false);
        while Instant::now() < deadline && !(motion && down && up) {
            match win.next_event().await {
                Some(Event::Input(InputEvent::PointerAbsolute(position)))
                    if position == expected_motion =>
                {
                    motion = true
                }
                Some(Event::Input(InputEvent::Key { key, state }))
                    if key == HidKey::keyboard(0xe1) =>
                {
                    down |= state == input_model::KeyState::Down;
                    up |= state == input_model::KeyState::Up;
                }
                _ => {}
            }
        }
        win.end_capture().expect("cleanup");
        assert!(motion && down && up, "motion={motion} down={down} up={up}");
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
