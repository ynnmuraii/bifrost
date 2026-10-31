//! Audio-owned GlobalShortcuts PTT. No input-agent dependency or synthetic key events.
//!
//! Supported portals must supply nonzero, stable elapsed-millisecond timestamps
//! for the live session. The portal specification leaves their base undefined;
//! ashpd's epoch wording is not a shared-clock guarantee. We anchor the first
//! timestamp before CreateSession, never at receipt, and retain that conservative
//! lag for the entire session. Zero timestamps (including current KDE), clock
//! regressions and unmappable jumps fail closed; not every portal supports PTT.
//! Both edges must have a conservative mapped age no greater than the 250 ms
//! PTT lease at admission. Consent/initial-signal lag beyond that closes the
//! session as freshness-unverifiable; there is no receipt-time recalibration or
//! later catch-up. Use the focused GUI PTT control when global PTT is unavailable.
use std::collections::VecDeque;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use ashpd::desktop::global_shortcuts::{BindShortcutsOptions, GlobalShortcuts, NewShortcut};
use ashpd::desktop::CreateSessionOptions;
use audio_backend::{BackendError, Event, PttHotkey, PttReleaseReason, PTT_LEASE_NS};
use futures::channel::mpsc;
use futures::{select_biased, FutureExt, StreamExt};

const QUEUE_CAPACITY: usize = 32;
const RENEW_NS: u64 = 100_000_000;
const IO_TIMEOUT: Duration = Duration::from_secs(2);
const SHORTCUT_ID: &str = "audio-ptt";

fn monotonic_ns() -> u64 {
    // Safe CLOCK_MONOTONIC wrapper preserves this crate's forbid(unsafe_code).
    let time = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    (time.tv_sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(time.tv_nsec as u64)
}

/// The compositor is trusted for stable elapsed milliseconds, not a shared epoch.
/// See https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.GlobalShortcuts.html
struct PortalTimeline {
    session_started_ns: u64,
    first_ms: Option<u64>,
    last_ms: u64,
}

impl PortalTimeline {
    fn new(session_started_ns: u64) -> Self {
        Self {
            session_started_ns,
            first_ms: None,
            last_ms: 0,
        }
    }

    fn observe(&mut self, timestamp: Duration, now: u64) -> Option<u64> {
        const MILLIS_NS: u64 = 1_000_000;
        let milliseconds = u64::try_from(timestamp.as_millis()).ok()?;
        if milliseconds == 0 || milliseconds <= self.last_ms || now == 0 {
            return None;
        }
        let first_ms = *self.first_ms.get_or_insert(milliseconds);
        let elapsed_ns = milliseconds.checked_sub(first_ms)?.checked_mul(MILLIS_NS)?;
        // Subtract one tick: the first signal can occur at either end of its
        // quantized millisecond. Session creation is its only trusted lower bound.
        let observed_at_ns = self
            .session_started_ns
            .checked_sub(MILLIS_NS)?
            .checked_add(elapsed_ns)?;
        if observed_at_ns == 0 || observed_at_ns > now || now - observed_at_ns > PTT_LEASE_NS {
            return None;
        }
        self.last_ms = milliseconds;
        Some(observed_at_ns)
    }
}

#[derive(Default)]
struct State {
    generation: u64,
    live_generation: Option<u64>,
    last_edge_ns: Option<u64>,
    held_until: Option<u64>,
    logical_down: bool,
    pending_deactivated: Option<u64>,
    renewed_at: u64,
    events: VecDeque<Event>,
}

impl State {
    fn registered(&mut self, generation: u64) -> bool {
        if self.generation != generation {
            return false;
        }
        self.live_generation = Some(generation);
        self.last_edge_ns = None;
        self.logical_down = false;
        true
    }

    fn portal_edge(
        &mut self,
        generation: u64,
        activated: bool,
        timestamp: Duration,
        now: u64,
        timeline: &mut PortalTimeline,
    ) -> bool {
        if self.live_generation != Some(generation) || self.generation != generation {
            return false;
        }
        let Some(observed_at_ns) = timeline.observe(timestamp, now) else {
            self.release(PttReleaseReason::PortalFreshnessUnavailable);
            self.push(Event::ResyncRequired);
            return false;
        };
        self.edge(generation, activated, observed_at_ns, now);
        true
    }

    fn release(&mut self, reason: PttReleaseReason) {
        if matches!(
            reason,
            PttReleaseReason::UnavailableOrReset | PttReleaseReason::PortalFreshnessUnavailable
        ) {
            self.live_generation = None;
        }
        self.held_until = None;
        self.events
            .retain(|event| !matches!(event, Event::PttPressed { .. } | Event::PttRenewed { .. }));
        if self.events.back() != Some(&Event::PttReleased { reason }) {
            self.push(Event::PttReleased { reason });
        }
    }

    fn push(&mut self, event: Event) {
        if matches!(event, Event::PttRenewed { .. }) {
            self.events
                .retain(|event| !matches!(event, Event::PttRenewed { .. }));
        }
        if self.events.len() >= QUEUE_CAPACITY {
            self.held_until = None;
            // Retain the newest genuine proof with its original timestamp, never
            // convert overflow/reset into a logical deactivation.
            for queued in self.events.iter().chain(std::iter::once(&event)) {
                if let Event::PttReleased {
                    reason: PttReleaseReason::PortalDeactivated { observed_at_ns },
                } = queued
                {
                    self.pending_deactivated =
                        Some(self.pending_deactivated.unwrap_or(0).max(*observed_at_ns));
                }
            }
            self.events.clear();
            self.events.push_back(Event::PttReleased {
                reason: PttReleaseReason::UnavailableOrReset,
            });
            self.events.push_back(Event::ResyncRequired);
            return;
        }
        self.events.push_back(event);
    }

    fn edge(&mut self, generation: u64, activated: bool, observed_at_ns: u64, now: u64) {
        if self.live_generation != Some(generation) || self.generation != generation {
            return;
        }
        if observed_at_ns == 0 || now == 0 || observed_at_ns > now {
            self.release(PttReleaseReason::UnavailableOrReset);
            self.push(Event::ResyncRequired);
            return;
        }
        if !activated && !self.logical_down && self.last_edge_ns.is_some() {
            return;
        }
        // Release wins ties: millisecond timestamps cannot prove that an
        // activation in the same clock tick happened after a deactivation.
        if self.last_edge_ns.is_some_and(|last| {
            observed_at_ns < last || (observed_at_ns == last && (activated || !self.logical_down))
        }) {
            return;
        }
        self.last_edge_ns = Some(observed_at_ns);
        if !activated {
            self.logical_down = false;
            self.release(PttReleaseReason::PortalDeactivated { observed_at_ns });
            return;
        }
        if self.logical_down {
            return;
        }
        self.logical_down = true;
        // Receipt bounds only the lease; original observation remains the child's
        // freshness/fence authority even after a long portal-approval delay.
        let expires_at_ns = now.saturating_add(PTT_LEASE_NS);
        // Expiry/overflow closes a logical hold until a genuine deactivation.
        if self.pending_deactivated.is_some() {
            return;
        }
        self.held_until = Some(expires_at_ns);
        self.renewed_at = now;
        self.push(Event::PttPressed {
            observed_at_ns,
            expires_at_ns,
        });
    }

    fn tick(&mut self, now: u64) {
        let Some(expiry) = self.held_until else {
            return;
        };
        if now == 0 || now >= expiry {
            self.release(if now == 0 {
                PttReleaseReason::PortalFreshnessUnavailable
            } else {
                PttReleaseReason::LeaseExpired
            });
            if now == 0 {
                self.push(Event::ResyncRequired);
            }
        } else if now.saturating_sub(self.renewed_at) >= RENEW_NS {
            let expires_at_ns = now.saturating_add(PTT_LEASE_NS);
            self.held_until = Some(expires_at_ns);
            self.renewed_at = now;
            self.push(Event::PttRenewed { expires_at_ns });
        }
    }

    fn pop_event(&mut self) -> Option<Event> {
        if let Some(observed_at_ns) = self.pending_deactivated.take() {
            return Some(Event::PttReleased {
                reason: PttReleaseReason::PortalDeactivated { observed_at_ns },
            });
        }
        self.events.pop_front()
    }
}

struct Configuration {
    generation: u64,
    chord: Option<String>,
}

/// `configure` acknowledges admission, not user consent. A pending dialog never opens the gate.
pub(crate) struct PortalPtt {
    commands: Option<mpsc::Sender<Configuration>>,
    state: Arc<Mutex<State>>,
    thread: Option<JoinHandle<()>>,
    done: std::sync::mpsc::Receiver<()>,
}

impl PortalPtt {
    pub(crate) fn new() -> Self {
        let (commands, receiver) = mpsc::channel(1);
        let (done_tx, done) = std::sync::mpsc::sync_channel(1);
        let state = Arc::new(Mutex::new(State::default()));
        let worker_state = state.clone();
        let thread = std::thread::spawn(move || {
            // Match platform-wayland's ashpd runtime; ashpd forbids mixed backends.
            if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                runtime.block_on(worker(receiver, &worker_state));
            }
            worker_state
                .lock()
                .unwrap()
                .release(PttReleaseReason::UnavailableOrReset);
            let _ = done_tx.send(());
        });
        Self {
            commands: Some(commands),
            state,
            thread: Some(thread),
            done,
        }
    }

    /// Availability/version and both signal subscriptions only: never creates a session.
    pub(crate) fn probe() -> Result<u32, BackendError> {
        if !wayland_session() {
            return Err(BackendError::Unsupported);
        }
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                let _ = tx.send(Err(BackendError::ServerUnavailable));
                return;
            };
            let result = runtime
                .block_on(bounded(
                    async {
                        let portal = GlobalShortcuts::new()
                            .await
                            .map_err(|_| BackendError::Unsupported)?;
                        if portal.version() == 0 {
                            return Err(BackendError::Unsupported);
                        }
                        let _pressed = portal
                            .receive_activated()
                            .await
                            .map_err(|_| BackendError::Unsupported)?;
                        let _released = portal
                            .receive_deactivated()
                            .await
                            .map_err(|_| BackendError::Unsupported)?;
                        Ok(portal.version())
                    },
                    IO_TIMEOUT,
                ))
                .unwrap_or(Err(BackendError::ServerUnavailable));
            let _ = tx.send(result);
        });
        rx.recv_timeout(IO_TIMEOUT + Duration::from_millis(100))
            .unwrap_or(Err(BackendError::ServerUnavailable))
    }

    pub(crate) fn configure(
        &mut self,
        binding: Option<PttHotkey>,
        reserved: &[PttHotkey],
    ) -> Result<(), BackendError> {
        let chord = validate_binding(binding, reserved)?;
        if chord.is_some() && !wayland_session() {
            return Err(BackendError::Unsupported);
        }
        let mut state = self.state.lock().unwrap();
        let generation = state
            .generation
            .checked_add(1)
            .ok_or(BackendError::Stopped)?;
        // Invalidate first, including when a busy/disconnected command queue refuses admission.
        state.generation = generation;
        state.release(PttReleaseReason::UnavailableOrReset);
        self.commands
            .as_mut()
            .ok_or(BackendError::Stopped)?
            .try_send(Configuration { generation, chord })
            .map_err(|_| BackendError::ServerUnavailable)
    }

    pub(crate) fn poll_event(&mut self) -> Option<Event> {
        let mut state = self.state.lock().unwrap();
        // A stalled worker must not leave stale grants in the consumer queue.
        if state
            .held_until
            .is_some_and(|expiry| monotonic_ns() >= expiry)
        {
            state.release(PttReleaseReason::LeaseExpired);
        }
        state.pop_event()
    }

    pub(crate) fn shutdown(&mut self) {
        self.state
            .lock()
            .unwrap()
            .release(PttReleaseReason::UnavailableOrReset);
        self.commands.take();
        if self.done.recv_timeout(Duration::from_millis(500)).is_ok() {
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
        // Never wait indefinitely for a desktop service or approval dialog.
        self.thread.take();
    }
}

impl Drop for PortalPtt {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn wayland_session() -> bool {
    match std::env::var("XDG_SESSION_TYPE") {
        Ok(kind) => kind.eq_ignore_ascii_case("wayland"),
        Err(_) => std::env::var_os("WAYLAND_DISPLAY").is_some(),
    }
}

async fn bounded<F: Future>(future: F, timeout: Duration) -> Option<F::Output> {
    let future = future.fuse();
    let deadline = futures_timer::Delay::new(timeout).fuse();
    futures::pin_mut!(future, deadline);
    select_biased! { result = future => Some(result), _ = deadline => None }
}

async fn worker(mut commands: mpsc::Receiver<Configuration>, state: &Arc<Mutex<State>>) {
    let mut next = commands.next().await;
    while let Some(configuration) = next {
        next = if let Some(chord) = configuration.chord {
            let run =
                run_configuration(configuration.generation, &chord, state, &mut commands).await;
            {
                let mut current = state.lock().unwrap();
                if current.generation == configuration.generation {
                    current.release(PttReleaseReason::UnavailableOrReset);
                    current.push(Event::ResyncRequired);
                }
            }
            match run {
                Outcome::Command(command) => command,
                Outcome::Failed => commands.next().await,
            }
        } else {
            commands.next().await
        };
    }
}

enum Outcome {
    Command(Option<Configuration>),
    Failed,
}

async fn run_configuration(
    generation: u64,
    chord: &str,
    state: &Arc<Mutex<State>>,
    commands: &mut mpsc::Receiver<Configuration>,
) -> Outcome {
    // A private connection is dropped on cancellation, including cancellation during CreateSession.
    let setup = async {
        let connection = ashpd::zbus::Connection::session().await.ok()?;
        let portal = GlobalShortcuts::with_connection(connection).await.ok()?;
        if portal.version() == 0 {
            return None;
        }
        let activated = portal.receive_activated().await.ok()?;
        let deactivated = portal.receive_deactivated().await.ok()?;
        let session_started_ns = monotonic_ns();
        let session = portal
            .create_session(CreateSessionOptions::default())
            .await
            .ok()?;
        Some((portal, session, activated, deactivated, session_started_ns))
    };
    let result = {
        let setup = bounded(setup, IO_TIMEOUT).fuse();
        let command = commands.next().fuse();
        futures::pin_mut!(setup, command);
        select_biased! {
            command = command => return Outcome::Command(command),
            result = setup => result,
        }
    };
    let Some(Some((portal, session, activated, deactivated, session_started_ns))) = result else {
        return Outcome::Failed;
    };
    let active = async {
        let toml::Value::String(path) = toml::Value::try_from(&session).ok()? else {
            return None;
        };
        let closed_proxy = ashpd::zbus::Proxy::new(
            portal.connection(),
            "org.freedesktop.portal.Desktop",
            path.as_str(),
            "org.freedesktop.portal.Session",
        )
        .await
        .ok()?;
        let closed = closed_proxy.receive_signal("Closed").await.ok()?;
        let shortcuts = [
            NewShortcut::new(SHORTCUT_ID, "Bifrost: hold to talk").preferred_trigger(Some(chord))
        ];
        // Both edges and Closed are subscribed before a bind can show a dialog or emit events.
        let request = portal
            .bind_shortcuts(&session, &shortcuts, None, BindShortcutsOptions::default())
            .await
            .ok()?;
        let response = request.response().ok()?;
        if !response.shortcuts().iter().any(|shortcut| {
            shortcut.id() == SHORTCUT_ID && !shortcut.trigger_description().trim().is_empty()
        }) {
            return None;
        }
        if !state.lock().unwrap().registered(generation) {
            return None;
        }
        let mut timeline = PortalTimeline::new(session_started_ns);
        futures::pin_mut!(activated, deactivated, closed);
        loop {
            let timer = futures_timer::Delay::new(Duration::from_millis(25)).fuse();
            let pressed = activated.next().fuse();
            let released = deactivated.next().fuse();
            let closed_event = closed.next().fuse();
            futures::pin_mut!(timer, pressed, released, closed_event);
            enum Edge {
                Press(Duration),
                Release(Duration),
                Tick,
            }
            let edge = select_biased! {
                _ = closed_event => return None,
                event = released => {
                    let event = event?;
                    if event.session_handle().as_str() != path { return None; }
                    if event.shortcut_id() != SHORTCUT_ID { continue; }
                    Edge::Release(event.timestamp())
                },
                event = pressed => {
                    let event = event?;
                    if event.session_handle().as_str() != path { return None; }
                    if event.shortcut_id() != SHORTCUT_ID { continue; }
                    Edge::Press(event.timestamp())
                },
                _ = timer => Edge::Tick,
            };
            let mut current = state.lock().unwrap();
            if current.generation != generation {
                return None;
            }
            let now = monotonic_ns();
            let is_activation = matches!(edge, Edge::Press(_));
            match edge {
                Edge::Press(timestamp) | Edge::Release(timestamp) => {
                    if !current.portal_edge(
                        generation,
                        is_activation,
                        timestamp,
                        now,
                        &mut timeline,
                    ) {
                        return None;
                    }
                }
                Edge::Tick => current.tick(now),
            }
        }
        #[allow(unreachable_code)]
        Some(())
    };
    let outcome = {
        let active = active.fuse();
        let command = commands.next().fuse();
        futures::pin_mut!(active, command);
        select_biased! {
            command = command => Outcome::Command(command),
            _ = active => Outcome::Failed,
        }
    };
    {
        let mut current = state.lock().unwrap();
        if current.generation == generation {
            current.release(PttReleaseReason::UnavailableOrReset);
        }
    }
    // Session.Close aborts related interactions too, including a pending BindShortcuts dialog.
    let _ = bounded(session.close(), Duration::from_millis(200)).await;
    outcome
}

fn validate_binding(
    binding: Option<PttHotkey>,
    reserved: &[PttHotkey],
) -> Result<Option<String>, BackendError> {
    if reserved.len() > 16 {
        return Err(BackendError::InvalidArgument);
    }
    for (index, hotkey) in reserved.iter().enumerate() {
        hotkey.validate()?;
        if reserved[..index].contains(hotkey) {
            return Err(BackendError::InvalidArgument);
        }
    }
    binding
        .map(|hotkey| {
            hotkey.validate()?;
            if reserved.contains(&hotkey) {
                return Err(BackendError::InvalidArgument);
            }
            preferred_chord(hotkey).ok_or(BackendError::Unsupported)
        })
        .transpose()
}

fn preferred_chord(hotkey: PttHotkey) -> Option<String> {
    let key = match hotkey.key_code {
        0x04..=0x1d => ((b'A' + (hotkey.key_code - 0x04) as u8) as char).to_string(),
        0x1e..=0x26 => (hotkey.key_code - 0x1d).to_string(),
        0x27 => "0".into(),
        0x28 => "Return".into(),
        0x29 => "Escape".into(),
        0x2a => "BackSpace".into(),
        0x2b => "Tab".into(),
        0x2c => "space".into(),
        0x3a..=0x45 => format!("F{}", hotkey.key_code - 0x39),
        0x49 => "Insert".into(),
        0x4a => "Home".into(),
        0x4b => "Page_Up".into(),
        0x4c => "Delete".into(),
        0x4d => "End".into(),
        0x4e => "Page_Down".into(),
        0x4f => "Right".into(),
        0x50 => "Left".into(),
        0x51 => "Down".into(),
        0x52 => "Up".into(),
        0x68..=0x73 => format!("F{}", hotkey.key_code - 0x68 + 13),
        // Layout-sensitive punctuation, lock keys and keypad usages have no safe HID-to-keysym equivalence.
        _ => return None,
    };
    let mut chord = String::new();
    for (enabled, modifier) in [
        (hotkey.control, "CTRL+"),
        (hotkey.alt, "ALT+"),
        (hotkey.shift, "SHIFT+"),
        (hotkey.meta, "SUPER+"),
    ] {
        if enabled {
            chord.push_str(modifier);
        }
    }
    chord.push_str(&key);
    Some(chord)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(code: u32) -> PttHotkey {
        PttHotkey {
            key_code: code,
            control: true,
            alt: true,
            shift: true,
            meta: false,
        }
    }

    #[test]
    fn conservative_hid_mapping_and_conflicts() {
        assert_eq!(
            preferred_chord(key(0x45)).as_deref(),
            Some("CTRL+ALT+SHIFT+F12")
        );
        assert_eq!(
            preferred_chord(PttHotkey {
                meta: true,
                ..key(0x04)
            })
            .as_deref(),
            Some("CTRL+ALT+SHIFT+SUPER+A")
        );
        assert_eq!(
            preferred_chord(key(0x73)).as_deref(),
            Some("CTRL+ALT+SHIFT+F24")
        );
        assert!(preferred_chord(key(0x2d)).is_none());
        assert_eq!(
            validate_binding(Some(key(0x45)), &[key(0x45)]),
            Err(BackendError::InvalidArgument)
        );
        assert_eq!(
            validate_binding(None, &[key(4), key(4)]),
            Err(BackendError::InvalidArgument)
        );
        assert_eq!(
            validate_binding(Some(key(3)), &[]),
            Err(BackendError::InvalidArgument)
        );
    }

    #[test]
    fn undefined_timestamp_base_cancels_without_receipt_time_calibration() {
        for base_ms in [1, 50_000, u64::MAX - 100] {
            let mut timeline = PortalTimeline::new(1_000_000_000);
            assert_eq!(
                timeline.observe(Duration::from_millis(base_ms), 1_200_000_000),
                Some(999_000_000)
            );
            assert_eq!(
                timeline.observe(Duration::from_millis(base_ms + 10), 1_200_000_000),
                Some(1_009_000_000)
            );
        }
    }

    #[test]
    fn queued_portal_signals_stay_before_fence_until_a_fresh_logical_cycle() {
        let mut timeline = PortalTimeline::new(1_000_000_000);
        let mut state = registered_state();
        let fence = 1_010_000_000;
        for (activated, timestamp_ms, receipt_ns, after_fence) in [
            (true, 50_000, 1_020_000_000, false),
            (false, 50_005, 1_025_000_000, false),
            (true, 50_006, 1_026_000_000, false),
            (false, 50_030, 1_040_000_000, true),
            (true, 50_031, 1_041_000_000, true),
        ] {
            let observed = timeline
                .observe(Duration::from_millis(timestamp_ms), receipt_ns)
                .unwrap();
            assert_eq!(observed > fence, after_fence);
            state.edge(0, activated, observed, receipt_ns);
            if activated {
                assert!(matches!(
                    state.pop_event(),
                    Some(Event::PttPressed { observed_at_ns, .. }) if observed_at_ns == observed
                ));
            } else {
                assert_eq!(state.pop_event(), Some(deactivated(observed)));
            }
        }
    }

    #[test]
    fn kde_zero_and_constant_clocks_report_unverifiable_not_release_proof() {
        for constant_ms in [0, 500] {
            let mut timeline = PortalTimeline::new(1_000_000_000);
            let mut state = registered_state();
            let timestamp = Duration::from_millis(constant_ms);
            if constant_ms != 0 {
                assert!(state.portal_edge(0, true, timestamp, 1_010_000_000, &mut timeline));
                assert!(matches!(state.pop_event(), Some(Event::PttPressed { .. })));
            }
            assert!(!state.portal_edge(0, false, timestamp, 1_020_000_000, &mut timeline));
            assert_eq!(
                state.pop_event(),
                Some(Event::PttReleased {
                    reason: PttReleaseReason::PortalFreshnessUnavailable,
                })
            );
            assert_eq!(state.pop_event(), Some(Event::ResyncRequired));
            assert!(state.held_until.is_none());
            // A later signal cannot reopen the failed session.
            assert!(!state.portal_edge(
                0,
                true,
                Duration::from_millis(501),
                1_030_000_000,
                &mut timeline,
            ));
            assert!(state.pop_event().is_none());
        }
    }

    #[test]
    fn seconds_style_values_fail_age_cap_without_rescaling_or_catchup() {
        let mut timeline = PortalTimeline::new(1_000_000_000);
        let mut state = registered_state();
        assert!(state.portal_edge(
            0,
            true,
            Duration::from_millis(42),
            1_100_000_000,
            &mut timeline,
        ));
        state.pop_event();
        assert!(!state.portal_edge(
            0,
            false,
            Duration::from_millis(43),
            2_100_000_000,
            &mut timeline,
        ));
        assert_eq!(
            state.pop_event(),
            Some(Event::PttReleased {
                reason: PttReleaseReason::PortalFreshnessUnavailable,
            })
        );
        assert_eq!(state.pop_event(), Some(Event::ResyncRequired));
        // No units guessing, new receipt anchor, or reopening after failure.
        assert!(!state.portal_edge(
            0,
            true,
            Duration::from_millis(44),
            3_100_000_000,
            &mut timeline,
        ));
        assert!(state.pop_event().is_none());
    }

    #[test]
    fn zero_regression_future_and_unmappable_timestamp_jumps_fail_closed() {
        let mut zero = PortalTimeline::new(1_000_000_000);
        assert_eq!(zero.observe(Duration::ZERO, 1_100_000_000), None);
        for timestamp in [499, 500, 1_500, u64::MAX] {
            let mut timeline = PortalTimeline::new(1_000_000_000);
            assert_eq!(
                timeline.observe(Duration::from_millis(500), 1_100_000_000),
                Some(999_000_000)
            );
            assert_eq!(
                timeline.observe(Duration::from_millis(timestamp), 1_200_000_000),
                None
            );
        }
        let mut invalid_host = PortalTimeline::new(1);
        assert_eq!(invalid_host.observe(Duration::from_millis(1), 10), None);
    }

    #[test]
    fn fresh_millisecond_cycle_at_age_limit_retains_original_time_and_receipt_lease() {
        let mut timeline = PortalTimeline::new(1_000_000_000);
        let mut state = registered_state();
        let mapped_release = 999_000_000;
        let release_receipt = mapped_release + PTT_LEASE_NS;
        assert!(state.portal_edge(
            0,
            false,
            Duration::from_millis(50_000),
            release_receipt,
            &mut timeline,
        ));
        assert_eq!(state.pop_event(), Some(deactivated(mapped_release)));
        let mapped_activation = mapped_release + 10_000_000;
        let activation_receipt = release_receipt + 10_000_000;
        assert!(state.portal_edge(
            0,
            true,
            Duration::from_millis(50_010),
            activation_receipt,
            &mut timeline,
        ));
        assert_eq!(
            state.pop_event(),
            Some(Event::PttPressed {
                observed_at_ns: mapped_activation,
                expires_at_ns: activation_receipt + PTT_LEASE_NS,
            })
        );
    }

    #[test]
    fn excessive_initial_lag_closes_both_edges_without_receipt_relabel_or_catchup() {
        for activated in [false, true] {
            let mut timeline = PortalTimeline::new(1_000_000_000);
            let mut state = registered_state();
            let now = 999_000_000 + PTT_LEASE_NS + 1;
            assert!(!state.portal_edge(
                0,
                activated,
                Duration::from_millis(50_000),
                now,
                &mut timeline,
            ));
            assert_eq!(
                state.pop_event(),
                Some(Event::PttReleased {
                    reason: PttReleaseReason::PortalFreshnessUnavailable,
                })
            );
            assert_eq!(state.pop_event(), Some(Event::ResyncRequired));
            assert!(state.held_until.is_none());
            assert!(!state.portal_edge(
                0,
                false,
                Duration::from_millis(50_001),
                now + 1_000_000,
                &mut timeline,
            ));
            assert!(!state.portal_edge(
                0,
                true,
                Duration::from_millis(50_002),
                now + 2_000_000,
                &mut timeline,
            ));
            assert!(state.pop_event().is_none());
        }
    }

    #[test]
    fn replacement_session_uses_its_own_lower_bound_and_timestamp_base() {
        let fence = 2_000_000_000;
        let mut old = PortalTimeline::new(1_000_000_000);
        let mut new = PortalTimeline::new(3_000_000_000);
        let old_observed = old
            .observe(Duration::from_millis(50_000), 1_010_000_000)
            .unwrap();
        let new_observed = new
            .observe(Duration::from_millis(7), 3_010_000_000)
            .unwrap();
        assert!(old_observed <= fence);
        assert!(new_observed > fence);
        let mut state = registered_state();
        state.generation = 1;
        assert!(state.registered(1));
        state.edge(0, false, old_observed, 3_010_000_000);
        assert!(state.pop_event().is_none());
        state.edge(1, false, new_observed, 3_010_000_000);
        assert_eq!(state.pop_event(), Some(deactivated(new_observed)));
    }

    fn registered_state() -> State {
        let mut state = State::default();
        assert!(state.registered(0));
        state
    }

    fn deactivated(observed_at_ns: u64) -> Event {
        Event::PttReleased {
            reason: PttReleaseReason::PortalDeactivated { observed_at_ns },
        }
    }

    #[test]
    fn modifier_logical_deactivation_then_reactivation_is_not_physical_up() {
        let mut state = registered_state();
        state.edge(0, true, 10, 10);
        assert!(matches!(state.pop_event(), Some(Event::PttPressed { .. })));
        // A modifier can deactivate the chord while its non-modifier stays held.
        state.edge(0, false, 20, 20);
        assert_eq!(state.pop_event(), Some(deactivated(20)));
        state.edge(0, true, 30, 30);
        assert_eq!(
            state.pop_event(),
            Some(Event::PttPressed {
                observed_at_ns: 30,
                expires_at_ns: 30 + PTT_LEASE_NS,
            })
        );
    }

    #[test]
    fn expiry_and_held_renewals_never_reopen() {
        let mut state = registered_state();
        state.edge(0, true, 1, 1);
        state.tick(1 + RENEW_NS);
        state.tick(1 + RENEW_NS * 2);
        assert_eq!(state.events.len(), 2);
        assert!(matches!(state.events[1], Event::PttRenewed { .. }));
        state.tick(1 + RENEW_NS * 2 + PTT_LEASE_NS);
        assert_eq!(
            state.pop_event(),
            Some(Event::PttReleased {
                reason: PttReleaseReason::LeaseExpired,
            })
        );
        state.tick(2_000_000_000);
        state.edge(0, true, 2_000_000_001, 2_000_000_001);
        assert!(state.pop_event().is_none());
        assert!(state.held_until.is_none());
        state.edge(0, false, 2_000_000_002, 2_000_000_002);
        assert_eq!(state.pop_event(), Some(deactivated(2_000_000_002)));
        state.edge(0, true, 2_000_000_003, 2_000_000_003);
        assert!(matches!(state.pop_event(), Some(Event::PttPressed { .. })));
    }

    #[test]
    fn duplicates_stale_edges_and_same_tick_activation_do_not_rearm() {
        let mut state = registered_state();
        state.edge(0, true, 10, 10);
        state.edge(0, true, 10, 11);
        assert_eq!(state.events.len(), 1);
        state.edge(0, false, 20, 20);
        state.edge(0, false, 20, 21);
        state.edge(0, false, 21, 22);
        state.edge(0, true, 19, 22);
        state.edge(0, true, 20, 23);
        assert_eq!(state.pop_event(), Some(deactivated(20)));
        assert!(state.pop_event().is_none());
        assert!(state.held_until.is_none());
        state.edge(0, true, 21, 24);
        assert!(matches!(state.pop_event(), Some(Event::PttPressed { .. })));
        state.edge(0, false, 19, 25);
        assert!(state.held_until.is_some());
    }

    #[test]
    fn queued_edges_keep_original_time_across_a_child_fence() {
        let mut state = registered_state();
        let fence = 100;
        state.edge(0, false, 90, 110);
        assert_eq!(state.pop_event(), Some(deactivated(90)));
        state.edge(0, true, 95, 120);
        let Some(Event::PttPressed {
            observed_at_ns,
            expires_at_ns,
        }) = state.pop_event()
        else {
            panic!("expected original queued activation");
        };
        assert!(observed_at_ns < fence);
        assert_eq!(expires_at_ns, 120 + PTT_LEASE_NS);
        state.edge(0, false, 130, 140);
        assert_eq!(state.pop_event(), Some(deactivated(130)));
        state.edge(0, true, 150, 160);
        assert!(matches!(
            state.pop_event(),
            Some(Event::PttPressed {
                observed_at_ns: 150,
                ..
            })
        ));
    }

    #[test]
    fn stalled_queued_activation_expires_and_held_renewals_cannot_reopen() {
        let mut state = registered_state();
        state.edge(0, true, 10, 10 + PTT_LEASE_NS);
        state.tick(10 + PTT_LEASE_NS * 2);
        assert_eq!(
            state.pop_event(),
            Some(Event::PttReleased {
                reason: PttReleaseReason::LeaseExpired,
            })
        );
        state.edge(0, true, 10 + PTT_LEASE_NS * 3, 10 + PTT_LEASE_NS * 3);
        assert!(state.pop_event().is_none());
        assert!(state.held_until.is_none());
    }

    #[test]
    fn reset_and_session_replacement_reject_old_session_edges() {
        let mut state = registered_state();
        state.edge(0, true, 10, 10);
        state.release(PttReleaseReason::UnavailableOrReset);
        state.edge(0, false, 20, 20);
        state.edge(0, true, 30, 30);
        assert_eq!(
            state.pop_event(),
            Some(Event::PttReleased {
                reason: PttReleaseReason::UnavailableOrReset,
            })
        );
        assert!(state.pop_event().is_none());
        state.generation = 1;
        assert!(!state.registered(0));
        assert!(state.registered(1));
        state.edge(0, false, 40, 40);
        state.edge(0, true, 50, 50);
        assert!(state.pop_event().is_none());
        state.edge(1, false, 60, 60);
        assert_eq!(state.pop_event(), Some(deactivated(60)));
        state.edge(1, true, 70, 70);
        assert!(matches!(state.pop_event(), Some(Event::PttPressed { .. })));
    }

    #[test]
    fn full_queue_preserves_original_logical_release_through_resets() {
        let mut state = registered_state();
        state.edge(0, true, 10, 10);
        state.pop_event();
        for _ in 0..QUEUE_CAPACITY {
            state.push(Event::ResyncRequired);
        }
        state.edge(0, false, 20, 20);
        for _ in 0..QUEUE_CAPACITY * 2 {
            state.push(Event::ResyncRequired);
        }
        state.release(PttReleaseReason::UnavailableOrReset);
        assert!(state.events.len() <= QUEUE_CAPACITY);
        assert_eq!(state.pop_event(), Some(deactivated(20)));
        while let Some(event) = state.pop_event() {
            assert!(!matches!(
                event,
                Event::PttReleased {
                    reason: PttReleaseReason::PortalDeactivated { .. }
                } | Event::PttPressed { .. }
                    | Event::PttRenewed { .. }
            ));
        }
    }

    #[test]
    fn lost_host_clock_closes_the_session_without_rearm() {
        let mut state = registered_state();
        state.edge(0, true, 10, 10);
        state.tick(0);
        assert_eq!(
            state.pop_event(),
            Some(Event::PttReleased {
                reason: PttReleaseReason::PortalFreshnessUnavailable,
            })
        );
        assert_eq!(state.pop_event(), Some(Event::ResyncRequired));
        state.tick(RENEW_NS);
        state.edge(0, false, 20, 20);
        assert!(state.pop_event().is_none());
    }

    #[test]
    fn invalid_clock_closes_without_claiming_logical_release() {
        for (observed, now) in [(0, 20), (30, 20), (20, 0)] {
            let mut state = registered_state();
            state.edge(0, true, 10, 10);
            state.edge(0, false, observed, now);
            assert_eq!(
                state.pop_event(),
                Some(Event::PttReleased {
                    reason: PttReleaseReason::UnavailableOrReset,
                })
            );
            assert_eq!(state.pop_event(), Some(Event::ResyncRequired));
            assert!(state.held_until.is_none());
            assert!(state.live_generation.is_none());
        }
    }

    #[test]
    fn reconfiguration_admission_failure_and_shutdown_are_not_release_proof() {
        let (commands, receiver) = mpsc::channel(1);
        let (done_tx, done) = std::sync::mpsc::channel();
        drop(done_tx);
        let state = Arc::new(Mutex::new(registered_state()));
        let mut ptt = PortalPtt {
            commands: Some(commands),
            state: state.clone(),
            thread: None,
            done,
        };
        state.lock().unwrap().edge(0, true, 1, 1);
        ptt.configure(None, &[]).unwrap();
        assert_eq!(
            state.lock().unwrap().pop_event(),
            Some(Event::PttReleased {
                reason: PttReleaseReason::UnavailableOrReset,
            })
        );
        drop(receiver);
        assert_eq!(
            ptt.configure(None, &[]),
            Err(BackendError::ServerUnavailable)
        );
        ptt.shutdown();
        let mut state = state.lock().unwrap();
        assert!(state.held_until.is_none());
        assert!(state.live_generation.is_none());
        while let Some(event) = state.pop_event() {
            assert!(!matches!(
                event,
                Event::PttReleased {
                    reason: PttReleaseReason::PortalDeactivated { .. }
                }
            ));
        }
    }

    #[test]
    fn opt_in_portal_availability_only() {
        if std::env::var("BIFROST_PORTAL_TESTS").as_deref() != Ok("1") {
            eprintln!(
                "skip portal availability test: set BIFROST_PORTAL_TESTS=1; no binding requested"
            );
            return;
        }
        match PortalPtt::probe() {
            Ok(version) => {
                eprintln!(
                    "GlobalShortcuts portal version={version}; availability-only, no binding"
                );
                assert!(version >= 1);
            }
            Err(error @ (BackendError::Unsupported | BackendError::ServerUnavailable)) => {
                eprintln!("GlobalShortcuts unavailable: {error:?}; no binding requested");
            }
            Err(error) => panic!("unexpected portal probe result: {error:?}"),
        }
    }
}
