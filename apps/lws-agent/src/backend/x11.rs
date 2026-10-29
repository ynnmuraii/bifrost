//! `platform-x11` backend: XInput2 capture with an explicit `XGrabPointer`/
//! `XGrabKeyboard` and XTest injection (spikes KVM-012/013).
//!
//! The grab is what makes the transition atomic: `middle of a session without it
//! nothing suppresses local input, so `BeginCapture` of a suppressing kind fails
//! instead of "working" without suppression.
//!
//! Known gaps of this slice, stated rather than hidden:
//!
//! * Global actions use lifetime passive key grabs, while remote capture retains its
//!   active keyboard/pointer grabs. The fixed emergency chord remains an additional safety path.
//! * Temporary capture leaves the X11 grabs released. Discrete local events
//!   are not forwarded or buffered for replay.

use std::time::Duration;

use input_model::{CaptureKind, HidKey, InputEvent, MouseButton};
use platform_x11::capture::{CapturedEvent, X11Capture};
use platform_x11::inject::X11Injector;

use crate::backend::{ClipboardChange, Event, Injected, Ready, Released, TakeoverPlan, POLL_TICK};
use crate::takeover::Detector;
use input_model::RepeatFilter;

fn clipboard_read_error(
    error: platform_x11::clipboard::ClipboardError,
) -> clipboard::ClipboardError {
    match error {
        platform_x11::clipboard::ClipboardError::Content(error) => error,
        _ => clipboard::ClipboardError::Invalid,
    }
}

/// The X11 input path.
pub struct X11 {
    ready: Ready,
    capture: Option<X11Capture>,
    source_kind: Option<CaptureKind>,
    pending_hotkeys: std::collections::VecDeque<Event>,
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
    /// A selection owner has no sequence number to compare, so the mechanism is the content
    /// itself: the payload this backend last put on CLIPBOARD is remembered in `echo`, and an
    /// observed value equal to it is the echo of our own `set` rather than a user's copy.
    /// (X11 could do better by owning a second selection and watching its timestamp; the
    /// adapter here exposes neither the selection timestamp nor the owner window — its
    /// [`platform_x11::clipboard::Change`] is an empty marker — so the content is what is left.
    /// Keeping its hash for a short window, instead of forgetting it after the first sighting,
    /// is what survives a clipboard manager re-owning the selection several times.)
    clipboard: Option<platform_x11::clipboard::Clipboard>,
    pending_clipboard_read: Option<platform_x11::clipboard::PendingRead>,
    /// The payload this backend last applied from core, for [`EchoSuppressor`].
    echo: EchoSuppressor,
}

/// How long an applied payload is treated as our own echo.
///
/// Long enough for the several re-owns a clipboard manager makes while it imports the item
/// (Klipper, CopyQ), short enough that a person pressing «copy» on the same text again is
/// reported. The clock is [`std::time::Instant`] and the window is passed in
/// ([`EchoSuppressor::is_echo`]), so the logic is tested without sleeping.
const ECHO_WINDOW: Duration = Duration::from_millis(400);

/// Remembers the payload this node put on CLIPBOARD so its own notifications are swallowed.
///
/// The old rule cleared the memory after the first sighting: a second notification with the
/// same content (Klipper re-reading the selection) then looked like a fresh local copy and was
/// sent back to the peer. The hash now stays for [`ECHO_WINDOW`]: repeats inside the window
/// are echoes, different content clears the memory, and the same content after the window
/// is a real re-copy by the user and passes.
#[derive(Debug, Default)]
struct EchoSuppressor {
    applied: Option<(u64, std::time::Instant)>,
}

impl EchoSuppressor {
    /// Remembers `text` — the payload just put on the selection — at `now`.
    #[cfg(test)]
    fn note(&mut self, text: &str, now: std::time::Instant) {
        self.applied = if text.is_empty() {
            None
        } else {
            Some((
                platform_x11::clipboard::content_hash(&clipboard::ClipboardContent::Text(
                    text.to_owned(),
                )),
                now,
            ))
        };
    }

    /// Whether the actor-computed fingerprint matches a recently applied payload.
    fn is_echo(&mut self, hash: u64, now: std::time::Instant) -> bool {
        let Some((applied, at)) = self.applied else {
            return false;
        };
        if applied != hash {
            // Something else is on the selection now: our payload is gone for good.
            self.applied = None;
            return false;
        }
        if now.saturating_duration_since(at) <= ECHO_WINDOW {
            true
        } else {
            // The window is over: the same text now is the user copying it again.
            self.applied = None;
            false
        }
    }
}

impl X11 {
    /// Opens a lifetime observe connection for passive shortcuts; injection stays lazy.
    /// Clipboard failure degrades only clipboard availability.
    pub fn new(
        ready: &Ready,
        emergency_hotkey: &str,
        plan: Option<TakeoverPlan>,
    ) -> Result<Self, String> {
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
        ready.set_image_clipboard(clipboard.is_some());
        let mut backend = X11 {
            ready,
            source_kind: None,
            pending_hotkeys: std::collections::VecDeque::new(),
            capture: Some({
                let mut capture =
                    X11Capture::new(CaptureKind::Observe).map_err(|error| error.to_string())?;
                capture
                    .set_hotkeys([None; 5])
                    .map_err(|error| error.to_string())?;
                capture
            }),
            injector: None,
            detector: None,
            plan,
            repeats: RepeatFilter::new(),
            clipboard,
            pending_clipboard_read: None,
            echo: EchoSuppressor::default(),
        };
        backend.set_hotkeys(&config::HotkeyBindings {
            emergency_release: if emergency_hotkey.trim().is_empty() {
                config::HotkeyBindings::default().emergency_release
            } else {
                emergency_hotkey.to_owned()
            },
            switch_next: String::new(),
            return_local: String::new(),
            claim_ownership: String::new(),
            game_guard_toggle: String::new(),
        })?;
        Ok(backend)
    }

    pub fn ready(&self) -> Ready {
        self.ready.clone()
    }

    pub fn set_hotkeys(&mut self, bindings: &config::HotkeyBindings) -> Result<(), String> {
        let capture = self
            .capture
            .as_mut()
            .ok_or("X11 hotkey connection unavailable")?;
        let mut compiled = [None; 5];
        for (index, (_, chord)) in bindings.iter().enumerate() {
            if chord.is_empty() {
                continue;
            }
            let chord = config::Hotkey::parse(chord).map_err(|error| error.to_string())?;
            let keysym = platform_x11::hotkeys::keysym(chord.key())
                .ok_or_else(|| format!("unsupported X11 hotkey key {}", chord.key()))?;
            let keycode = capture
                .hotkey_keycode(keysym)
                .map_err(|error| error.to_string())?;
            let modifiers = chord.modifiers().iter().fold(0, |mask, modifier| {
                mask | match modifier {
                    config::Modifier::Shift => 1,
                    config::Modifier::Ctrl => 4,
                    config::Modifier::Alt => 8,
                    config::Modifier::Super => 64,
                }
            });
            compiled[index] = Some(platform_x11::hotkeys::Hotkey { keycode, modifiers });
        }
        capture
            .set_hotkeys(compiled)
            .map_err(|error| error.to_string())
    }

    /// Whether this backend owns a connection to the CLIPBOARD selection.
    pub fn clipboard_available(&self) -> bool {
        self.clipboard.is_some()
    }

    /// The next clipboard change the *user* made, if the selection informed us of one.
    ///
    /// Non-blocking: selection reads stay on the existing clipboard actor and are polled
    /// across ticks, so a stalled owner cannot hold up input or the heartbeat.
    pub async fn clipboard_change(
        &mut self,
    ) -> Result<Option<ClipboardChange>, clipboard::ClipboardError> {
        let Some(clipboard) = self.clipboard.as_ref() else {
            return Ok(None);
        };
        if self.pending_clipboard_read.is_none() {
            if clipboard.wait(Duration::ZERO).is_none() {
                return Ok(None);
            }
            self.pending_clipboard_read =
                Some(clipboard.start_read().map_err(clipboard_read_error)?);
        }
        let pending = self.pending_clipboard_read.as_ref().expect("read started");
        let result = pending.poll();
        if matches!(result, Ok(None)) {
            return Ok(None);
        }
        self.pending_clipboard_read = None;
        let snapshot = result
            .map_err(clipboard_read_error)?
            .expect("read completed");
        let Some(content) = snapshot.content else {
            return Ok(None);
        };
        if snapshot.markers.is_empty()
            && snapshot
                .hash
                .is_some_and(|hash| self.echo.is_echo(hash, std::time::Instant::now()))
        {
            tracing::debug!(
                bytes = content.len(),
                "swallowing the CLIPBOARD selection this node applied"
            );
            return Ok(None);
        }
        tracing::info!(
            bytes = content.len(),
            markers = snapshot.markers.len(),
            "local clipboard change"
        );
        Ok(Some(ClipboardChange {
            content,
            markers: snapshot.markers,
        }))
    }

    /// Owns the CLIPBOARD selection; `None` clears the payload with empty text.
    ///
    /// The adapter has no "release the selection" call in this build (ICCCM release and
    /// `SelectionClear` handling are not wired), so a release puts the empty string on the
    /// selection: the previous payload stops being pastable, which is what the command is for.
    ///
    /// Async for the session's sake: the X round-trips ride the selection's own thread, and
    /// the runtime thread must stay free for the poll tick and the heartbeat.
    pub fn begin_set_clipboard(
        &mut self,
        content: Option<clipboard::ClipboardContent>,
    ) -> Result<super::ClipboardApplyJob, String> {
        self.pending_clipboard_read = None;
        let clipboard = self.clipboard.as_ref().ok_or_else(|| {
            "CLIPBOARD_DISABLED: this session has no CLIPBOARD selection".to_owned()
        })?;
        let setter = clipboard.owned_setter();
        Ok(Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                let content =
                    content.unwrap_or_else(|| clipboard::ClipboardContent::Text(String::new()));
                let hash =
                    (!content.is_empty()).then(|| platform_x11::clipboard::content_hash(&content));
                setter(content).map_err(|error| format!("CLIPBOARD_APPLY_FAILED: {error}"))?;
                Ok(super::ClipboardApplyReceipt::X11(hash))
            })
            .await
            .map_err(|_| "CLIPBOARD_APPLY_FAILED: clipboard worker failed".to_owned())?
        }))
    }

    pub fn complete_clipboard_apply(&mut self, hash: Option<u64>) {
        self.echo.applied = hash.map(|hash| (hash, std::time::Instant::now()));
    }

    pub fn begin_capture(&mut self, kind: CaptureKind) -> Result<(), String> {
        self.source_kind = Some(kind);
        if self.capture.is_none() {
            let capture = X11Capture::new(kind).map_err(|error| error.to_string())?;
            self.capture = Some(capture);
        }
        let capture = self.capture.as_mut().expect("just inserted");
        if !kind.is_suppressed() {
            capture.ungrab();
            return Ok(());
        }
        // Discard observation events already delivered locally before committing
        // the grab; otherwise a queued local click could be duplicated remotely.
        if !capture.grab_active() {
            while let Some(event) = capture
                .next_event(Some(Duration::ZERO))
                .map_err(|error| error.to_string())?
            {
                if matches!(event, CapturedEvent::Emergency) {
                    return Err("EMERGENCY: capture commit cancelled by local hotkey".to_owned());
                }
                if let CapturedEvent::HotkeyPressed(action) = event {
                    self.pending_hotkeys.push_back(hotkey_event(action));
                }
            }
            self.repeats.clear();
        }
        capture.grab().map_err(|error| error.to_string())
    }

    pub fn end_capture(&mut self) -> Result<Released, String> {
        self.source_kind = None;
        if let Some(capture) = &mut self.capture {
            capture.ungrab();
        }
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
        if let Some(event) = self.pending_hotkeys.pop_front() {
            return Some(event);
        }
        if let Some(capture) = &mut self.capture {
            // A non-blocking poll; the loop's tick provides the wait. The drain skips the OS
            // autorepeats without ending the call (batch-3 audit F13): returning as soon as one
            // was seen made the session's `select!` call this again immediately, so a held key
            // — which the X server repeats on its own schedule — spun the poll loop at full CPU
            // instead of its tick.
            let grab_active = capture.grab_active();
            match drain_capture(
                || {
                    next_source_capture(self.source_kind, grab_active, || {
                        capture.next_event(Some(Duration::ZERO))
                    })
                },
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
                Ok(Some(CapturedEvent::HotkeyPressed(action))) => {
                    return Some(hotkey_event(action))
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(%error, "the receiver takeover detector stopped; disarming");
                    self.detector = None;
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

/// Armed Observe reports existing raw edges without suppressing or replaying them.
/// Temporary edges already reached the desktop and must never become deferred peer input.
fn next_source_capture(
    kind: Option<CaptureKind>,
    grab_active: bool,
    mut next: impl FnMut() -> Result<Option<CapturedEvent>, platform_x11::X11Error>,
) -> Result<Option<CapturedEvent>, platform_x11::X11Error> {
    loop {
        let Some(event) = next()? else {
            return Ok(None);
        };
        if let CapturedEvent::Input(input) = event {
            if kind.is_none() {
                continue;
            }
            if !grab_active
                && kind != Some(CaptureKind::Observe)
                && matches!(
                    input,
                    InputEvent::Key { .. } | InputEvent::Button { .. } | InputEvent::Scroll { .. }
                )
            {
                continue;
            }
        }
        return Ok(Some(event));
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
            CapturedEvent::HotkeyPressed(action) => return Ok(Some(hotkey_event(action))),
        }
    }
}

fn hotkey_event(action: u8) -> Event {
    Event::HotkeyPressed(match action {
        1 => config::HotkeyAction::SwitchNext,
        2 => config::HotkeyAction::ReturnLocal,
        3 => config::HotkeyAction::ClaimOwnership,
        4 => config::HotkeyAction::GameGuardToggle,
        _ => config::HotkeyAction::EmergencyRelease,
    })
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

    #[test]
    fn clipboard_read_failures_reach_the_shared_error_boundary() {
        use platform_x11::clipboard::ClipboardError as Native;
        for error in [
            clipboard::ClipboardError::TooLarge,
            clipboard::ClipboardError::Invalid,
        ] {
            assert_eq!(clipboard_read_error(Native::Content(error)), error);
        }
        for error in [Native::Timeout, Native::Internal("unreadable".into())] {
            assert_eq!(
                clipboard_read_error(error),
                clipboard::ClipboardError::Invalid
            );
        }
    }

    fn text_hash(text: &str) -> u64 {
        platform_x11::clipboard::content_hash(&clipboard::ClipboardContent::Text(text.to_owned()))
    }

    #[test]
    fn image_echo_uses_the_worker_fingerprint_without_reading_payload_in_the_loop() {
        let content = clipboard::ClipboardContent::Image {
            png: b"\x89PNG\r\n\x1a\nraw".to_vec(),
        };
        let hash = platform_x11::clipboard::content_hash(&content);
        let now = std::time::Instant::now();
        let mut echo = EchoSuppressor {
            applied: Some((hash, now)),
        };
        assert!(echo.is_echo(hash, now));
        assert!(!echo.is_echo(hash, now + ECHO_WINDOW + Duration::from_nanos(1)));
        assert_ne!(hash, text_hash("raw"));
    }

    #[tokio::test]
    async fn stalled_clipboard_owner_does_not_block_backend_poll_or_later_apply() {
        if std::env::var_os("LWS_X11_TEST_ISOLATED").is_none() {
            eprintln!("requires LWS_X11_TEST_ISOLATED=1 under Xvfb");
            return;
        }
        use x11rb::connection::Connection as _;
        use x11rb::protocol::xproto::{ConnectionExt as _, CreateWindowAux, WindowClass};
        let clipboard = platform_x11::clipboard::Clipboard::connect().unwrap();
        let (owner, screen) = x11rb::connect(None).unwrap();
        let window = owner.generate_id().unwrap();
        owner
            .create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                window,
                owner.setup().roots[screen].root,
                0,
                0,
                1,
                1,
                0,
                WindowClass::INPUT_OUTPUT,
                x11rb::COPY_FROM_PARENT,
                &CreateWindowAux::new(),
            )
            .unwrap()
            .check()
            .unwrap();
        let selection = owner
            .intern_atom(false, b"CLIPBOARD")
            .unwrap()
            .reply()
            .unwrap()
            .atom;
        owner
            .set_selection_owner(window, selection, x11rb::CURRENT_TIME)
            .unwrap()
            .check()
            .unwrap();
        assert!(clipboard.wait(Duration::from_secs(2)).is_some());
        let mut backend = X11 {
            ready: Ready::degraded("fixture", "platform-x11", "x11"),
            capture: None,
            source_kind: None,
            pending_hotkeys: Default::default(),
            injector: None,
            detector: None,
            plan: None,
            repeats: RepeatFilter::new(),
            clipboard: Some(clipboard),
            pending_clipboard_read: None,
            echo: EchoSuppressor::default(),
        };
        // Queue another owner notification for the actual backend poll.
        owner
            .set_selection_owner(window, selection, x11rb::CURRENT_TIME)
            .unwrap()
            .check()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut elapsed = Duration::ZERO;
        loop {
            let started = std::time::Instant::now();
            assert!(backend.clipboard_change().await.unwrap().is_none());
            elapsed = elapsed.max(started.elapsed());
            if matches!(
                owner.poll_for_event().unwrap(),
                Some(x11rb::protocol::Event::SelectionRequest(_))
            ) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "owner never received a read request"
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        // The owner has received the request and deliberately never answers it.
        let started = std::time::Instant::now();
        assert!(backend.clipboard_change().await.unwrap().is_none());
        let second_elapsed = started.elapsed();
        let apply_started = std::time::Instant::now();
        let receipt = backend
            .begin_set_clipboard(Some(clipboard::ClipboardContent::Text(
                "worker remains usable".to_owned(),
            )))
            .unwrap()
            .await
            .unwrap();
        let super::super::ClipboardApplyReceipt::X11(hash) = receipt else {
            panic!("X11 receipt expected");
        };
        backend.complete_clipboard_apply(hash);
        let apply_elapsed = apply_started.elapsed();
        assert_eq!(
            backend.clipboard.as_ref().unwrap().read().unwrap().content,
            Some(clipboard::ClipboardContent::Text(
                "worker remains usable".to_owned()
            ))
        );
        assert!(
            elapsed < Duration::from_millis(50),
            "first poll took {elapsed:?}"
        );
        assert!(
            second_elapsed < Duration::from_millis(50),
            "pending poll took {second_elapsed:?}"
        );
        assert!(
            apply_elapsed < Duration::from_millis(100),
            "cancelling read and applying took {apply_elapsed:?}"
        );
    }

    #[test]
    fn armed_observe_reports_raw_key_and_button_edges_without_a_grab() {
        let edges = [
            InputEvent::Key {
                key: HidKey::keyboard(0x04),
                state: input_model::KeyState::Down,
            },
            InputEvent::Key {
                key: HidKey::keyboard(0x04),
                state: input_model::KeyState::Up,
            },
            InputEvent::Button {
                button: MouseButton::Left,
                state: input_model::KeyState::Down,
            },
            InputEvent::Button {
                button: MouseButton::Left,
                state: input_model::KeyState::Up,
            },
        ];
        let mut queue = std::collections::VecDeque::from(edges.map(CapturedEvent::Input));
        let mut repeats = RepeatFilter::new();
        for expected in edges {
            let actual = drain_capture(
                || next_source_capture(Some(CaptureKind::Observe), false, || Ok(queue.pop_front())),
                &mut repeats,
            )
            .expect("raw observation");
            assert_eq!(actual, Some(Event::Input(expected)));
        }
        assert!(queue.is_empty());
    }

    #[test]
    fn unarmed_and_temporary_raw_edges_cannot_become_peer_input() {
        for kind in [
            None,
            Some(CaptureKind::Temporary),
            Some(CaptureKind::Remote),
        ] {
            let mut queue = std::collections::VecDeque::from([
                CapturedEvent::Input(InputEvent::Key {
                    key: HidKey::keyboard(0x04),
                    state: input_model::KeyState::Down,
                }),
                CapturedEvent::Input(InputEvent::Button {
                    button: MouseButton::Left,
                    state: input_model::KeyState::Down,
                }),
                CapturedEvent::Input(InputEvent::Scroll { h120: 0, v120: 120 }),
                CapturedEvent::Emergency,
            ]);
            assert_eq!(
                next_source_capture(kind, false, || Ok(queue.pop_front())).unwrap(),
                Some(CapturedEvent::Emergency),
            );
            assert!(queue.is_empty());
        }
        let expected = CapturedEvent::Input(InputEvent::Key {
            key: HidKey::keyboard(0x04),
            state: input_model::KeyState::Down,
        });
        let mut queue = std::collections::VecDeque::from([expected]);
        assert_eq!(
            next_source_capture(Some(CaptureKind::Remote), true, || Ok(queue.pop_front())).unwrap(),
            Some(expected),
        );
    }

    /// L3: the applied payload's hash, not a one-shot string, survives a clipboard manager
    /// re-owning CLIPBOARD several times.
    #[test]
    fn repeated_notifications_of_the_applied_text_are_echoes_within_the_window() {
        let mut echo = EchoSuppressor::default();
        let t0 = std::time::Instant::now();
        echo.note("remote payload", t0);

        // Klipper/CopyQ re-reads and re-owns the selection: three notifications, same text.
        for offset_ms in [0, 5, 120, 399] {
            assert!(
                echo.is_echo(
                    text_hash("remote payload"),
                    t0 + Duration::from_millis(offset_ms)
                ),
                "a re-own at {offset_ms} ms is our own payload"
            );
        }
        // Still the same payload: the memory is not cleared by the repeats.
        assert!(echo.is_echo(text_hash("remote payload"), t0 + Duration::from_millis(400)));
    }

    #[test]
    fn a_different_text_is_not_an_echo_and_forgets_the_applied_one() {
        let mut echo = EchoSuppressor::default();
        let t0 = std::time::Instant::now();
        echo.note("remote payload", t0);

        assert!(!echo.is_echo(text_hash("something the user copied"), t0));
        // Our payload can no longer be on the selection, so an old repeat is a user's copy.
        assert!(!echo.is_echo(text_hash("remote payload"), t0 + Duration::from_millis(10)));
    }

    #[test]
    fn the_same_text_after_the_window_is_a_real_copy() {
        let mut echo = EchoSuppressor::default();
        let t0 = std::time::Instant::now();
        echo.note("remote payload", t0);
        assert!(!echo.is_echo(
            text_hash("remote payload"),
            t0 + ECHO_WINDOW + Duration::from_millis(1)
        ));
    }

    #[test]
    fn nothing_is_an_echo_before_anything_was_applied_and_empty_text_clears() {
        let mut echo = EchoSuppressor::default();
        let t0 = std::time::Instant::now();
        assert!(!echo.is_echo(text_hash("anything"), t0));

        echo.note("remote payload", t0);
        echo.note("", t0);
        assert!(!echo.is_echo(text_hash("remote payload"), t0));
    }

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
