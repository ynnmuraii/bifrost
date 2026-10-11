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
    EmergencyFired, InjectResult, LocalEvent, Pong, Released,
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

    let (mut backend, ready) = Backend::select(
        backend::BackendKindArg(attach.backend_override),
        args.dry_inject,
        args.dry_input_events,
        attach.emergency_hotkey.as_str(),
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
    let mut released_after_silence = false;

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
                    &mut held_lease, &mut batch,
                ).await? {
                    return Ok(stop);
                }
            }
            event = backend.next_event() => {
                let now = now_ms(started);
                if let Some(event) = event {
                    handle_backend_event(link, backend, event, &mut held_lease, &mut batch).await?;
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
        // coalesce motion on the far side.
        if !batch.is_empty() {
            let events = std::mem::take(&mut batch);
            link.send(Msg::Captured(CapturedBatch {
                events,
                at_edge: false,
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
            // `enumerate_displays` does not exist yet in the platform crates
            // (`docs/06-development-plan.md` KVM-040). An empty report would mean
            // "this node has no screens", so it is an explicit refusal instead.
            send_error(
                link,
                "DISPLAYS_UNAVAILABLE",
                "display enumeration is KVM-040; this slice reports nothing",
            )
            .await?;
        }
        Msg::ForgetPortalGrants(_) => {
            tracing::info!("ForgetPortalGrants: this build holds no portal grants");
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
async fn handle_backend_event<S: AsyncRead + AsyncWrite + Unpin>(
    link: &mut AgentLink<S>,
    backend: &mut Backend,
    event: Event,
    held_lease: &mut Option<Vec<u8>>,
    batch: &mut Vec<LocalEvent>,
) -> anyhow::Result<()> {
    match event {
        Event::Input(input) => match crate::events::to_local_event(&input, monotonic_us()) {
            Some(event) => batch.push(event),
            None => tracing::trace!("event has no wire form and was dropped"),
        },
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
        Event::Failed(detail) => {
            tracing::error!(detail = detail.as_str(), "capture failed; ending capture");
            let _ = backend.end_capture(false).await;
            send_error(link, "CAPTURE_FAILED", &detail).await?;
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
