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

use crate::backend::{Event, Injected, Ready, Released, TakeoverPlan, POLL_TICK};
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
}

impl X11 {
    /// Builds the X11 backend. Both X connections are opened lazily, so a source
    /// that never injects and a receiver that never captures each pay for one.
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
        Ok(X11 {
            ready: ready.clone(),
            capture: None,
            injector: None,
            detector: None,
            plan,
            repeats: RepeatFilter::new(),
        })
    }

    pub fn ready(&self) -> Ready {
        self.ready.clone()
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
    pub fn displays(&mut self) -> Vec<crate::displays::RawDisplay> {
        match platform_x11::displays::enumerate() {
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
                tracing::warn!(%error, "cannot enumerate X11 monitors");
                Vec::new()
            }
        }
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
            // A non-blocking poll; the loop's tick provides the wait.
            match capture.next_event(Some(Duration::ZERO)) {
                Ok(Some(CapturedEvent::Input(event))) => {
                    if self.repeats.is_repeat(&event, false) {
                        tracing::trace!(?event, "dropping an OS autorepeat");
                        return None;
                    }
                    return Some(Event::Input(event));
                }
                Ok(Some(CapturedEvent::Emergency)) => return Some(Event::Emergency),
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
