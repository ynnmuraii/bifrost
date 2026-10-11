//! The agent's session loop: attach, the IPC command set, the watchdog and the
//! deadline of rule 4.
//!
//! One `select!` over two cancellation-safe futures — the framed IPC link
//! (`ipc::Framed` keeps partial reads in its own buffer, so cancelling `recv` never
//! loses a byte) and the backend's event poll, which always returns inside one
//! [`backend::POLL_TICK`]. Everything the agent owes core is on the frame path:
//!
//! | IPC (core → agent) | IPC (agent → core) | reference |
//! |---|---|---|
//! | `AgentAttach` | `AgentReady` | rule 7, `RECOVERY` allow-list |
//! | `BeginCapture` | `CaptureReady` | rules 2, 5 |
//! | `EndCapture` | `Released` | |
//! | `PrepareInject` | `Ready` | `AgentEvent::Ready` = «agent answered `PrepareInject`» |
//! | `Inject` | `InjectResult` | |
//! | `ArmDeadline` | `DeadlineExpired` | rule 4, ≤ 1000 ms on the agent's clock |
//! | `ReleaseSession` | `Released` | |
//! | `ReleaseKeys` | `Released` | rule 7 (only in `RECOVERY`) |
//! | `Ping` | `Pong` | rule 11 |
//! | — | `CapturedBatch`, `EmergencyFired`, `LocalInputDetected`, `SessionEvent` | rules 2, 3, 9 |

use std::time::{Duration, Instant};

use ::ipc::{AgentLink, Watchdog};
use protocol::ipc::agent_frame::Msg;
use protocol::ipc::{
    self, AgentError, AgentFrame, AgentMode, CaptureReady, CapturedBatch, DeadlineExpired,
    EmergencyFired, InjectResult, LocalEvent, Pong, PortalGrantChanged, Released,
};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::backend::{self, Backend, Event, Injected};
use crate::launch;
use crate::{Args, FRAME_MAJOR, FRAME_MINOR};

/// How the loop ended.
enum Stop {
    /// `AgentShutdown` from core.
    Shutdown,
    /// Core's channel closed (EOF) or broke; capture and ledger are released.
    LostCore,
}

/// Connects, verifies the launch, and runs until core says stop or disappears.
pub async fn run(args: &Args) -> anyhow::Result<()> {
    let endpoint = launch::endpoint_from_spec(&args.ipc_endpoint);
    let stream = ::ipc::transport::connect(&endpoint).await?;
    let mut link = AgentLink::new(stream, FRAME_MAJOR, FRAME_MINOR);

    let frame: AgentFrame = link.recv().await?;
    let attach = match frame.msg.as_ref() {
        Some(message) => ::ipc::agent::expect_attach(message)?.clone(),
        None => return Err(anyhow::anyhow!("first AgentFrame carried no message")),
    };
    ::ipc::agent::check_frame_version(frame.major, FRAME_MAJOR)?;
    let token = launch::take_launch_token(&args.token_file)?;
    ::ipc::agent::verify_launch_token(&token, &attach.launch_token)?;

    let mode = attach.mode;
    tracing::info!(
        mode = AgentMode::try_from(mode)
            .unwrap_or(AgentMode::Unspecified)
            .as_str_name(),
        core_silence_ms = attach.core_silence_ms,
        backend_override = attach.backend_override,
        restore_tokens = attach.restore_tokens.len(),
        "attached"
    );

    let started = Instant::now();
    let mut watchdog = match Watchdog::for_core_silence_ms(attach.core_silence_ms, 0) {
        Ok(watchdog) => watchdog,
        Err(error) => {
            tracing::warn!(
                %error,
                "AgentAttach.core_silence_ms outside 200..=500; using the default"
            );
            Watchdog::for_core_silence_ms(::ipc::DEFAULT_CORE_SILENCE_MS, 0)?
        }
    };

    let (mut backend, ready, refusals) = Backend::select(
        backend::BackendKindArg(attach.backend_override),
        args.dry_inject,
        args.dry_input_events,
        args.dry_motion,
        attach.emergency_hotkey.as_str(),
        attach.restore_tokens.as_slice(),
    )
    .await;
    if args.dry_inject {
        tracing::warn!("dry mode: no capture hooks, no injection; Inject is only logged");
    }
    if !backend.has_capabilities() {
        tracing::warn!(
            reason = ready.backend.degraded_reason.as_str(),
            "no capability advertised: this node stays local"
        );
    }
    link.send(Msg::Ready(ready.to_ipc())).await?;
    // A backend that is usable but incomplete (a missing portal, a dismissed dialog)
    // says so explicitly: `AgentError` is how `docs/ipc.proto` carries
    // `PORTAL_DENIED`/`PORTAL_UNAVAILABLE` and the interface they name.
    for refusal in &refusals {
        tracing::warn!(
            code = refusal.code,
            detail = refusal.detail.as_str(),
            "backend degraded"
        );
        send_error(&mut link, refusal.code, &refusal.detail).await?;
    }

    let stop = loop_forever(
        &mut link,
        &mut backend,
        &ready,
        mode,
        &mut watchdog,
        started,
    )
    .await?;

    // Both endings leave nothing of this session behind (rules 4, 6, 10; S2).
    if let Stop::LostCore = stop {
        release_everything(&mut backend).await;
    }
    tracing::info!("agent exiting");
    Ok(())
}

/// The loop in one place so every exit path releases through the same code.
async fn loop_forever<S: AsyncRead + AsyncWrite + Unpin>(
    link: &mut AgentLink<S>,
    backend: &mut Backend,
    ready: &backend::Ready,
    mode: i32,
    watchdog: &mut Watchdog,
    started: Instant,
) -> anyhow::Result<Stop> {
    // `ArmDeadline` arms this; rule 4 fixes the bound at 1000 ms.
    let mut deadline: Option<(Instant, Vec<u8>)> = None;
    // `BeginCapture(TEMPORARY)` self-timeout (rule 5).
    let mut self_timeout: Option<(Instant, Vec<u8>)> = None;
    // A lease the backend is currently holding state for.
    let mut held_lease: Option<Vec<u8>> = None;
    // Captured events waiting for the next tick, so one tick is one batch.
    let mut batch: Vec<LocalEvent> = Vec::new();
    // `CapturedBatch.at_edge`: set when the backend reports an edge crossing, cleared
    // once core has been told. The batch is sent even when it carries no events,
    // because the crossing itself is the news.
    let mut at_edge = false;
    let mut released_after_silence = false;
    // The node's own display set and its revision (`Hello.displays_revision`).
    let mut displays = crate::displays::DisplaysState::default();

    loop {
        tokio::select! {
            received = link.recv() => {
                let frame = match received {
                    Ok(frame) => frame,
                    Err(error) => {
                        tracing::warn!(%error, "IPC ended; releasing capture and ledger");
                        return Ok(Stop::LostCore);
                    }
                };
                let now = now_ms(started);
                watchdog.on_frame(now);
                released_after_silence = false;
                let Some(message) = frame.msg.as_ref() else {
                    send_error(link, "EMPTY_FRAME", "AgentFrame with no message").await?;
                    continue;
                };
                if !::ipc::agent::mode_allows(mode, message) {
                    tracing::warn!(
                        mode = AgentMode::try_from(mode).unwrap_or(AgentMode::Unspecified).as_str_name(),
                        "message refused by the RECOVERY allow-list"
                    );
                    send_error(link, "RECOVERY_ONLY_RELEASEKEYS", "agent is in RECOVERY mode").await?;
                    continue;
                }
                if let Some(stop) = handle_message(
                    link, backend, ready, message, &mut deadline, &mut self_timeout,
                    &mut held_lease, &mut batch, &mut displays,
                ).await? {
                    return Ok(stop);
                }
            }
            event = backend.next_event() => {
                let now = now_ms(started);
                if let Some(event) = event {
                    handle_backend_event(
                        link,
                        backend,
                        event,
                        &mut held_lease,
                        &mut batch,
                        &mut at_edge,
                    )
                    .await?;
                }

                // Rule 4: the deadline is the agent's own monotonic clock.
                if let Some((due, lease_id)) = deadline.as_ref() {
                    if Instant::now() >= *due {
                        let lease_id = lease_id.clone();
                        deadline = None;
                        tracing::warn!("deadline expired; releasing the session");
                        let released = backend.release_session().await;
                        link.send(Msg::DeadlineExpired(DeadlineExpired { lease_id })).await?;
                        tracing::info!(keys = released.keys, buttons = released.buttons, "released");
                        held_lease = None;
                    }
                }
                // Rule 5: a TEMPORARY capture ends by itself and the buffered edge
                // is played locally rather than reaching the wire.
                if let Some((due, lease_id)) = &self_timeout {
                    if Instant::now() >= *due {
                        let lease_id = lease_id.clone();
                        self_timeout = None;
                        tracing::warn!("self-timeout of a temporary capture");
                        let _ = backend.end_capture(true).await;
                        send_error(
                            link,
                            "CAPTURE_FAILED",
                            "temporary capture self-timeout expired",
                        )
                        .await?;
                        let _ = lease_id;
                        held_lease = None;
                    }
                }
                // Rule 6: core went quiet. No frame, not even a Ping.
                if watchdog.silence_exceeded(now) && !released_after_silence {
                    released_after_silence = true;
                    tracing::warn!(
                        silence_ms = watchdog.silence_ms(now),
                        "core silent past the limit; releasing locally"
                    );
                    release_everything(backend).await;
                    deadline = None;
                    self_timeout = None;
                    held_lease = None;
                }
            }
        }

        // One tick, one batch: keeps the frame rate bounded and lets the transport
        // coalesce motion on the far side. A tick that only learned about an edge
        // crossing still goes out, with no events.
        if !batch.is_empty() || at_edge {
            let events = std::mem::take(&mut batch);
            let edge = at_edge;
            at_edge = false;
            link.send(Msg::Captured(CapturedBatch {
                events,
                at_edge: edge,
            }))
            .await?;
        }
    }
}

/// Handles one core → agent frame. `Some(stop)` ends the loop.
#[allow(clippy::too_many_arguments)]
async fn handle_message<S: AsyncRead + AsyncWrite + Unpin>(
    link: &mut AgentLink<S>,
    backend: &mut Backend,
    ready: &backend::Ready,
    message: &Msg,
    deadline: &mut Option<(Instant, Vec<u8>)>,
    self_timeout: &mut Option<(Instant, Vec<u8>)>,
    held_lease: &mut Option<Vec<u8>>,
    batch: &mut Vec<LocalEvent>,
    displays: &mut crate::displays::DisplaysState,
) -> anyhow::Result<Option<Stop>> {
    match message {
        Msg::Attach(_) => {
            send_error(link, "PROTOCOL", "a second AgentAttach is not allowed").await?;
        }
        Msg::Ping(ping) => {
            link.send(Msg::Pong(Pong { nonce: ping.nonce })).await?;
        }
        Msg::BeginCapture(begin) => {
            let kind = match backend::capture_kind_from_i32(begin.kind) {
                Ok(kind) => kind,
                Err(detail) => {
                    send_error(link, "PROTOCOL", &detail).await?;
                    return Ok(None);
                }
            };
            match backend.begin_capture(kind).await {
                Ok(()) => {
                    *held_lease = Some(begin.lease_id.clone());
                    if kind == input_model::CaptureKind::Temporary && begin.self_timeout_ms > 0 {
                        *self_timeout = Some((
                            Instant::now()
                                + Duration::from_millis(u64::from(begin.self_timeout_ms).min(1000)),
                            begin.lease_id.clone(),
                        ));
                    } else {
                        *self_timeout = None;
                    }
                    link.send(Msg::CaptureReady(CaptureReady {
                        lease_id: begin.lease_id.clone(),
                        kind: begin.kind,
                    }))
                    .await?;
                }
                Err(detail) => {
                    tracing::error!(detail = detail.as_str(), "BeginCapture failed");
                    send_error(link, "CAPTURE_FAILED", &detail).await?;
                }
            }
        }
        Msg::EndCapture(end) => {
            *self_timeout = None;
            match backend.end_capture(end.replay_buffered_locally).await {
                Ok(released) => {
                    *held_lease = None;
                    link.send(Msg::Released(Released {
                        lease_id: end.lease_id.clone(),
                        keys_released: released.keys,
                        buttons_released: released.buttons,
                    }))
                    .await?;
                }
                Err(detail) => send_error(link, "CAPTURE_FAILED", &detail).await?,
            }
        }
        Msg::PrepareInject(prepare) => {
            match backend.prepare_inject().await {
                // `ipc.proto` has no dedicated "ready to receive" reply, and the FSM's
                // `AgentEvent::Ready` is documented as «агент ответил `PrepareInject`»,
                // so the acknowledgement is `AgentReady`.
                Ok(()) => {
                    *held_lease = Some(prepare.lease_id.clone());
                    link.send(Msg::Ready(ready.to_ipc())).await?;
                }
                Err(detail) => send_error(link, "CAPTURE_FAILED", &detail).await?,
            }
        }
        Msg::Inject(inject) => {
            let events: Vec<input_model::InputEvent> = inject
                .events
                .iter()
                .filter_map(backend::to_input_event)
                .collect();
            if events.len() != inject.events.len() {
                tracing::warn!(
                    offered = inject.events.len(),
                    accepted = events.len(),
                    "some injected events had no normalised form"
                );
            }
            let injected: Injected = backend.inject(&events).await;
            batch.clear();
            link.send(Msg::InjectResult(InjectResult {
                lease_id: inject.lease_id.clone(),
                input_batch_seq: inject.input_batch_seq,
                applied_count: injected.applied as u32,
                os_error: injected.os_error,
                actual: injected.actual.map(|position| ipc::PointerAbs {
                    // The receiver resolves the display from its own zones/geometry;
                    // the normalised event carries no display id.
                    display_id: String::new(),
                    x: position.x,
                    y: position.y,
                }),
            }))
            .await?;
        }
        Msg::ArmDeadline(arm) => {
            // Rule 4: `remaining_ms` is at most 1000 and the clock is ours.
            let remaining = u64::from(arm.remaining_ms).clamp(1, 1000);
            *deadline = Some((
                Instant::now() + Duration::from_millis(remaining),
                arm.lease_id.clone(),
            ));
            tracing::debug!(remaining, "deadline armed");
        }
        Msg::ReleaseSession(release) => {
            *deadline = None;
            *self_timeout = None;
            let released = backend.release_session().await;
            *held_lease = None;
            link.send(Msg::Released(Released {
                lease_id: release.lease_id.clone(),
                keys_released: released.keys,
                buttons_released: released.buttons,
            }))
            .await?;
        }
        Msg::ReleaseKeys(release) => {
            let keys: Vec<input_model::HidKey> = release
                .keys
                .iter()
                .map(|key| input_model::HidKey {
                    page: u16::try_from(key.usage_page).unwrap_or(0),
                    usage: u16::try_from(key.usage).unwrap_or(0),
                })
                .collect();
            let buttons: Vec<input_model::MouseButton> = release
                .buttons
                .iter()
                .filter_map(|button| match button {
                    1 => Some(input_model::MouseButton::Left),
                    2 => Some(input_model::MouseButton::Right),
                    3 => Some(input_model::MouseButton::Middle),
                    4 => Some(input_model::MouseButton::Back),
                    5 => Some(input_model::MouseButton::Forward),
                    _ => None,
                })
                .collect();
            let released = backend.release_keys(&keys, &buttons).await;
            tracing::info!(
                keys = released.keys,
                buttons = released.buttons,
                "recovery release"
            );
            link.send(Msg::Released(Released {
                lease_id: Vec::new(),
                keys_released: released.keys,
                buttons_released: released.buttons,
            }))
            .await?;
        }
        Msg::QueryDisplays(_) => {
            // KVM-040: the backend enumerates its monitors; the revision only moves
            // when the set does, so core can tell a re-query from a hot-plug.
            let current = backend.displays();
            let report = displays.observe(&current);
            tracing::info!(
                displays = report.displays.len(),
                revision = report.local_revision,
                "display report"
            );
            link.send(Msg::Displays(report)).await?;
        }
        Msg::ForgetPortalGrants(_) => {
            backend.forget_portal_grants().await;
        }
        Msg::Shutdown(_) => {
            tracing::info!("shutdown requested by core");
            release_everything(backend).await;
            return Ok(Some(Stop::Shutdown));
        }
        // Agent → core messages arriving here are a protocol error.
        other => {
            send_error(
                link,
                "PROTOCOL",
                &format!("{other:?} is an agent-to-core message"),
            )
            .await?;
        }
    }
    Ok(None)
}

/// Handles one backend event.
#[allow(clippy::too_many_arguments)]
async fn handle_backend_event<S: AsyncRead + AsyncWrite + Unpin>(
    link: &mut AgentLink<S>,
    backend: &mut Backend,
    event: Event,
    held_lease: &mut Option<Vec<u8>>,
    batch: &mut Vec<LocalEvent>,
    at_edge: &mut bool,
) -> anyhow::Result<()> {
    match event {
        Event::Input(input) => match crate::events::to_local_event(&input, monotonic_us()) {
            Some(event) => batch.push(event),
            None => tracing::trace!("event has no wire form and was dropped"),
        },
        Event::AtEdge { barrier_id } => {
            // `docs/05-protocol.md`: the source's `CapturedBatch` with `at_edge` is what
            // opens a transition. On a Wayland source this is the only edge signal
            // there is, because the compositor diverts the pointer at the barrier.
            *at_edge = true;
            tracing::info!(barrier_id, "the pointer reached the edge; telling core");
        }
        Event::PortalGrant(grant) => {
            // Core owns `portal-restore.toml`; the agent only hands the token over.
            tracing::info!(
                interface = grant.portal_interface.as_str(),
                "reporting a fresh portal restore token to core"
            );
            link.send(Msg::PortalGrant(PortalGrantChanged {
                grant: Some(grant),
                revoked: false,
            }))
            .await?;
        }
        Event::Emergency => {
            // Rule 3: agent restores local control itself, then tells core.
            tracing::warn!("emergency combination: releasing local capture");
            release_everything(backend).await;
            link.send(Msg::Emergency(EmergencyFired {
                lease_id: held_lease.take().unwrap_or_default(),
            }))
            .await?;
        }
        Event::QueueOverflow => {
            // Rule 2: the agent ends capture itself and reports the overflow.
            tracing::error!("capture queue overflow; ending capture");
            let _ = backend.end_capture(false).await;
            send_error(link, "QUEUE_OVERFLOW", "capture queue overflowed").await?;
        }
        Event::Failed { code, detail } => {
            tracing::error!(
                code,
                detail = detail.as_str(),
                "capture failed; ending capture"
            );
            let _ = backend.end_capture(false).await;
            send_error(link, code, &detail).await?;
        }
    }
    Ok(())
}

/// Capture and ledger both go, in that order (rule 8 for lock/suspend, rule 10 for
/// portal sessions).
async fn release_everything(backend: &mut Backend) {
    if let Err(detail) = backend.end_capture(true).await {
        tracing::warn!(detail = detail.as_str(), "EndCapture failed");
    }
    let released = backend.release_session().await;
    tracing::info!(
        keys = released.keys,
        buttons = released.buttons,
        "session-owned state released"
    );
}

async fn send_error<S: AsyncRead + AsyncWrite + Unpin>(
    link: &mut AgentLink<S>,
    code: &str,
    detail: &str,
) -> anyhow::Result<()> {
    link.send(Msg::Error(AgentError {
        code: code.to_owned(),
        detail: detail.to_owned(),
    }))
    .await?;
    Ok(())
}

fn now_ms(started: Instant) -> u64 {
    started.elapsed().as_millis() as u64
}

/// The event's own monotonic timestamp for `LocalEvent.monotonic_us`.
///
/// Diagnostic only (`transport::convert` sets it to 0 on the wire and nothing may
/// derive timing from it), so a process-relative clock is enough.
fn monotonic_us() -> u64 {
    static START: std::sync::LazyLock<Instant> = std::sync::LazyLock::new(Instant::now);
    START.elapsed().as_micros() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::BackendKindArg;
    use tokio::io::duplex;

    const HOTKEY: &str = "Ctrl+Alt+Shift+F12";

    fn lease(byte: u8) -> Vec<u8> {
        vec![byte; 16]
    }

    /// One agent session over an in-memory link and the dry backend.
    struct Harness {
        agent: AgentLink<tokio::io::DuplexStream>,
        core: AgentLink<tokio::io::DuplexStream>,
        backend: Backend,
        ready: backend::Ready,
        deadline: Option<(Instant, Vec<u8>)>,
        self_timeout: Option<(Instant, Vec<u8>)>,
        held: Option<Vec<u8>>,
        batch: Vec<LocalEvent>,
        at_edge: bool,
        displays: crate::displays::DisplaysState,
    }

    impl Harness {
        async fn new() -> Self {
            let (agent_io, core_io) = duplex(8 * 1024);
            let (backend, ready, _refusals) = Backend::select(
                BackendKindArg(::protocol::ipc::BackendKind::Auto as i32),
                true,
                0,
                false,
                HOTKEY,
                &[],
            )
            .await;
            Self {
                agent: AgentLink::new(agent_io, FRAME_MAJOR, FRAME_MINOR),
                core: AgentLink::new(core_io, FRAME_MAJOR, FRAME_MINOR),
                backend,
                ready,
                deadline: None,
                self_timeout: None,
                held: None,
                batch: Vec::new(),
                at_edge: false,
                displays: crate::displays::DisplaysState::default(),
            }
        }

        async fn send(&mut self, message: Msg) {
            handle_message(
                &mut self.agent,
                &mut self.backend,
                &self.ready,
                &message,
                &mut self.deadline,
                &mut self.self_timeout,
                &mut self.held,
                &mut self.batch,
                &mut self.displays,
            )
            .await
            .expect("a valid frame must not fail the handler");
        }

        async fn recv(&mut self) -> Msg {
            self.core
                .recv()
                .await
                .expect("the agent must answer")
                .msg
                .expect("an answered frame carries a payload")
        }

        async fn event(&mut self, event: Event) {
            handle_backend_event(
                &mut self.agent,
                &mut self.backend,
                event,
                &mut self.held,
                &mut self.batch,
                &mut self.at_edge,
            )
            .await
            .expect("a backend event must not fail the handler");
        }
    }

    fn before(ms: u64) -> Instant {
        Instant::now() + Duration::from_millis(ms)
    }

    #[tokio::test]
    async fn a_temporary_capture_arms_the_self_timeout_and_a_remote_capture_clears_it() {
        let mut harness = Harness::new().await;

        harness
            .send(Msg::BeginCapture(ipc::BeginCapture {
                kind: ::protocol::ipc::CaptureKind::Temporary as i32,
                lease_id: lease(7),
                self_timeout_ms: 500,
            }))
            .await;
        match harness.recv().await {
            Msg::CaptureReady(ready) => {
                assert_eq!(ready.lease_id, lease(7));
                assert_eq!(ready.kind, ::protocol::ipc::CaptureKind::Temporary as i32);
            }
            other => panic!("expected CaptureReady, got {other:?}"),
        }
        assert_eq!(
            harness.held,
            Some(lease(7)),
            "the lease is held while capturing"
        );
        let (due, lease_id) = harness
            .self_timeout
            .clone()
            .expect("rule 5 arms a self-timeout");
        assert_eq!(lease_id, lease(7));
        assert!(
            due <= before(500) && due > before(400),
            "the deadline is ~500 ms away: {due:?}"
        );

        // The bound of rule 4/`ipc.proto`: the agent never arms more than 1000 ms.
        harness
            .send(Msg::BeginCapture(ipc::BeginCapture {
                kind: ::protocol::ipc::CaptureKind::Temporary as i32,
                lease_id: lease(8),
                self_timeout_ms: 60_000,
            }))
            .await;
        harness.recv().await;
        let (due, _) = harness.self_timeout.clone().expect("armed");
        assert!(
            due <= before(1000),
            "a self-timeout is clamped to 1000 ms: {due:?}"
        );

        // REMOTE has no self-timeout: it ends only when core says so.
        harness
            .send(Msg::BeginCapture(ipc::BeginCapture {
                kind: ::protocol::ipc::CaptureKind::Remote as i32,
                lease_id: lease(9),
                self_timeout_ms: 9000,
            }))
            .await;
        harness.recv().await;
        assert!(
            harness.self_timeout.is_none(),
            "a remote capture is not temporary"
        );
        assert_eq!(harness.held, Some(lease(9)));
    }

    #[tokio::test]
    async fn end_capture_releases_the_lease_and_clears_the_self_timeout_but_not_the_deadline() {
        let mut harness = Harness::new().await;
        harness
            .send(Msg::BeginCapture(ipc::BeginCapture {
                kind: ::protocol::ipc::CaptureKind::Temporary as i32,
                lease_id: lease(1),
                self_timeout_ms: 500,
            }))
            .await;
        harness.recv().await;
        harness
            .send(Msg::ArmDeadline(ipc::ArmDeadline {
                lease_id: lease(1),
                remaining_ms: 900,
            }))
            .await;
        assert!(harness.deadline.is_some());

        harness
            .send(Msg::EndCapture(ipc::EndCapture {
                lease_id: lease(1),
                replay_buffered_locally: true,
            }))
            .await;
        match harness.recv().await {
            Msg::Released(released) => assert_eq!(released.lease_id, lease(1)),
            other => panic!("expected Released, got {other:?}"),
        }
        assert!(harness.self_timeout.is_none(), "capture is over");
        assert!(harness.held.is_none(), "the lease is no longer held");
        assert!(
            harness.deadline.is_some(),
            "only ReleaseSession clears the deadline"
        );
    }

    #[tokio::test]
    async fn arm_deadline_uses_the_agents_own_clock_within_the_protocol_bound() {
        let mut harness = Harness::new().await;
        harness
            .send(Msg::ArmDeadline(ipc::ArmDeadline {
                lease_id: lease(3),
                remaining_ms: 750,
            }))
            .await;
        let (due, lease_id) = harness.deadline.clone().expect("rule 4 arms the deadline");
        assert_eq!(lease_id, lease(3), "the deadline belongs to the lease");
        assert!(due > before(700) && due <= before(750), "{due:?}");

        // `ArmDeadline.remaining_ms` is 1..=1000, so a longer value is clamped, not trusted.
        harness
            .send(Msg::ArmDeadline(ipc::ArmDeadline {
                lease_id: lease(3),
                remaining_ms: 60_000,
            }))
            .await;
        let (due, _) = harness.deadline.clone().expect("re-armed");
        assert!(due <= before(1000), "clamped to 1000 ms: {due:?}");

        harness
            .send(Msg::ArmDeadline(ipc::ArmDeadline {
                lease_id: lease(3),
                remaining_ms: 0,
            }))
            .await;
        let (due, _) = harness.deadline.clone().expect("armed at the floor");
        assert!(
            due <= before(1) && due >= Instant::now(),
            "the floor is 1 ms: {due:?}"
        );
    }

    #[tokio::test]
    async fn release_session_clears_both_clocks_and_the_lease() {
        let mut harness = Harness::new().await;
        harness
            .send(Msg::BeginCapture(ipc::BeginCapture {
                kind: ::protocol::ipc::CaptureKind::Temporary as i32,
                lease_id: lease(4),
                self_timeout_ms: 400,
            }))
            .await;
        harness.recv().await;
        harness
            .send(Msg::ArmDeadline(ipc::ArmDeadline {
                lease_id: lease(4),
                remaining_ms: 400,
            }))
            .await;

        harness
            .send(Msg::ReleaseSession(ipc::ReleaseSession {
                lease_id: lease(4),
            }))
            .await;
        match harness.recv().await {
            Msg::Released(released) => assert_eq!(released.lease_id, lease(4)),
            other => panic!("expected Released, got {other:?}"),
        }
        assert!(harness.deadline.is_none());
        assert!(harness.self_timeout.is_none());
        assert!(harness.held.is_none());
    }

    #[tokio::test]
    async fn an_emergency_releases_the_capture_and_reports_the_held_lease() {
        let mut harness = Harness::new().await;
        harness
            .send(Msg::BeginCapture(ipc::BeginCapture {
                kind: ::protocol::ipc::CaptureKind::Remote as i32,
                lease_id: lease(5),
                self_timeout_ms: 0,
            }))
            .await;
        harness.recv().await;

        harness.event(Event::Emergency).await;
        match harness.recv().await {
            Msg::Emergency(fired) => assert_eq!(fired.lease_id, lease(5)),
            other => panic!("expected EmergencyFired, got {other:?}"),
        }
        assert!(
            harness.held.is_none(),
            "local control is restored first (rule 3)"
        );
    }

    #[tokio::test]
    async fn a_queue_overflow_ends_the_capture_and_is_reported() {
        let mut harness = Harness::new().await;
        harness
            .send(Msg::BeginCapture(ipc::BeginCapture {
                kind: ::protocol::ipc::CaptureKind::Remote as i32,
                lease_id: lease(6),
                self_timeout_ms: 0,
            }))
            .await;
        harness.recv().await;

        harness.event(Event::QueueOverflow).await;
        match harness.recv().await {
            Msg::Error(error) => {
                assert_eq!(error.code, "QUEUE_OVERFLOW", "rule 2 names the overflow");
                assert!(!error.detail.is_empty());
            }
            other => panic!("expected an AgentError, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_capture_failure_is_reported_as_capture_failed() {
        let mut harness = Harness::new().await;
        harness
            .event(Event::Failed {
                code: "CAPTURE_FAILED",
                detail: "no X connection".to_owned(),
            })
            .await;
        match harness.recv().await {
            Msg::Error(error) => assert_eq!(error.code, "CAPTURE_FAILED"),
            other => panic!("expected an AgentError, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_ping_is_answered_with_the_same_nonce() {
        let mut harness = Harness::new().await;
        harness.send(Msg::Ping(ipc::Ping { nonce: 42 })).await;
        match harness.recv().await {
            Msg::Pong(pong) => assert_eq!(pong.nonce, 42),
            other => panic!("expected Pong, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_second_attach_and_an_unknown_capture_kind_are_protocol_errors() {
        let mut harness = Harness::new().await;
        harness
            .send(Msg::Attach(ipc::AgentAttach {
                launch_token: vec![0u8; 32],
                mode: AgentMode::Normal as i32,
                emergency_hotkey: HOTKEY.to_owned(),
                backend_override: ::protocol::ipc::BackendKind::Auto as i32,
                restore_tokens: Vec::new(),
                require_emergency_hotkey: true,
                core_silence_ms: 300,
            }))
            .await;
        match harness.recv().await {
            Msg::Error(error) => assert_eq!(error.code, "PROTOCOL"),
            other => panic!("expected an AgentError, got {other:?}"),
        }

        harness
            .send(Msg::BeginCapture(ipc::BeginCapture {
                kind: 0,
                lease_id: lease(1),
                self_timeout_ms: 100,
            }))
            .await;
        match harness.recv().await {
            Msg::Error(error) => assert_eq!(error.code, "PROTOCOL"),
            other => panic!("expected an AgentError, got {other:?}"),
        }
    }

    #[test]
    fn recovery_allows_only_the_release_and_the_shutdown() {
        let recovery = AgentMode::Recovery as i32;
        let normal = AgentMode::Normal as i32;

        for allowed in [
            Msg::ReleaseKeys(ipc::ReleaseKeys {
                keys: Vec::new(),
                buttons: Vec::new(),
            }),
            Msg::Shutdown(ipc::AgentShutdown {}),
        ] {
            assert!(
                ::ipc::agent::mode_allows(recovery, &allowed),
                "{allowed:?} is part of the RECOVERY allow-list (rule 7)"
            );
        }

        let refused = [
            Msg::Attach(ipc::AgentAttach::default()),
            Msg::BeginCapture(ipc::BeginCapture::default()),
            Msg::EndCapture(ipc::EndCapture::default()),
            Msg::PrepareInject(ipc::PrepareInject::default()),
            Msg::Inject(ipc::Inject::default()),
            Msg::ArmDeadline(ipc::ArmDeadline::default()),
            Msg::ReleaseSession(ipc::ReleaseSession::default()),
            Msg::Ping(ipc::Ping::default()),
            Msg::QueryDisplays(ipc::QueryDisplays {}),
            Msg::ForgetPortalGrants(ipc::ForgetPortalGrants {}),
        ];
        for message in refused {
            assert!(
                !::ipc::agent::mode_allows(recovery, &message),
                "{message:?} must be refused in RECOVERY"
            );
            assert!(
                ::ipc::agent::mode_allows(normal, &message),
                "{message:?} is allowed in NORMAL"
            );
        }
    }
}
