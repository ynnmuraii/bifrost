//! Audio-owned GlobalShortcuts PTT. No input-agent dependency or synthetic key events.
use std::collections::VecDeque;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use ashpd::desktop::global_shortcuts::{BindShortcutsOptions, GlobalShortcuts, NewShortcut};
use ashpd::desktop::CreateSessionOptions;
use audio_backend::{BackendError, Event, PttHotkey, PTT_LEASE_NS};
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

#[derive(Default)]
struct State {
    generation: u64,
    held_until: Option<u64>,
    renewed_at: u64,
    events: VecDeque<Event>,
}

impl State {
    fn release(&mut self) {
        self.held_until = None;
        self.events
            .retain(|event| !matches!(event, Event::PttPressed { .. } | Event::PttRenewed { .. }));
        if !matches!(self.events.back(), Some(Event::PttReleased)) {
            self.push(Event::PttReleased);
        }
    }

    fn push(&mut self, event: Event) {
        if matches!(event, Event::PttRenewed { .. }) {
            self.events
                .retain(|event| !matches!(event, Event::PttRenewed { .. }));
        }
        if self.events.len() >= QUEUE_CAPACITY {
            self.held_until = None;
            self.events.clear();
            self.events.push_back(Event::PttReleased);
            self.events.push_back(Event::ResyncRequired);
            return;
        }
        self.events.push_back(event);
    }

    fn press(&mut self, now: u64) {
        if self.held_until.is_some() || now == 0 {
            return;
        }
        let expires_at_ns = now.saturating_add(PTT_LEASE_NS);
        self.held_until = Some(expires_at_ns);
        self.renewed_at = now;
        self.push(Event::PttPressed {
            observed_at_ns: now,
            expires_at_ns,
        });
    }

    fn tick(&mut self, now: u64) {
        let Some(expiry) = self.held_until else {
            return;
        };
        if now == 0 || now >= expiry {
            self.release();
        } else if now.saturating_sub(self.renewed_at) >= RENEW_NS {
            let expires_at_ns = now.saturating_add(PTT_LEASE_NS);
            self.held_until = Some(expires_at_ns);
            self.renewed_at = now;
            self.push(Event::PttRenewed { expires_at_ns });
        }
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
            worker_state.lock().unwrap().release();
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
        state.release();
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
            state.release();
        }
        state.events.pop_front()
    }

    pub(crate) fn shutdown(&mut self) {
        self.state.lock().unwrap().release();
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
                    current.release();
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
        let session = portal
            .create_session(CreateSessionOptions::default())
            .await
            .ok()?;
        Some((portal, session, activated, deactivated))
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
    let Some(Some((portal, session, activated, deactivated))) = result else {
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
        futures::pin_mut!(activated, deactivated, closed);
        // Separate subscriptions may drain out of order; never reopen on an old press.
        let mut last_release = None;
        loop {
            let timer = futures_timer::Delay::new(Duration::from_millis(25)).fuse();
            let pressed = activated.next().fuse();
            let released = deactivated.next().fuse();
            let closed_event = closed.next().fuse();
            futures::pin_mut!(timer, pressed, released, closed_event);
            enum Edge {
                Press,
                Release,
                Tick,
            }
            let edge = select_biased! {
                _ = closed_event => return None,
                event = released => {
                    let event = event?;
                    if event.session_handle().as_str() != path || event.shortcut_id() != SHORTCUT_ID { continue; }
                    last_release = Some(event.timestamp());
                    Edge::Release
                },
                event = pressed => {
                    let event = event?;
                    if event.session_handle().as_str() != path || event.shortcut_id() != SHORTCUT_ID { continue; }
                    if last_release.is_some_and(|timestamp| event.timestamp() <= timestamp) { continue; }
                    Edge::Press
                },
                _ = timer => Edge::Tick,
            };
            let mut current = state.lock().unwrap();
            if current.generation != generation {
                return None;
            }
            match edge {
                Edge::Press => current.press(monotonic_ns()),
                Edge::Release => current.release(),
                Edge::Tick => current.tick(monotonic_ns()),
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
            current.release();
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
    fn lease_renewal_expiry_and_no_reopening() {
        let mut state = State::default();
        state.press(1);
        assert_eq!(state.held_until, Some(1 + PTT_LEASE_NS));
        state.tick(1 + RENEW_NS);
        assert!(matches!(
            state.events.back(),
            Some(Event::PttRenewed { .. })
        ));
        state.tick(1 + RENEW_NS + PTT_LEASE_NS);
        assert_eq!(state.events.pop_front(), Some(Event::PttReleased));
        state.tick(1_000_000_000);
        assert!(state.held_until.is_none());
        assert!(state.events.is_empty());
    }

    #[test]
    fn release_cancellation_and_overflow_fail_closed() {
        let mut state = State::default();
        state.press(1);
        state.release();
        assert_eq!(state.events.pop_front(), Some(Event::PttReleased));
        state.press(2);
        for _ in 0..QUEUE_CAPACITY {
            state.push(Event::ResyncRequired);
        }
        assert!(state.held_until.is_none());
        assert!(state.events.contains(&Event::PttReleased));
        assert!(state.events.len() <= QUEUE_CAPACITY);
        state.release();
        state.tick(100_000_002);
        assert!(!state
            .events
            .iter()
            .any(|event| matches!(event, Event::PttRenewed { .. } | Event::PttPressed { .. })));
    }

    #[test]
    fn ordering_and_renewal_coalescing() {
        let mut state = State::default();
        state.press(1);
        state.tick(RENEW_NS + 1);
        state.tick(RENEW_NS * 2 + 1);
        assert_eq!(state.events.len(), 2);
        assert!(matches!(state.events[0], Event::PttPressed { .. }));
        assert!(matches!(state.events[1], Event::PttRenewed { .. }));
        state.release();
        state.press(RENEW_NS * 2 + 2);
        assert!(matches!(state.events[0], Event::PttReleased));
        assert!(matches!(state.events[1], Event::PttPressed { .. }));
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
