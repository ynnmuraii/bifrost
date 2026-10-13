//! `platform-x11` backend: XInput2 capture with an explicit `XGrabPointer`/
//! `XGrabKeyboard` and XTest injection (spikes KVM-012/013).
//!
//! The grab is what makes the transition atomic: `middle of a session without it
//! nothing suppresses local input, so `BeginCapture` of a suppressing kind fails
//! instead of "working" without suppression.
//!
//! Known gaps of this slice, stated rather than hidden:
//!
//! * The emergency combination comes from the platform crate's fixed F12 keycode
//!   (`platform_x11::EMERGENCY_F12_X11_KEYCODE`), not from
//!   `AgentAttach.emergency_hotkey`; the X11 spike never took the combination as a
//!   parameter. The keycode is checked against the current XKB map by that crate.
//! * As on Windows, nothing buffers a `TEMPORARY` capture for a local replay.

use std::time::Duration;

use input_model::{CaptureKind, HidKey, InputEvent, MouseButton};
use platform_x11::capture::{CapturedEvent, X11Capture};
use platform_x11::inject::X11Injector;

use crate::backend::{ClipboardChange, Event, Injected, Ready, Released, TakeoverPlan, POLL_TICK};
use crate::takeover::Detector;
use input_model::RepeatFilter;

/// The X11 input path.
pub struct X11 {
    ready: Ready,
    capture: Option<X11Capture>,
    injector: Option<X11Injector>,
    /// The receiver-takeover detector, when `[input].takeover_detection` armed it.
    ///
    /// A second, observe-only `X11Capture` (no grab): XI2 raw events reach us while this
    /// node injects, and the ones whose `deviceid`/`sourceid` is an XTEST slave are dropped
    /// by `platform_x11::capture::is_own_injection` before the detector sees them — which is
    /// what makes «physical input» here mean the user's own keyboard and mouse.
    detector: Option<(Detector, X11Capture)>,
    plan: Option<TakeoverPlan>,
    /// Source-side autorepeat filter. X11's `raw KeyPress` carries `XIKeyRepeat`, but
    /// `platform_x11::capture` does not expose it, so the pressed-set rule decides here too.
    repeats: RepeatFilter,
    /// The local clipboard: this backend's own X connection to the CLIPBOARD selection.
    /// `None` when it could not be established, in which case this backend has no clipboard
    /// and says so.
    ///
    /// # Echo suppression (KVM-041)
    ///
    /// A selection owner has no sequence number to compare, so the mechanism is the text
    /// itself: the payload this backend last put on CLIPBOARD is remembered in `applied_text`,
    /// and an observed value equal to it is the echo of our own `set` rather than a user's
    /// copy. (X11 cannot do better without owning a second selection: the `SelectionNotify`
    /// for our own ownership carries no identity beyond the content.)
    clipboard: Option<platform_x11::clipboard::Clipboard>,
    /// The text this backend last applied from core.
    applied_text: Option<String>,
}

impl X11 {
    /// Builds the X11 backend. The capture and injection connections are opened lazily, so a
    /// source that never injects and a receiver that never captures each pay for one; the
    /// clipboard's connection is opened here because the capability reported in `AgentReady`
    /// has to be the truth about what this session will do (ADR-008).
    ///
    /// `emergency_hotkey` is only checked, not honoured: the X11 spike recognises a
    /// fixed F12 keycode (`platform_x11::EMERGENCY_F12_X11_KEYCODE`) and never took
    /// the combination as a parameter, so a configuration that names another key is
    /// reported instead of being pretended to work.
    pub fn new(
        ready: &Ready,
        emergency_hotkey: &str,
        plan: Option<TakeoverPlan>,
    ) -> Result<Self, String> {
        let configured = emergency_hotkey.trim();
        if !configured.is_empty() && !configured.eq_ignore_ascii_case("Ctrl+Alt+Shift+F12") {
            tracing::warn!(
                configured,
                "platform-x11 arms a fixed Ctrl+Alt+Shift+F12 combination; the configured \
                 emergency hotkey is not used by this backend (KVM-013)"
            );
        }
        // Losing the clipboard connection costs the clipboard and nothing else, so it is
        // reported in the capability rather than refused.
        let clipboard = match platform_x11::clipboard::Clipboard::connect() {
            Ok(clipboard) => Some(clipboard),
            Err(error) => {
                tracing::warn!(
                    %error,
                    "no X connection for the CLIPBOARD selection: this session will not do \
                     the clipboard"
                );
                None
            }
        };
        let mut ready = ready.clone();
        ready.set_text_clipboard(clipboard.is_some());
        Ok(X11 {
            ready,
            capture: None,
            injector: None,
            detector: None,
            plan,
            repeats: RepeatFilter::new(),
            clipboard,
            applied_text: None,
        })
    }

    pub fn ready(&self) -> Ready {
        self.ready.clone()
    }

    /// Whether this backend owns a connection to the CLIPBOARD selection.
    pub fn clipboard_available(&self) -> bool {
        self.clipboard.is_some()
    }

    /// The next clipboard change the *user* made, if the selection informed us of one.
    ///
    /// Non-blocking on purpose: the session polls this from its own tick, and its watchdog and
    /// deadline live on that tick too. The X work itself already lives on the selection's own
    /// thread (`platform_x11::clipboard::Clipboard` dispatches commands to it), so the
    /// runtime thread only waits for that thread's reply.
    pub async fn clipboard_change(&mut self) -> Option<ClipboardChange> {
        let clipboard = self.clipboard.as_ref()?;
        clipboard.wait(Duration::ZERO)?;
        let snapshot = match clipboard.read() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                tracing::warn!(%error, "the CLIPBOARD selection could not be read");
                return None;
            }
        };
        if !snapshot.has_text {
            tracing::debug!("the CLIPBOARD selection holds no text");
            return None;
        }
        if self.applied_text.as_deref() == Some(snapshot.text.as_str()) {
            // The text this node itself put there. Cleared after one sighting: a *later* copy
            // of the same text by the user is then reported, which is the safe direction.
            self.applied_text = None;
            tracing::debug!(
                bytes = snapshot.text.len(),
                "swallowing the CLIPBOARD selection this node applied"
            );
            return None;
        }
        tracing::info!(
            bytes = snapshot.text.len(),
            markers = snapshot.markers.len(),
            "local clipboard change"
        );
        Some(ClipboardChange {
            text: snapshot.text,
            markers: snapshot.markers,
        })
    }

    /// Owns the CLIPBOARD selection: `Some` puts `text` on it, `None` gives up the payload.
    ///
    /// The adapter has no "release the selection" call in this build (ICCCM release and
    /// `SelectionClear` handling are not wired), so a release puts the empty string on the
    /// selection: the previous payload stops being pastable, which is what the command is for.
    ///
    /// Async for the session's sake: the X round-trips ride the selection's own thread, and
    /// the runtime thread must stay free for the poll tick and the heartbeat.
    pub async fn set_clipboard(&mut self, text: Option<&str>) -> Result<(), String> {
        let Some(clipboard) = self.clipboard.as_ref() else {
            return Err("CLIPBOARD_DISABLED: this session has no CLIPBOARD selection".to_owned());
        };
        let text = text.unwrap_or("");
        clipboard
            .set(text)
            .map_err(|error| format!("CLIPBOARD_APPLY_FAILED: CLIPBOARD was not taken: {error}"))?;
        self.applied_text = if text.is_empty() {
            None
        } else {
            Some(text.to_owned())
        };
        tracing::info!(
            bytes = text.len(),
            released = text.is_empty(),
            "CLIPBOARD selection set"
        );
        Ok(())
    }

    pub fn begin_capture(&mut self, kind: CaptureKind) -> Result<(), String> {
        if self.capture.is_none() {
            let capture = X11Capture::new(kind).map_err(|error| error.to_string())?;
            self.capture = Some(capture);
        }
        let capture = self.capture.as_mut().expect("just inserted");
        if kind == CaptureKind::Observe {
            capture.ungrab();
            return Ok(());
        }
        capture.grab().map_err(|error| error.to_string())
    }

    pub fn end_capture(&mut self) -> Result<Released, String> {
        if let Some(capture) = &mut self.capture {
            capture.ungrab();
        }
        self.capture = None;
        self.repeats.clear();
        self.disarm_detector();
        let released = match &mut self.injector {
            Some(injector) => release_counts(injector),
            None => Released::default(),
        };
        Ok(released)
    }

    /// Prepares injection — and arms the receiver-takeover detector (KVM-035).
    pub fn prepare_inject(&mut self) -> Result<(), String> {
        self.injector_mut().map(|_| ())?;
        if let Some(plan) = self.plan {
            if self.detector.is_none() {
                match X11Capture::new(CaptureKind::Observe) {
                    Ok(capture) => {
                        tracing::info!(
                            threshold_px = plan.threshold_px,
                            "receiver takeover detector armed (observe-only XI2 capture)"
                        );
                        self.detector = Some((Detector::new(plan.threshold_px), capture));
                    }
                    Err(error) => {
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

    /// Stops watching for physical input: the lease that made it meaningful is over.
    fn disarm_detector(&mut self) {
        if let Some((detector, capture)) = self.detector.take() {
            tracing::info!(
                triggers = detector.triggers(),
                filtered_injected = capture.injected_events_filtered(),
                "receiver takeover detector disarmed"
            );
        }
    }

    pub fn inject(&mut self, events: &[InputEvent]) -> Injected {
        match self.injector_mut() {
            Ok(injector) => {
                let result = injector.inject(events);
                Injected {
                    applied: result.applied_count,
                    os_error: result.os_error.clone().unwrap_or_default(),
                    actual: result.actual,
                }
            }
            Err(error) => Injected {
                applied: 0,
                os_error: error,
                actual: None,
            },
        }
    }

    pub fn release_session(&mut self) -> Released {
        // `ReleaseSession` ends the receiver side of a lease as surely as `EndCapture`.
        self.disarm_detector();
        match &mut self.injector {
            Some(injector) => release_counts(injector),
            None => Released::default(),
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
        if let Ok(injector) = self.injector_mut() {
            let _ = injector.inject(&events);
        }
        Released {
            keys: keys.len() as u32,
            buttons: buttons.len() as u32,
        }
    }

    /// X11 monitors through RandR 1.5 (KVM-040).
    ///
    /// A failure is reported as an empty set: core then falls back to the
    /// `[[topology.displays]]` snapshot instead of losing the node.
    ///
    /// The enumeration is a burst of X round-trips on the calling thread, so it runs on a
    /// blocking thread: the session answers `QueryDisplays` on the same task the heartbeat
    /// `Pong` rides.
    pub async fn displays(&mut self) -> Vec<crate::displays::RawDisplay> {
        let enumerated = match tokio::task::spawn_blocking(platform_x11::displays::enumerate).await
        {
            Ok(Ok(monitors)) => monitors,
            Ok(Err(error)) => {
                tracing::warn!(%error, "cannot enumerate X11 monitors");
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
                // X11's id (an output name such as `eDP-1`) is already its own stable name;
                // the alias field exists for Windows renumbering and is not used here.
                name: String::new(),
                logical: monitor.logical,
                native: Some(monitor.native),
                scale: monitor.scale,
                primary: monitor.primary,
            })
            .collect()
    }

    pub async fn next_event(&mut self) -> Option<Event> {
        if let Some((detector, capture)) = &mut self.detector {
            match capture.next_event(Some(Duration::ZERO)) {
                Ok(Some(CapturedEvent::Input(event))) => {
                    if let Some(trigger) = detector.observe(&event) {
                        let distance_px = match trigger {
                            crate::takeover::Trigger::Motion { distance_px } => Some(distance_px),
                            _ => None,
                        };
                        tracing::warn!(
                            reason = trigger.reason(),
                            distance_px = ?distance_px,
                            "physical input on the receiver; ending the lease"
                        );
                        return Some(Event::LocalTakeover {
                            reason: trigger.reason(),
                            distance_px,
                        });
                    }
                }
                // Rule 4: the emergency combination works on a receiver too.
                Ok(Some(CapturedEvent::Emergency)) => return Some(Event::Emergency),
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(%error, "the receiver takeover detector stopped; disarming");
                    self.detector = None;
                }
            }
        }
        if let Some(capture) = &mut self.capture {
            // A non-blocking poll; the loop's tick provides the wait. The drain skips the OS
            // autorepeats without ending the call (batch-3 audit F13): returning as soon as one
            // was seen made the session's `select!` call this again immediately, so a held key
            // — which the X server repeats on its own schedule — spun the poll loop at full CPU
            // instead of its tick.
            match drain_capture(
                || capture.next_event(Some(Duration::ZERO)),
                &mut self.repeats,
            ) {
                Ok(Some(event)) => return Some(event),
                Ok(None) => {}
                Err(error) => {
                    return Some(Event::Failed {
                        code: "CAPTURE_FAILED",
                        detail: error.to_string(),
                    })
                }
            }
        }
        tokio::time::sleep(POLL_TICK).await;
        None
    }

    fn injector_mut(&mut self) -> Result<&mut X11Injector, String> {
        if self.injector.is_none() {
            let injector = X11Injector::new().map_err(|error| error.to_string())?;
            self.injector = Some(injector);
        }
        Ok(self.injector.as_mut().expect("just inserted"))
    }
}

/// The next captured event, skipping the OS autorepeats (batch-3 audit, F13).
///
/// `next` is the non-blocking XI2 read, which reports an error of its own. A repeat does **not**
/// end the call: the session's `select!` calls `next_event` again the moment it returns, so
/// returning on a repeat spun the poll loop for as long as a key was held. The drain runs until
/// it has a real event or the queue is empty — then the caller reaches its tick's sleep.
///
/// `platform_says_repeat` is false here: `platform_x11::capture` does not expose XI2's
/// `XIKeyRepeat` yet, so the pressed set in [`RepeatFilter`] is the detection rule.
fn drain_capture(
    mut next: impl FnMut() -> Result<Option<CapturedEvent>, platform_x11::X11Error>,
    repeats: &mut RepeatFilter,
) -> Result<Option<Event>, platform_x11::X11Error> {
    loop {
        let Some(event) = next()? else {
            return Ok(None);
        };
        match event {
            CapturedEvent::Input(event) => {
                if repeats.is_repeat(&event, false) {
                    tracing::trace!(?event, "dropping an OS autorepeat");
                    continue;
                }
                return Ok(Some(Event::Input(event)));
            }
            CapturedEvent::Emergency => return Ok(Some(Event::Emergency)),
        }
    }
}

/// Releases everything the injector still holds, counting what actually went up.
fn release_counts(injector: &mut X11Injector) -> Released {
    let keys_before = injector.pressed_keys().len();
    let buttons_before = injector.pressed_buttons().len();
    let _ = injector.release_all();
    Released {
        keys: keys_before.saturating_sub(injector.pressed_keys().len()) as u32,
        buttons: buttons_before.saturating_sub(injector.pressed_buttons().len()) as u32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Batch-3 audit (F13): a queued run of X-server autorepeats must not end the drain.
    ///
    /// Every repeat used to be one `next_event` call, and the session's `select!` called it
    /// again at once, so a held key spun the poll loop. One call now consumes the whole run and
    /// returns the first event that is not a repeat.
    #[test]
    fn a_run_of_x_server_repeats_is_drained_in_one_call() {
        let down = || {
            CapturedEvent::Input(InputEvent::Key {
                key: HidKey::keyboard(0x04),
                state: input_model::KeyState::Down,
            })
        };
        let up = CapturedEvent::Input(InputEvent::Key {
            key: HidKey::keyboard(0x04),
            state: input_model::KeyState::Up,
        });
        let mut repeats = RepeatFilter::new();
        // The press itself is a real event, and it is what puts the key in the pressed set.
        let mut press: std::collections::VecDeque<CapturedEvent> = vec![down()].into();
        assert!(matches!(
            drain_capture(|| Ok(press.pop_front()), &mut repeats),
            Ok(Some(Event::Input(_)))
        ));
        // Then the server repeats the key, and the up that ends the hold.
        let mut queue: std::collections::VecDeque<CapturedEvent> = vec![down(), down(), up].into();
        let mut reads = 0usize;
        let drained = drain_capture(
            || {
                reads += 1;
                Ok(queue.pop_front())
            },
            &mut repeats,
        );
        assert_eq!(
            reads, 3,
            "one call drained the two repeats and the key-up that followed them"
        );
        assert!(
            matches!(
                drained,
                Ok(Some(Event::Input(InputEvent::Key {
                    state: input_model::KeyState::Up,
                    ..
                })))
            ),
            "the first non-repeat event is the one that is returned"
        );
        assert!(queue.is_empty(), "nothing is left behind for the next call");

        // A queue of nothing but repeats drains to `None` in one call, so the caller reaches its
        // sleep instead of spinning on the same repeats.
        let mut pressed = RepeatFilter::new();
        let mut seed: std::collections::VecDeque<CapturedEvent> = vec![down()].into();
        assert!(matches!(
            drain_capture(|| Ok(seed.pop_front()), &mut pressed),
            Ok(Some(Event::Input(_)))
        ));
        let mut only_repeats: std::collections::VecDeque<CapturedEvent> =
            vec![down(), down()].into();
        assert!(
            matches!(
                drain_capture(|| Ok(only_repeats.pop_front()), &mut pressed),
                Ok(None)
            ),
            "repeats alone yield no event"
        );
        assert!(
            only_repeats.is_empty(),
            "and they are consumed, not re-read"
        );
    }
}
