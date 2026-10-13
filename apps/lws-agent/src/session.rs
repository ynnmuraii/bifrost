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

use std::future::Future;
use std::time::{Duration, Instant};

use ::ipc::{AgentLink, Watchdog};
use protocol::ipc::agent_frame::Msg;
use protocol::ipc::{
    self, AgentError, AgentFrame, AgentMode, CaptureReady, CapturedBatch, DeadlineExpired,
    EmergencyFired, InjectResult, LocalEvent, Pong, PortalGrantChanged, Released,
};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::backend::{self, Backend, ClipboardChange, Event, Injected};
use crate::clipboard_wire;
use crate::launch;
use crate::{Args, FRAME_MAJOR, FRAME_MINOR};

/// How often the session asks the backend whether the local clipboard changed.
///
/// The backend poll never blocks, so this interval is the only pacing there is: 20 ms is short
/// enough that a copy reaches core well inside the input path's own budget (the prepare window
/// is 200 ms) and long enough that the clipboard's notification queue, its `stat` and its
/// framing are not serviced on the 5 ms input tick.
const CLIPBOARD_POLL: Duration = Duration::from_millis(20);

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
    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
    let backend_selection = Backend::select(
        backend::BackendKindArg(attach.backend_override),
        args.dry_inject,
        args.dry_input_events,
        args.dry_displays,
        args.dry_motion,
        args.dry_clipboard_dir.clone(),
        attach.emergency_hotkey.as_str(),
        attach.restore_tokens.as_slice(),
        attach.allow_portal_sessions && mode != AgentMode::Recovery as i32,
        progress_tx,
        ipc::TakeoverDetection::try_from(attach.takeover_detection)
            .unwrap_or(ipc::TakeoverDetection::Unspecified),
        attach.takeover_threshold_px,
    );
    let selected = complete_backend_startup(&mut link, &mut progress_rx, backend_selection).await?;
    let Some((mut backend, ready, refusals)) = selected else {
        return Ok(());
    };
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

    // Rule 6's clock starts when the agent is ready. Before `AgentReady` the agent has
    // no capture or injection capability, and `complete_backend_startup` continues to
    // answer core heartbeats while the user responds to portal prompts.
    let mut watchdog = watchdog_at_ready(attach.core_silence_ms, now_ms(started));

    let stop = loop_forever(
        &mut link,
        &mut backend,
        &ready,
        mode,
        attach.repeat_mode,
        &mut watchdog,
        started,
        args.dev_capture_injected,
    )
    .await?;

    // Both endings leave nothing of this session behind (rules 4, 6, 10; S2).
    if let Stop::LostCore = stop {
        // The loop is over, so nothing can inject any more; the state still goes with it.
        release_everything(&mut backend, &mut None, &mut None, &mut None, &mut None).await;
    }
    tracing::info!("agent exiting");
    Ok(())
}

/// Keeps the IPC heartbeat responsive while portal sessions are opening or restoring.
async fn complete_backend_startup<S, F, T>(
    link: &mut AgentLink<S>,
    progress_rx: &mut tokio::sync::mpsc::UnboundedReceiver<Msg>,
    backend: F,
) -> anyhow::Result<Option<T>>
where
    S: AsyncRead + AsyncWrite + Unpin,
    F: Future<Output = T>,
{
    enum Startup<T> {
        Backend(T),
        Progress(Option<Msg>),
        Frame(Result<AgentFrame, ::ipc::IpcError>),
    }
    tokio::pin!(backend);
    loop {
        let startup = tokio::select! {
            selected = &mut backend => Startup::Backend(selected),
            progress = progress_rx.recv() => Startup::Progress(progress),
            frame = link.recv() => Startup::Frame(frame),
        };
        match startup {
            Startup::Backend(selected) => return Ok(Some(selected)),
            Startup::Progress(Some(progress)) => {
                link.send(progress).await?;
            }
            Startup::Progress(None) => continue,
            Startup::Frame(Ok(AgentFrame {
                msg: Some(Msg::Ping(ping)),
                ..
            })) => link.send(Msg::Pong(Pong { nonce: ping.nonce })).await?,
            Startup::Frame(Ok(AgentFrame {
                msg: Some(Msg::Shutdown(_)),
                ..
            })) => return Ok(None),
            Startup::Frame(Ok(_)) => {
                send_error(link, "STARTUP_ONLY", "agent is initializing its backend").await?;
            }
            Startup::Frame(Err(error)) => {
                tracing::warn!(%error, "core IPC ended during backend initialization");
                return Ok(None);
            }
        }
    }
}

/// The loop in one place so every exit path releases through the same code.
#[allow(clippy::too_many_arguments)]
async fn loop_forever<S: AsyncRead + AsyncWrite + Unpin>(
    link: &mut AgentLink<S>,
    backend: &mut Backend,
    ready: &backend::Ready,
    mode: i32,
    repeat_mode: i32,
    watchdog: &mut Watchdog,
    started: Instant,
    dev_capture_injected: bool,
) -> anyhow::Result<Stop> {
    // `ArmDeadline` arms this; rule 4 fixes the bound at 1000 ms.
    let mut deadline: Option<(Instant, Vec<u8>)> = None;
    // `BeginCapture(TEMPORARY)` self-timeout (rule 5).
    let mut self_timeout: Option<(Instant, Vec<u8>)> = None;
    // A lease the backend is currently holding state for.
    let mut held_lease: Option<Vec<u8>> = None;
    // Captured events waiting for the next tick, so one tick is one batch.
    let mut batch: Vec<LocalEvent> = Vec::new();
    // KVM-036: with `ReceiverSoftware` this agent repeats a held key itself, at the OS rate
    // (Windows cannot do it natively for injected keys — measured); with `ReceiverNative` the
    // platform repeats it and the guard below releases the keys instead. `None` means the
    // software engine is not running, which is exactly the native case.
    let repeat_mode =
        ipc::RepeatMode::try_from(repeat_mode).unwrap_or(ipc::RepeatMode::ReceiverSoftware);
    let mut repeat = match repeat_mode {
        ipc::RepeatMode::ReceiverNative => None,
        _ => {
            let timing = backend.repeat_timing();
            tracing::info!(
                repeat_mode = repeat_mode.as_str_name(),
                delay_ms = timing.delay_ms,
                interval_ms = timing.interval_ms,
                "software autorepeat engine armed"
            );
            Some(crate::repeat::SoftwareRepeat::new(timing))
        }
    };
    // `CapturedBatch.at_edge`: set when the backend reports an edge crossing, cleared
    // once core has been told. The batch is sent even when it carries no events,
    // because the crossing itself is the news.
    let mut at_edge = false;
    let mut activation = None;
    let mut last_inject_log = None;
    let mut last_capture_batch_log = None;
    let mut released_after_silence = false;
    // The node's own display set and its revision (`Hello.displays_revision`).
    let mut displays = crate::displays::DisplaysState::default();
    // The clipboard: the versions this node speaks, and the transfer it is assembling.
    let mut clipboard = ClipboardState::new();
    let mut clipboard_tick = tokio::time::interval(CLIPBOARD_POLL);
    // A slow tick must not queue up: the clipboard is polled, not scheduled.
    clipboard_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            // The IPC arm first: a `Ping` that is already in the link's buffer is
            // answered before this turn services any deadline, and a turn that woke on
            // a frame services the time-services with the clock that frame just reset.
            // Biased for liveness — the heartbeat rides this arm (rule 11: the agent
            // «отвечает `Pong`»), and a busy poll loop must not delay it. The tick
            // services below run once per turn whatever arm won, so the bias cannot
            // starve the deadline (rule 4) either.
            biased;
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
                    &mut held_lease, &mut batch, &mut displays, &mut repeat,
                    &mut clipboard, dev_capture_injected, &mut last_inject_log, now,
                ).await? {
                    return Ok(stop);
                }
            }
            // ADR-008, agent side: a local copy is *polled*, never waited for, so the
            // deadline and the core-silence watchdog keep running on their own tick
            // while this branch asks the platform whether the user copied something.
            _ = clipboard_tick.tick() => {
                if let Some(change) = backend.clipboard_change().await {
                    send_clipboard_change(link, &mut clipboard, change).await?;
                }
            }
            event = backend.next_event() => {
                let now = now_ms(started);
                // KVM-036: one repeat per interval, injected like any other key event, so it
                // travels the same ledger and the same cleanup as the original press.
                if let Some(engine) = repeat.as_mut() {
                    if let Some(key) = engine.due(now) {
                        let events = [input_model::InputEvent::Key {
                            key,
                            state: input_model::KeyState::Down,
                        }];
                        let injected = backend.inject(&events).await;
                        tracing::debug!(
                            ?key,
                            applied = injected.applied,
                            repeats = engine.repeats(),
                            "software repeat"
                        );
                    }
                }
                if let Some(event) = event {
                    handle_backend_event(
                        link,
                        backend,
                        &mut repeat,
                        &mut deadline,
                        &mut self_timeout,
                        event,
                        &mut held_lease,
                        &mut batch,
                        &mut at_edge,
                        &mut activation,
                    )
                    .await?;
                }
            }
        }

        service_timers(
            link,
            backend,
            &mut repeat,
            &mut deadline,
            &mut self_timeout,
            &mut held_lease,
            &mut released_after_silence,
            watchdog,
            now_ms(started),
        )
        .await?;

        // One tick, one batch: keeps the frame rate bounded and lets the transport
        // coalesce motion on the far side. A tick that only learned about an edge
        // crossing still goes out, with no events.
        flush_captured(
            link,
            &mut batch,
            &mut at_edge,
            &mut activation,
            &mut last_capture_batch_log,
            now_ms(started),
        )
        .await?;
    }
}

/// The time-services of one turn: the deadline of rule 4, the `TEMPORARY` self-timeout of
/// rule 5, and the core-silence watchdog of rule 6.
///
/// They live here, once per turn, rather than inside the poll arm: a turn that woke on an
/// IPC frame must service the deadline as surely as a turn that woke on the 5 ms tick,
/// and rule 6 must only judge a clock its own turn just reset.
#[allow(clippy::too_many_arguments)]
async fn service_timers<S: AsyncRead + AsyncWrite + Unpin>(
    link: &mut AgentLink<S>,
    backend: &mut Backend,
    repeat: &mut Option<crate::repeat::SoftwareRepeat>,
    deadline: &mut Option<(Instant, Vec<u8>)>,
    self_timeout: &mut Option<(Instant, Vec<u8>)>,
    held_lease: &mut Option<Vec<u8>>,
    released_after_silence: &mut bool,
    watchdog: &mut Watchdog,
    now: u64,
) -> anyhow::Result<()> {
    // Rule 4: the deadline is the agent's own monotonic clock.
    if let Some((due, lease_id)) = deadline.as_ref() {
        if Instant::now() >= *due {
            let lease_id = lease_id.clone();
            *deadline = None;
            tracing::warn!("deadline expired; releasing the session");
            // The lease is over: the repeat engine, the deadlines and the held lease all go
            // with it, or the engine keeps injecting a key that was just released (batch-3
            // audit, F12).
            forget_lease_state(repeat, deadline, self_timeout, held_lease);
            let released = backend.release_session().await;
            link.send(Msg::DeadlineExpired(DeadlineExpired { lease_id }))
                .await?;
            tracing::info!(keys = released.keys, buttons = released.buttons, "released");
        }
    }
    // Rule 5: a TEMPORARY capture ends by itself and the buffered edge
    // is played locally rather than reaching the wire.
    if let Some((due, lease_id)) = self_timeout.as_ref() {
        if Instant::now() >= *due {
            let lease_id = lease_id.clone();
            *self_timeout = None;
            tracing::warn!("self-timeout of a temporary capture");
            forget_capture_state(repeat, self_timeout);
            let _ = backend.end_capture(true).await;
            send_error(
                link,
                "CAPTURE_FAILED",
                "temporary capture self-timeout expired",
            )
            .await?;
            let _ = lease_id;
            *held_lease = None;
        }
    }
    // Rule 6: core went quiet. No frame, not even a Ping.
    if watchdog.silence_exceeded(now) && !*released_after_silence {
        *released_after_silence = true;
        tracing::warn!(
            silence_ms = watchdog.silence_ms(now),
            "core silent past the limit; releasing locally"
        );
        release_everything(backend, repeat, deadline, self_timeout, held_lease).await;
    }
    Ok(())
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
    repeat: &mut Option<crate::repeat::SoftwareRepeat>,
    clipboard: &mut ClipboardState,
    dev_capture_injected: bool,
    last_inject_log: &mut Option<u64>,
    now: u64,
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
            match backend
                .begin_capture(kind, dev_capture_injected && begin.dev_capture_injected)
                .await
            {
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
            // An empty lease rejects only a portal activation on a receiver;
            // its injection lease and repeat engine must continue unchanged.
            let capture_only = end.lease_id.is_empty();
            if !capture_only {
                forget_capture_state(repeat, self_timeout);
            }
            match backend.end_capture(end.replay_buffered_locally).await {
                Ok(released) => {
                    if !capture_only {
                        *held_lease = None;
                    }
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
            // The engine follows what actually went to the OS: the most recent held key is
            // the one the OS would repeat.
            if let Some(engine) = repeat.as_mut() {
                engine.applied(&events, now);
            }
            batch.clear();
            let report = last_inject_log.is_none_or(|last| now.saturating_sub(last) >= 1_000);
            if report {
                *last_inject_log = Some(now);
                let mut keys = 0;
                let mut pointers = 0;
                let mut buttons = 0;
                let mut scrolls = 0;
                for event in &events {
                    match event {
                        input_model::InputEvent::Key { .. } => keys += 1,
                        input_model::InputEvent::PointerAbsolute(_)
                        | input_model::InputEvent::PointerRelative { .. } => pointers += 1,
                        input_model::InputEvent::Button { .. } => buttons += 1,
                        input_model::InputEvent::Scroll { .. } => scrolls += 1,
                    }
                }
                tracing::info!(
                    lease_id = ?inject.lease_id,
                    input_batch_seq = inject.input_batch_seq,
                    events = events.len(),
                    keys,
                    pointers,
                    buttons,
                    scrolls,
                    accepted = injected.applied,
                    os_error = ?injected.os_error,
                    actual_position = ?injected.actual,
                    "agent InjectResult"
                );
            }
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
        Msg::StopRepeat(stop) => {
            // AC-14 (`docs/03-architecture.md`, «Autorepeat при обрыве связи»): no input and
            // no renew for `repeat_guard_ms`. Software repeat stops its engine; native repeat
            // lets the held keys up, because the OS or the application owns that repeat and
            // releasing is the only way to stop it. The lease itself stays alive: a late
            // `LeaseRenew` may still arrive.
            // The branch is the *mode*, not whether a key happened to be repeating: with
            // software repeat there is no OS repeat to release, and with native repeat there
            // is nothing else that could stop it.
            match repeat.as_mut() {
                Some(engine) => {
                    let stopped = engine.stop();
                    tracing::warn!(
                        key = ?stopped,
                        repeats = engine.repeats(),
                        "repeat guard: software repeat stopped"
                    );
                }
                None => {
                    let released = backend.release_session().await;
                    tracing::warn!(
                        keys = released.keys,
                        buttons = released.buttons,
                        lease = stop.lease_id.len(),
                        "repeat guard: held keys released (native repeat)"
                    );
                }
            }
        }
        Msg::ReleaseSession(release) => {
            forget_lease_state(repeat, deadline, self_timeout, held_lease);
            let released = backend.release_session().await;
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
            let current = backend.displays().await;
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
        // ADR-008: the local selection is this agent's to own, and core hands over a payload
        // a peer sent (or the release of one). It is applied only once every chunk arrived and
        // the whole payload passed its sha256 and the UTF-8 check.
        Msg::SetClipboardText(set) => {
            handle_set_clipboard(link, backend, clipboard, set).await?;
        }
        Msg::Shutdown(_) => {
            tracing::info!("shutdown requested by core");
            release_everything(backend, repeat, deadline, self_timeout, held_lease).await;
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

/// The agent's clipboard state: the versions it speaks, and the transfer it is assembling.
///
/// ADR-008 asks for origin/version/hash so that an echo cannot become a loop. Core owns the
/// fan-out and its own echo history; this is the agent's half of the rule, one hop earlier:
/// what core sent is applied with core's `origin_version` remembered, and the *next* change
/// this node reports is numbered strictly above it.
struct ClipboardState {
    /// The `origin_version` the next `ClipboardChanged` carries.
    own_version: u64,
    /// The `origin_version` core last asked this node to apply.
    applied_version: u64,
    /// The inbound transfer being assembled, if any. Only one at a time, per channel.
    inbound: Option<clipboard_wire::Inbound>,
}

impl ClipboardState {
    fn new() -> Self {
        ClipboardState {
            own_version: 0,
            applied_version: 0,
            inbound: None,
        }
    }

    /// Records a version this node applied, so a later local change is numbered above it.
    fn applied(&mut self, origin_version: u64) {
        self.applied_version = origin_version;
        self.own_version = self.own_version.max(origin_version);
    }

    /// The version for the next change this node reports: strictly greater than every version
    /// applied here, so core can never mistake it for the payload it just sent.
    fn next_version(&mut self) -> u64 {
        self.own_version = self.own_version.max(self.applied_version) + 1;
        self.own_version
    }
}

/// Applies one `SetClipboardText` frame from core.
///
/// The chunks are assembled by `origin_version`, and the selection is set only after the last
/// chunk's `sha256` covers the whole payload *and* the bytes are valid UTF-8
/// (`docs/05-protocol.md`: «проверка sha256 и UTF-8 до установки selection»). Every refusal is
/// an `AgentError` with a `CLIPBOARD_*` code, and a refusal never touches the selection:
/// unverified text is never applied. `chunk_count == 0` releases the selection and forgets the
/// text.
async fn handle_set_clipboard<S: AsyncRead + AsyncWrite + Unpin>(
    link: &mut AgentLink<S>,
    backend: &mut Backend,
    state: &mut ClipboardState,
    set: &ipc::SetClipboardText,
) -> anyhow::Result<()> {
    if !backend.clipboard_available() {
        // ADR-008: the clipboard is opt-in on both sides and separate from the KVM, so a node
        // that cannot own a selection says so instead of dropping the frame silently.
        state.inbound = None;
        send_error(
            link,
            "CLIPBOARD_DISABLED",
            "this session has no local clipboard (the clipboard is a separate, opt-in feature)",
        )
        .await?;
        return Ok(());
    }
    if set.chunk_count == 0 {
        state.inbound = None;
        match backend.set_clipboard(None).await {
            Ok(()) => {
                state.applied(set.origin_version);
                tracing::info!(version = set.origin_version, "clipboard released by core");
            }
            Err(detail) => send_error(link, "CLIPBOARD_APPLY_FAILED", &detail).await?,
        }
        return Ok(());
    }

    // "Keyed by origin_version": a frame for another version opens a new transfer and drops
    // whatever was half-assembled for the old one. A frame for the *same* version that
    // disagrees about anything else is the protocol error `push` reports.
    let opens = state
        .inbound
        .as_ref()
        .map(clipboard_wire::Inbound::origin_version)
        != Some(set.origin_version);
    if opens {
        match clipboard_wire::Inbound::start(set) {
            Ok(inbound) => state.inbound = Some(inbound),
            Err(error) => {
                state.inbound = None;
                send_error(link, error.code(), error.detail()).await?;
                return Ok(());
            }
        }
    }
    let inbound = state.inbound.as_mut().expect("opened or kept above");
    match inbound.push(set) {
        Err(error) => {
            state.inbound = None;
            send_error(link, error.code(), error.detail()).await?;
        }
        Ok(clipboard_wire::Progress::NeedMore) => {}
        Ok(clipboard_wire::Progress::Ready) => {
            let inbound = state.inbound.take().expect("checked above");
            match inbound.finish(set) {
                Err(error) => send_error(link, error.code(), error.detail()).await?,
                Ok(text) => match backend.set_clipboard(Some(&text)).await {
                    Ok(()) => {
                        tracing::info!(
                            version = set.origin_version,
                            bytes = text.len(),
                            "clipboard applied from core"
                        );
                        state.applied(set.origin_version);
                    }
                    Err(detail) => send_error(link, "CLIPBOARD_APPLY_FAILED", &detail).await?,
                },
            }
        }
    }
    Ok(())
}

/// Reports one local clipboard change to core, in frames that fit an IPC frame.
///
/// `size_bytes` and the markers travel on every chunk and the `sha256` on the last one, which
/// is what core's assembly verifies before it offers anything to a peer. The text is never
/// logged — only its size, the chunk count and how many markers came with it.
async fn send_clipboard_change<S: AsyncRead + AsyncWrite + Unpin>(
    link: &mut AgentLink<S>,
    state: &mut ClipboardState,
    change: ClipboardChange,
) -> anyhow::Result<()> {
    let bytes = change.text.len();
    // The 1 MiB limit is *core's* policy — it is the side that can answer `CLIPBOARD_TOO_LARGE`
    // and that knows `[clipboard].max_bytes` — so a copy over it is still reported. What the
    // agent owns is the framing: `MAX_CHUNKS` frames of `CHUNK_BYTES` each, beyond which the
    // payload has no wire form at all and is dropped rather than sent truncated.
    let ceiling = clipboard_wire::MAX_CHUNKS as usize * clipboard_wire::CHUNK_BYTES;
    if bytes > ceiling {
        tracing::warn!(
            bytes,
            limit = ceiling,
            "clipboard payload beyond the IPC framing ceiling; not offering it"
        );
        return Ok(());
    }
    let version = state.next_version();
    let frames = clipboard_wire::frames(&change.text, version, &change.markers);
    tracing::info!(
        version,
        bytes,
        chunks = frames.len(),
        markers = change.markers.len(),
        "offering a local clipboard change"
    );
    for frame in frames {
        link.send(Msg::ClipboardChanged(frame)).await?;
    }
    Ok(())
}

async fn flush_captured<S: AsyncRead + AsyncWrite + Unpin>(
    link: &mut AgentLink<S>,
    batch: &mut Vec<LocalEvent>,
    at_edge: &mut bool,
    activation: &mut Option<ipc::PortalActivation>,
    last_log: &mut Option<u64>,
    now: u64,
) -> anyhow::Result<()> {
    if batch.is_empty() && !*at_edge {
        return Ok(());
    }
    let events = std::mem::take(batch);
    let edge = std::mem::take(at_edge);
    if last_log.is_none_or(|last| now.saturating_sub(last) >= 1_000) {
        *last_log = Some(now);
        let mut keys = 0;
        let mut pointers = 0;
        let mut buttons = 0;
        let mut scrolls = 0;
        for event in &events {
            match backend::to_input_event(event) {
                Some(input_model::InputEvent::Key { .. }) => keys += 1,
                Some(
                    input_model::InputEvent::PointerAbsolute(_)
                    | input_model::InputEvent::PointerRelative { .. },
                ) => pointers += 1,
                Some(input_model::InputEvent::Button { .. }) => buttons += 1,
                Some(input_model::InputEvent::Scroll { .. }) => scrolls += 1,
                None => {}
            }
        }
        tracing::info!(
            events = events.len(),
            keys,
            pointers,
            buttons,
            scrolls,
            at_edge = edge,
            "agent CapturedBatch"
        );
    }
    link.send(Msg::Captured(CapturedBatch {
        events,
        at_edge: edge,
        activation: activation.take(),
    }))
    .await?;
    Ok(())
}

/// Handles one backend event.
#[allow(clippy::too_many_arguments)]
async fn handle_backend_event<S: AsyncRead + AsyncWrite + Unpin>(
    link: &mut AgentLink<S>,
    backend: &mut Backend,
    repeat: &mut Option<crate::repeat::SoftwareRepeat>,
    deadline: &mut Option<(Instant, Vec<u8>)>,
    self_timeout: &mut Option<(Instant, Vec<u8>)>,
    event: Event,
    held_lease: &mut Option<Vec<u8>>,
    batch: &mut Vec<LocalEvent>,
    at_edge: &mut bool,
    pending_activation: &mut Option<ipc::PortalActivation>,
) -> anyhow::Result<()> {
    match event {
        Event::Input(input) => match crate::events::to_local_event(&input, monotonic_us()) {
            Some(event) => batch.push(event),
            None => tracing::trace!("event has no wire form and was dropped"),
        },
        Event::AtEdge {
            barrier_id,
            activation,
        } => {
            // `docs/05-protocol.md`: the source's `CapturedBatch` with `at_edge` is what
            // opens a transition. On a Wayland source this is the only edge signal
            // there is, because the compositor diverts the pointer at the barrier.
            *at_edge = true;
            // Seed before any subsequent integrated remote motion, even if the
            // preparing buffer is later cleared by BeginCapture.
            if let Some(position) = activation.position.as_ref() {
                if let Some(seed) = crate::events::to_local_event(
                    &input_model::InputEvent::PointerAbsolute(input_model::Position::new(
                        position.x, position.y,
                    )),
                    monotonic_us(),
                ) {
                    batch.push(seed);
                }
            }
            *pending_activation = Some(activation);
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
            let lease_id = release_everything(backend, repeat, deadline, self_timeout, held_lease)
                .await
                .unwrap_or_default();
            link.send(Msg::Emergency(EmergencyFired { lease_id }))
                .await?;
        }
        Event::LocalTakeover {
            reason,
            distance_px,
        } => {
            // Rule 9 (KVM-035): «физическое … событие при активном inject →
            // `LocalInputDetected`. Agent сам освобождает ledger». The order matters: this
            // machine's keys and buttons are released *before* core hears about it, so the
            // user who just reached for the keyboard finds it working.
            tracing::warn!(
                reason,
                distance_px = ?distance_px,
                "physical input on the receiver: releasing the lease locally"
            );
            let lease_id = release_everything(backend, repeat, deadline, self_timeout, held_lease)
                .await
                .unwrap_or_default();
            link.send(Msg::LocalInput(ipc::LocalInputDetected { lease_id }))
                .await?;
        }
        Event::QueueOverflow => {
            // Rule 2: the agent ends capture itself and reports the overflow.
            tracing::error!("capture queue overflow; ending capture");
            forget_capture_state(repeat, self_timeout);
            let _ = backend.end_capture(false).await;
            send_error(link, "QUEUE_OVERFLOW", "capture queue overflowed").await?;
        }
        Event::Failed { code, detail } => {
            tracing::error!(
                code,
                detail = detail.as_str(),
                "capture failed; ending capture"
            );
            forget_capture_state(repeat, self_timeout);
            let _ = backend.end_capture(false).await;
            send_error(link, code, &detail).await?;
        }
    }
    Ok(())
}

/// Forgets the capture-side state a release ends: the repeat engine's slot and the
/// `TEMPORARY` self-timeout (batch-3 audit, F12).
///
/// The engine is the piece every release path has to clear: it holds the key the OS is
/// repeating for us, so a release that leaves it armed injects a fresh down for a key it has
/// just released — and, with no core left to send `StopRepeat`, keeps doing it for ever. The
/// injection deadline and the held lease belong to the *lease*, not to the capture, and an
/// `EndCapture` must not touch them.
fn forget_capture_state(
    repeat: &mut Option<crate::repeat::SoftwareRepeat>,
    self_timeout: &mut Option<(Instant, Vec<u8>)>,
) {
    if let Some(engine) = repeat.as_mut() {
        engine.resume();
    }
    *self_timeout = None;
}

/// Forgets the state a *lease* release ends: everything [`forget_capture_state`] clears, plus
/// the injection deadline and the held lease (batch-3 audit, F12).
///
/// Returns the lease that was held, for the frame that reports the release.
fn forget_lease_state(
    repeat: &mut Option<crate::repeat::SoftwareRepeat>,
    deadline: &mut Option<(Instant, Vec<u8>)>,
    self_timeout: &mut Option<(Instant, Vec<u8>)>,
    held_lease: &mut Option<Vec<u8>>,
) -> Option<Vec<u8>> {
    forget_capture_state(repeat, self_timeout);
    *deadline = None;
    held_lease.take()
}

/// Capture and ledger both go, in that order (rule 8 for lock/suspend, rule 10 for
/// portal sessions), and so does every piece of session state that goes with them.
///
/// The single release entry point for the paths that end a *session* (core silence, the
/// shutdown frame, an emergency, a receiver takeover) — see [`forget_lease_state`] for why one
/// place matters (batch-3 audit, F12).
async fn release_everything(
    backend: &mut Backend,
    repeat: &mut Option<crate::repeat::SoftwareRepeat>,
    deadline: &mut Option<(Instant, Vec<u8>)>,
    self_timeout: &mut Option<(Instant, Vec<u8>)>,
    held_lease: &mut Option<Vec<u8>>,
) -> Option<Vec<u8>> {
    let lease = forget_lease_state(repeat, deadline, self_timeout, held_lease);
    if let Err(detail) = backend.end_capture(true).await {
        tracing::warn!(detail = detail.as_str(), "EndCapture failed");
    }
    let released = backend.release_session().await;
    tracing::info!(
        keys = released.keys,
        buttons = released.buttons,
        "session-owned state released"
    );
    lease
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

/// Rule 6's watchdog, clocked from `now_ms` — the moment the agent became ready.
///
/// The attach, the token check and the platform probe all happen before the agent's
/// first frame can be expected, and on a loaded machine they outlast the whole silence
/// limit (measured on CI: over 300 ms between the attach and `AgentReady`). Counting
/// that as core silence is the false release of CI-38220074904 — the bootstrap worker
/// saw «core silent past the limit; releasing locally silence_ms=305» on the agent side
/// of the same failure. Rule 6 is scoped to `REMOTE` («Core молчит в REMOTE дольше
/// `core_silence_ms` … → agent выполняет release и Local»): before `AgentReady` the
/// agent holds no capture, so the clock starts here and the frames core already sent
/// reset it as the loop begins.
///
/// `now_ms` is the elapsed value of the session's own `started` at this point, so the
/// watchdog and the loop share one clock.
fn watchdog_at_ready(core_silence_ms: u32, now_ms: u64) -> Watchdog {
    match Watchdog::for_core_silence_ms(core_silence_ms, now_ms) {
        Ok(watchdog) => watchdog,
        Err(error) => {
            tracing::warn!(
                %error,
                "AgentAttach.core_silence_ms outside 200..=500; using the default"
            );
            Watchdog::for_core_silence_ms(::ipc::DEFAULT_CORE_SILENCE_MS, now_ms)
                .expect("the default core_silence_ms is in range")
        }
    }
}

#[cfg(test)]
mod watchdog_start_tests {
    use super::watchdog_at_ready;

    /// The CI-38220074904 failure, in integers: the backend probe takes longer than the
    /// whole silence limit, and that must not read as core silence.
    #[test]
    fn the_silence_clock_starts_when_the_agent_is_ready_not_at_the_attach() {
        // The attach is t = 0; the probe (a `spawn_blocking` snapshot plus the backend's
        // own setup) finishes at t = 700, past the 300 ms limit.
        let watchdog = watchdog_at_ready(300, 700);

        assert_eq!(watchdog.silence_ms(700), 0, "the clock starts at ready");
        assert!(
            !watchdog.silence_exceeded(700),
            "a slow start is not silence"
        );
        // Rule 6 from ready onwards: the limit is the limit.
        assert!(
            !watchdog.silence_exceeded(1_000),
            "300 ms of silence is the limit"
        );
        assert!(
            watchdog.silence_exceeded(1_001),
            "and one more millisecond is not"
        );
    }

    /// A config value the schema rejects falls back to the default rather than to a
    /// watchdog that would release on every loop iteration.
    #[test]
    fn an_out_of_range_limit_falls_back_to_the_default() {
        let watchdog = watchdog_at_ready(1_000, 500);
        assert_eq!(watchdog.silence_limit_ms(), 300, "the documented default");
        assert_eq!(watchdog.silence_ms(500), 0);
        assert!(!watchdog.silence_exceeded(800));
        assert!(watchdog.silence_exceeded(801));
    }
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
    use clipboard::Marker;
    use std::path::PathBuf;
    use tokio::io::duplex;

    const HOTKEY: &str = "Ctrl+Alt+Shift+F12";
    #[tokio::test]
    async fn activation_metadata_and_seed_precede_remote_motion_on_wire() {
        let mut harness = Harness::new().await;
        let activation = ipc::PortalActivation {
            position: Some(ipc::PointerAbs {
                display_id: String::new(),
                x: 120.0,
                y: 210.0,
            }),
            edge: ipc::PortalEdge::Right as i32,
        };
        harness
            .event(Event::AtEdge {
                barrier_id: Some(7),
                activation: activation.clone(),
            })
            .await;
        harness
            .event(Event::Input(input_model::InputEvent::PointerAbsolute(
                input_model::Position::new(123.0, 208.0),
            )))
            .await;
        let mut last_log = None;
        flush_captured(
            &mut harness.agent,
            &mut harness.batch,
            &mut harness.at_edge,
            &mut harness.activation,
            &mut last_log,
            0,
        )
        .await
        .unwrap();
        let Msg::Captured(batch) = harness.recv().await else {
            panic!("expected captured batch")
        };
        assert!(batch.at_edge);
        assert_eq!(batch.activation, Some(activation));
        let positions: Vec<_> = batch
            .events
            .iter()
            .filter_map(|event| match &event.ev {
                Some(ipc::local_event::Ev::Pointer(position)) => Some((position.x, position.y)),
                _ => None,
            })
            .collect();
        assert_eq!(positions, [(120.0, 210.0), (123.0, 208.0)]);
        assert!(harness.activation.is_none());
        assert!(!harness.at_edge);
    }

    #[tokio::test]
    async fn backend_startup_answers_pings_while_portal_permission_is_pending() {
        let (agent_io, core_io) = duplex(4096);
        let mut agent = AgentLink::new(agent_io, FRAME_MAJOR, FRAME_MINOR);
        let mut core = AgentLink::new(core_io, FRAME_MAJOR, FRAME_MINOR);
        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
        progress_tx
            .send(Msg::PortalPermission(ipc::PortalPermissionRequired {
                portal_interface: "org.freedesktop.portal.RemoteDesktop".to_owned(),
                purpose: "inject".to_owned(),
            }))
            .expect("progress receiver is open");
        let (opened, portal_opened) = tokio::sync::oneshot::channel();
        let startup = tokio::spawn(async move {
            complete_backend_startup(&mut agent, &mut progress_rx, async move {
                portal_opened.await.expect("portal approval signal");
                42
            })
            .await
            .expect("startup IPC")
        });
        assert!(matches!(
            core.recv().await.unwrap().msg,
            Some(Msg::PortalPermission(permission))
                if permission.portal_interface == "org.freedesktop.portal.RemoteDesktop"
        ));
        for nonce in 1..=3 {
            core.send(Msg::Ping(ipc::Ping { nonce })).await.unwrap();
            assert!(matches!(
                core.recv().await.unwrap().msg,
                Some(Msg::Pong(Pong { nonce: actual })) if actual == nonce
            ));
        }
        opened.send(()).unwrap();
        assert_eq!(startup.await.unwrap(), Some(42));
    }

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
        activation: Option<ipc::PortalActivation>,
        displays: crate::displays::DisplaysState,
        /// The software repeat engine, or `None` for native repeat (KVM-036).
        repeat: Option<crate::repeat::SoftwareRepeat>,
        /// The clipboard versions and the transfer being assembled (KVM-041).
        clipboard: ClipboardState,
        /// The harness's monotonic clock, advanced by the tests that need one.
        clock: u64,
        last_inject_log: Option<u64>,
    }

    impl Harness {
        async fn new() -> Self {
            Harness::dry(None).await
        }

        /// The same, with a dry session that has a clipboard (`--dry-clipboard-dir`).
        async fn dry(clipboard_dir: Option<PathBuf>) -> Self {
            // Room for a whole chunked clipboard payload on the wire. A test that writes the
            // frames before reading any of them blocks on the pipe as soon as it is full, so
            // the pipe has to hold the largest payload any of these tests sends — 1 MiB + 1
            // is 33 frames of 32 KiB — and not merely one frame.
            let (agent_io, core_io) = duplex(4 * 1024 * 1024);
            let (backend, ready, _refusals) = Backend::select(
                BackendKindArg(::protocol::ipc::BackendKind::Auto as i32),
                true,
                0,
                false,
                false,
                clipboard_dir,
                HOTKEY,
                &[],
                false,
                tokio::sync::mpsc::unbounded_channel().0,
                ::protocol::ipc::TakeoverDetection::Auto,
                12,
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
                activation: None,
                displays: crate::displays::DisplaysState::default(),
                // The harness drives the engine on an injected clock, so a repeat schedule
                // is asserted exactly instead of waited for (KVM-036).
                repeat: Some(crate::repeat::SoftwareRepeat::new(
                    crate::repeat::RepeatTiming::default(),
                )),
                clipboard: ClipboardState::new(),
                clock: 0,
                last_inject_log: None,
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
                &mut self.repeat,
                &mut self.clipboard,
                false,
                &mut self.last_inject_log,
                self.clock,
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
                &mut self.repeat,
                &mut self.deadline,
                &mut self.self_timeout,
                event,
                &mut self.held,
                &mut self.batch,
                &mut self.at_edge,
                &mut self.activation,
            )
            .await
            .expect("a backend event must not fail the handler");
        }

        /// Asserts that the handler answered nothing at all.
        async fn quiet(&mut self) {
            match tokio::time::timeout(Duration::from_millis(50), self.core.recv()).await {
                Err(_) => {}
                Ok(Ok(frame)) => panic!("expected no frame, got {:?}", frame.msg),
                Ok(Err(error)) => panic!("the link ended instead of staying quiet: {error}"),
            }
        }
    }

    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn windows_remote_raw_motion_reaches_captured_batch() {
        use crate::backend::SourceMotion;
        use input_model::{CaptureKind, InputEvent, Position};

        let mut harness = Harness::new().await;
        let mut motion = SourceMotion::new(Some(Position::new(100.0, 200.0)), 1.0);
        for (kind, input) in [
            (
                CaptureKind::Observe,
                InputEvent::PointerRelative { dx: 12, dy: 6 },
            ),
            (
                CaptureKind::Observe,
                InputEvent::PointerAbsolute(Position::new(120.0, 210.0)),
            ),
            (
                CaptureKind::Temporary,
                InputEvent::PointerRelative { dx: 12, dy: 6 },
            ),
            (
                CaptureKind::Remote,
                InputEvent::PointerRelative { dx: 12, dy: 6 },
            ),
        ] {
            if let Some(input) = motion.normalize(kind, input) {
                harness.event(Event::Input(input)).await;
            }
        }
        let mut last_log = None;
        flush_captured(
            &mut harness.agent,
            &mut harness.batch,
            &mut harness.at_edge,
            &mut harness.activation,
            &mut last_log,
            0,
        )
        .await
        .expect("production capture flush");
        let Msg::Captured(batch) = harness.recv().await else {
            panic!("expected CapturedBatch");
        };
        let positions: Vec<_> = batch
            .events
            .iter()
            .map(|event| {
                let Some(ipc::local_event::Ev::Pointer(pointer)) = &event.ev else {
                    panic!("expected absolute pointer");
                };
                (pointer.x, pointer.y)
            })
            .collect();
        assert_eq!(positions, [(120.0, 210.0), (132.0, 216.0), (144.0, 222.0)]);

        // A fresh capture must use its new OS seed, not the old virtual accumulator.
        let mut motion = SourceMotion::new(Some(Position::new(300.0, 400.0)), 2.0);
        harness
            .event(Event::Input(motion.seed_event().expect("startup baseline")))
            .await;
        let input = motion
            .normalize(
                CaptureKind::Remote,
                InputEvent::PointerRelative { dx: 12, dy: 6 },
            )
            .expect("seeded remote motion");
        harness.event(Event::Input(input)).await;
        flush_captured(
            &mut harness.agent,
            &mut harness.batch,
            &mut harness.at_edge,
            &mut harness.activation,
            &mut last_log,
            1_000,
        )
        .await
        .expect("production capture flush after new seed");
        let Msg::Captured(batch) = harness.recv().await else {
            panic!("expected restart CapturedBatch");
        };
        assert_eq!(batch.events.len(), 2);
        let Some(ipc::local_event::Ev::Pointer(seed)) = &batch.events[0].ev else {
            panic!("expected restart baseline");
        };
        assert_eq!((seed.x, seed.y), (300.0, 400.0));
        let Some(ipc::local_event::Ev::Pointer(pointer)) = &batch.events[1].ev else {
            panic!("expected restart absolute pointer");
        };
        assert_eq!((pointer.x, pointer.y), (306.0, 403.0));
    }

    static CLIPBOARD_DIRS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    /// A private directory under the system temp root, for the dry clipboard's two files.
    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lws-agent-clipboard-{}-{}-{label}",
            std::process::id(),
            CLIPBOARD_DIRS.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the temporary directory must be creatable");
        dir
    }

    /// One whole payload in one frame, as core sends it.
    fn set_text(version: u64, text: &str) -> Msg {
        Msg::SetClipboardText(ipc::SetClipboardText {
            origin_version: version,
            chunk_index: 0,
            chunk_count: 1,
            data: text.as_bytes().to_vec(),
            sha256: clipboard::sha256(text.as_bytes()),
        })
    }

    /// Waits out the dry hook's own pacing (`in.txt` is looked at ~20 times a second).
    async fn clipboard_tick() {
        tokio::time::sleep(crate::backend::DRY_CLIPBOARD_POLL + Duration::from_millis(10)).await;
    }

    fn read(path: &std::path::Path) -> String {
        std::fs::read_to_string(path).expect("the dry clipboard file must be readable")
    }

    /// A dry session with a clipboard: what core applies lands on the selection, the payload
    /// itself is not reported back as a local copy, and the next user copy is — numbered above
    /// the version core used (ADR-008's echo rule at the agent's edge).
    #[tokio::test]
    async fn a_payload_from_core_is_applied_and_its_own_echo_is_not_a_local_change() {
        let dir = temp_dir("apply");
        let in_path = dir.join("in.txt");
        let out_path = dir.join("out.txt");
        let mut harness = Harness::dry(Some(dir)).await;
        assert!(harness.backend.clipboard_available());
        assert!(
            harness
                .ready
                .capabilities
                .iter()
                .any(|name| name == "TEXT_CLIPBOARD"),
            "a dry session with a clipboard advertises the capability: {:?}",
            harness.ready.capabilities
        );
        assert_eq!(read(&out_path), "", "a fresh selection is empty");

        harness.send(set_text(41, "from core")).await;
        harness.quiet().await;
        assert_eq!(read(&out_path), "from core", "the selection holds it");
        assert_eq!(harness.clipboard.applied_version, 41);

        // The outside world reports the item this node caused: "it is what you put there".
        std::fs::write(&in_path, "from core").expect("in.txt is writable");
        clipboard_tick().await;
        assert!(
            harness.backend.clipboard_change().await.is_none(),
            "the payload core applied is not a local change"
        );
        assert_eq!(harness.clipboard.applied_version, 41);

        // A copy the user makes, on the other hand, is exactly what core has to hear about.
        std::fs::write(&in_path, "copied by the user").expect("in.txt is writable");
        clipboard_tick().await;
        let change = harness
            .backend
            .clipboard_change()
            .await
            .expect("a local copy surfaces");
        assert_eq!(change.text, "copied by the user");
        assert!(change.markers.is_empty(), "dry mode reports no marker");
        assert_eq!(read(&out_path), "copied by the user");

        send_clipboard_change(&mut harness.agent, &mut harness.clipboard, change)
            .await
            .expect("the frame goes out");
        match harness.recv().await {
            Msg::ClipboardChanged(changed) => {
                assert_eq!(changed.data, b"copied by the user");
                assert_eq!(changed.chunk_index, 0);
                assert_eq!(changed.chunk_count, 1);
                assert_eq!(changed.size_bytes, "copied by the user".len() as u64);
                assert_eq!(changed.markers, Vec::<String>::new());
                assert_eq!(changed.sha256, clipboard::sha256(b"copied by the user"));
                assert_eq!(
                    changed.origin_version, 42,
                    "strictly greater than the 41 core applied"
                );
            }
            other => panic!("expected ClipboardChanged, got {other:?}"),
        }
    }

    /// A release (`chunk_count == 0`) forgets the text, and a local change after it is still
    /// numbered above the version of the release.
    #[tokio::test]
    async fn a_release_empties_the_selection_and_does_not_undo_the_version_order() {
        let dir = temp_dir("release");
        let out_path = dir.join("out.txt");
        let mut harness = Harness::dry(Some(dir)).await;
        harness.send(set_text(5, "held")).await;
        harness.quiet().await;
        assert_eq!(read(&out_path), "held");

        harness
            .send(Msg::SetClipboardText(ipc::SetClipboardText {
                origin_version: 6,
                chunk_index: 0,
                chunk_count: 0,
                data: Vec::new(),
                sha256: Vec::new(),
            }))
            .await;
        harness.quiet().await;
        assert_eq!(read(&out_path), "", "the selection is released");
        assert_eq!(harness.clipboard.applied_version, 6);

        send_clipboard_change(
            &mut harness.agent,
            &mut harness.clipboard,
            ClipboardChange {
                text: "local".to_owned(),
                markers: Vec::new(),
            },
        )
        .await
        .expect("the frame goes out");
        match harness.recv().await {
            Msg::ClipboardChanged(changed) => assert_eq!(changed.origin_version, 7),
            other => panic!("expected ClipboardChanged, got {other:?}"),
        }
    }

    /// An out-of-sequence chunk and a payload whose hash does not cover it are both refused
    /// with a `CLIPBOARD_*` code, and neither touches the selection.
    #[tokio::test]
    async fn an_unverified_transfer_is_refused_and_never_applied() {
        let dir = temp_dir("verify");
        let out_path = dir.join("out.txt");
        let mut harness = Harness::dry(Some(dir)).await;

        // Index 1 as the first chunk of a two-chunk transfer: out of order.
        harness
            .send(Msg::SetClipboardText(ipc::SetClipboardText {
                origin_version: 7,
                chunk_index: 1,
                chunk_count: 2,
                data: b"def".to_vec(),
                sha256: clipboard::sha256(b"abcdef"),
            }))
            .await;
        match harness.recv().await {
            Msg::Error(error) => assert_eq!(error.code, "CLIPBOARD_INVALID"),
            other => panic!("expected an AgentError, got {other:?}"),
        }
        assert_eq!(read(&out_path), "", "nothing out of order is applied");

        // The right shape, the wrong hash.
        harness
            .send(Msg::SetClipboardText(ipc::SetClipboardText {
                origin_version: 8,
                chunk_index: 0,
                chunk_count: 1,
                data: "секрет".as_bytes().to_vec(),
                sha256: clipboard::sha256(b"another payload"),
            }))
            .await;
        match harness.recv().await {
            Msg::Error(error) => assert_eq!(error.code, "CLIPBOARD_HASH_MISMATCH"),
            other => panic!("expected an AgentError, got {other:?}"),
        }
        assert_eq!(read(&out_path), "", "unverified text is never applied");
        assert_eq!(
            harness.clipboard.applied_version, 0,
            "a refused transfer does not move the version"
        );

        // `chunk_count` outside `1..=64` is refused before anything is buffered.
        harness
            .send(Msg::SetClipboardText(ipc::SetClipboardText {
                origin_version: 9,
                chunk_index: 0,
                chunk_count: crate::clipboard_wire::MAX_CHUNKS + 1,
                data: b"x".to_vec(),
                sha256: Vec::new(),
            }))
            .await;
        match harness.recv().await {
            Msg::Error(error) => assert_eq!(error.code, "CLIPBOARD_INVALID"),
            other => panic!("expected an AgentError, got {other:?}"),
        }

        // And a valid two-chunk payload still goes through after all that.
        let payload = "abcdef".as_bytes().to_vec();
        let hash = clipboard::sha256(&payload);
        harness
            .send(Msg::SetClipboardText(ipc::SetClipboardText {
                origin_version: 10,
                chunk_index: 0,
                chunk_count: 2,
                data: b"abc".to_vec(),
                sha256: Vec::new(),
            }))
            .await;
        harness.quiet().await;
        harness
            .send(Msg::SetClipboardText(ipc::SetClipboardText {
                origin_version: 10,
                chunk_index: 1,
                chunk_count: 2,
                data: b"def".to_vec(),
                sha256: hash,
            }))
            .await;
        harness.quiet().await;
        assert_eq!(read(&out_path), "abcdef");
        assert_eq!(harness.clipboard.applied_version, 10);
    }

    /// A session with no clipboard at all answers `CLIPBOARD_DISABLED`, never polls, and
    /// advertises nothing (ADR-008: opt-in on both sides).
    #[tokio::test]
    async fn without_a_clipboard_the_frame_is_refused_with_clipboard_disabled() {
        let mut harness = Harness::new().await;
        assert!(!harness.backend.clipboard_available());
        assert!(
            !harness
                .ready
                .capabilities
                .iter()
                .any(|name| name == "TEXT_CLIPBOARD"),
            "no clipboard, no capability: {:?}",
            harness.ready.capabilities
        );
        assert!(harness.backend.clipboard_change().await.is_none());
        assert!(harness.backend.set_clipboard(Some("x")).await.is_err());

        harness.send(set_text(1, "x")).await;
        match harness.recv().await {
            Msg::Error(error) => assert_eq!(error.code, "CLIPBOARD_DISABLED"),
            other => panic!("expected an AgentError, got {other:?}"),
        }
    }

    /// Markers travel with every chunk of a multi-chunk payload and the hash only with the
    /// last one — the shape core's own assembly checks.
    #[tokio::test]
    async fn a_large_change_is_chunked_with_the_markers_on_every_frame() {
        let mut harness = Harness::new().await;
        let text = "x".repeat(70 * 1024);
        send_clipboard_change(
            &mut harness.agent,
            &mut harness.clipboard,
            ClipboardChange {
                text: text.clone(),
                markers: vec![Marker::KdeSecret, Marker::ExcludeFromMonitor],
            },
        )
        .await
        .expect("the frames go out");

        let mut collected = String::new();
        for index in 0..3u32 {
            match harness.recv().await {
                Msg::ClipboardChanged(changed) => {
                    assert_eq!(changed.chunk_index, index);
                    assert_eq!(changed.chunk_count, 3, "70 KiB is three 32 KiB frames");
                    assert_eq!(changed.size_bytes, 70 * 1024);
                    assert_eq!(changed.markers, vec!["kde_secret", "exclude_from_monitor"]);
                    assert_eq!(changed.origin_version, 1);
                    if index < 2 {
                        assert!(changed.sha256.is_empty(), "no hash before the last chunk");
                    } else {
                        assert_eq!(changed.sha256, clipboard::sha256(text.as_bytes()));
                    }
                    collected.push_str(&String::from_utf8(changed.data).expect("UTF-8 text"));
                }
                other => panic!("expected ClipboardChanged, got {other:?}"),
            }
        }
        assert_eq!(collected, text);
        harness.quiet().await;

        // The second change is numbered after the first, so core can order them.
        send_clipboard_change(
            &mut harness.agent,
            &mut harness.clipboard,
            ClipboardChange {
                text: "second".to_owned(),
                markers: Vec::new(),
            },
        )
        .await
        .expect("the frame goes out");
        match harness.recv().await {
            Msg::ClipboardChanged(changed) => assert_eq!(changed.origin_version, 2),
            other => panic!("expected ClipboardChanged, got {other:?}"),
        }
    }

    /// Two different ceilings, deliberately: a payload over the *policy's* 1 MiB is reported
    /// anyway (core is the side that answers `CLIPBOARD_TOO_LARGE`, and it knows
    /// `[clipboard].max_bytes`), while one beyond the *framing* ceiling — `MAX_CHUNKS` frames
    /// of `CHUNK_BYTES` each — has no wire form at all and is dropped rather than truncated.
    #[tokio::test]
    async fn the_policy_limit_is_reported_and_the_framing_ceiling_is_dropped() {
        let mut harness = Harness::new().await;

        // 1 MiB + 1: over the policy, inside the framing. It goes out whole, so core can name
        // the refusal.
        send_clipboard_change(
            &mut harness.agent,
            &mut harness.clipboard,
            ClipboardChange {
                text: "x".repeat(clipboard::MAX_BYTES as usize + 1),
                markers: Vec::new(),
            },
        )
        .await
        .expect("reporting it is not an error");
        // All 33 frames, so nothing is left in the pipe to confuse the next step: the whole
        // copy is reported, in order, with the hash on the last frame only.
        let mut seen = 0u32;
        while let Msg::ClipboardChanged(changed) = harness.recv().await {
            assert_eq!(changed.chunk_count, 33);
            assert_eq!(changed.chunk_index, seen);
            if seen == 0 {
                assert_eq!(changed.origin_version, 1);
                assert_eq!(changed.size_bytes, clipboard::MAX_BYTES + 1);
            }
            if changed.chunk_index + 1 == changed.chunk_count {
                assert!(
                    !changed.sha256.is_empty(),
                    "the last chunk carries the hash"
                );
                break;
            }
            assert!(
                changed.sha256.is_empty(),
                "only the last chunk carries the hash"
            );
            seen += 1;
        }
        assert_eq!(seen, 32, "33 frames, the last one ending the loop");

        // Beyond the framing ceiling: nothing is numbered, so the next change is version 2.
        let ceiling = clipboard_wire::MAX_CHUNKS as usize * clipboard_wire::CHUNK_BYTES;
        send_clipboard_change(
            &mut harness.agent,
            &mut harness.clipboard,
            ClipboardChange {
                text: "x".repeat(ceiling + 1),
                markers: Vec::new(),
            },
        )
        .await
        .expect("dropping it is not an error");
        harness.quiet().await;
        send_clipboard_change(
            &mut harness.agent,
            &mut harness.clipboard,
            ClipboardChange {
                text: "small".to_owned(),
                markers: Vec::new(),
            },
        )
        .await
        .expect("the frame goes out");
        match harness.recv().await {
            Msg::ClipboardChanged(changed) => assert_eq!(changed.origin_version, 2),
            other => panic!("expected ClipboardChanged, got {other:?}"),
        }
    }

    /// The whole loop, on a real link: a copy the user makes reaches core without anyone
    /// asking, and a payload core sends lands on the selection. This is the only test that
    /// covers the `select!` wiring itself — the clipboard branch, its own pacing, and the
    /// `SetClipboardText` arm behind the same loop that services the deadline and the watchdog.
    #[tokio::test]
    async fn the_running_loop_reports_a_local_copy_and_applies_what_core_sends() {
        // `Backend` holds the portal streams on Linux, which are not `Send`, so the loop is
        // driven on a local set: the same task, no cross-thread bound to satisfy.
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let dir = temp_dir("loop");
                let in_path = dir.join("in.txt");
                let out_path = dir.join("out.txt");
                // Wide enough for the chunked payloads this loop writes before anything
                // reads them (see `Harness::dry`).
                let (agent_io, core_io) = duplex(4 * 1024 * 1024);
                let (mut backend, ready, _refusals) = Backend::select(
                    backend::BackendKindArg(::protocol::ipc::BackendKind::Auto as i32),
                    true,
                    0,
                    false,
                    false,
                    Some(dir),
                    HOTKEY,
                    &[],
                    false,
                    tokio::sync::mpsc::unbounded_channel().0,
                    ::protocol::ipc::TakeoverDetection::Auto,
                    12,
                )
                .await;
                let mut agent = AgentLink::new(agent_io, FRAME_MAJOR, FRAME_MINOR);
                let mut core = AgentLink::new(core_io, FRAME_MAJOR, FRAME_MINOR);
                // 500 ms is the widest the watchdog accepts; the test stays well inside it.
                let mut watchdog =
                    Watchdog::for_core_silence_ms(500, 0).expect("500 ms is in range");
                let started = Instant::now();
                let running = tokio::task::spawn_local(async move {
                    loop_forever(
                        &mut agent,
                        &mut backend,
                        &ready,
                        AgentMode::Normal as i32,
                        ::protocol::ipc::RepeatMode::ReceiverSoftware as i32,
                        &mut watchdog,
                        started,
                        false,
                    )
                    .await
                });

                // 1. A copy the user makes, with the markers a password manager would set. Nothing has
                //    been applied from core yet, so it is the first version on the wire.
                std::fs::write(&in_path, "typed into the file").expect("in.txt is writable");
                std::fs::write(dir_join(&in_path, "in.markers"), "kde_secret").expect("in.markers");
                match tokio::time::timeout(Duration::from_secs(2), core.recv()).await {
                    Ok(Ok(frame)) => match frame.msg.expect("a payload") {
                        Msg::ClipboardChanged(changed) => {
                            assert_eq!(changed.data, b"typed into the file");
                            assert_eq!(changed.markers, vec!["kde_secret"]);
                            assert_eq!(changed.origin_version, 1);
                            assert_eq!(changed.chunk_count, 1);
                        }
                        other => panic!("expected ClipboardChanged, got {other:?}"),
                    },
                    Ok(Err(error)) => panic!("the link ended: {error}"),
                    Err(_) => panic!("the local copy never reached core"),
                }

                // 2. Core applies a payload of its own: it lands on the selection, and the next local
                //    change is numbered above the version core used.
                core.send(set_text(9, "from core"))
                    .await
                    .expect("the frame goes out");
                let mut applied = false;
                for _ in 0..100 {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    if read(&out_path) == "from core" {
                        applied = true;
                        break;
                    }
                }
                assert!(applied, "the payload core sent lands on the selection");

                std::fs::write(&in_path, "a second copy").expect("in.txt is writable");
                match tokio::time::timeout(Duration::from_secs(2), core.recv()).await {
                    Ok(Ok(frame)) => match frame.msg.expect("a payload") {
                        Msg::ClipboardChanged(changed) => {
                            assert_eq!(changed.data, b"a second copy");
                            assert_eq!(
                                changed.origin_version, 10,
                                "strictly above the 9 core applied"
                            );
                        }
                        other => panic!("expected ClipboardChanged, got {other:?}"),
                    },
                    Ok(Err(error)) => panic!("the link ended: {error}"),
                    Err(_) => panic!("the second copy never reached core"),
                }

                core.send(Msg::Shutdown(ipc::AgentShutdown {}))
                    .await
                    .expect("shutdown goes out");
                let stop = tokio::time::timeout(Duration::from_secs(2), running)
                    .await
                    .expect("the loop ends on Shutdown")
                    .expect("the task does not panic");
                assert!(
                    matches!(stop, Ok(Stop::Shutdown)),
                    "the loop ends on Shutdown, not on a lost core"
                );
            })
            .await;
    }

    /// `dir.join` for the sibling file the test writes, so the intent stays visible.
    fn dir_join(path: &std::path::Path, name: &str) -> PathBuf {
        path.parent()
            .expect("the dry clipboard files live in a directory")
            .join(name)
    }

    fn before(ms: u64) -> Instant {
        Instant::now() + Duration::from_millis(ms)
    }

    /// Batch-3 audit (F12): the lease deadline is a release path, and it has to stop the
    /// software repeat engine like `EndCapture`/`ReleaseSession` do.
    ///
    /// The engine keeps the key the OS is repeating for us. A held key whose repeat guard never
    /// fired (core kept renewing) was released on the OS by the deadline path, yet the engine
    /// still held the slot: the next tick injected a fresh down for that key and, with no core
    /// left to send `StopRepeat`, repeated it for ever.
    #[tokio::test]
    async fn the_deadline_release_stops_the_software_repeat() {
        let mut harness = Harness::new().await;
        // An injection lease with a deadline that is already due, and a key in the engine's
        // slot: `Inject` is what feeds the engine (`engine.applied`).
        harness
            .send(Msg::ArmDeadline(ipc::ArmDeadline {
                lease_id: lease(1),
                remaining_ms: 1,
            }))
            .await;
        harness
            .send(Msg::Inject(ipc::Inject {
                lease_id: lease(1),
                input_batch_seq: 1,
                events: vec![ipc::LocalEvent {
                    monotonic_us: 0,
                    ev: Some(ipc::local_event::Ev::Key(ipc::HidKey {
                        usage_page: 7,
                        usage: 0x04,
                        down: true,
                    })),
                }],
            }))
            .await;
        let Msg::InjectResult(applied) = harness.recv().await else {
            panic!("expected the injection report")
        };
        assert!(applied.applied_count > 0, "the down reaches the backend");
        let engine = harness.repeat.as_ref().expect("software repeat");
        assert_eq!(
            engine.holding(),
            Some(input_model::HidKey::keyboard(0x04)),
            "the injected down takes the repeat slot"
        );

        tokio::time::sleep(Duration::from_millis(5)).await;
        let mut released_after_silence = false;
        // A watchdog that has just seen a frame: the silence path must not be what ends this
        // session, or the test would pass without the deadline path doing anything.
        let mut watchdog = Watchdog::for_core_silence_ms(300, harness.clock).expect("watchdog");
        service_timers(
            &mut harness.agent,
            &mut harness.backend,
            &mut harness.repeat,
            &mut harness.deadline,
            &mut harness.self_timeout,
            &mut harness.held,
            &mut released_after_silence,
            &mut watchdog,
            harness.clock,
        )
        .await
        .expect("the timers run");
        assert!(
            !released_after_silence,
            "the deadline path ended the session, not the silence watchdog"
        );
        let Msg::DeadlineExpired(expired) = harness.recv().await else {
            panic!("expected the deadline to be reported")
        };
        assert_eq!(expired.lease_id, lease(1));
        let engine = harness.repeat.as_mut().expect("software repeat");
        assert_eq!(
            engine.holding(),
            None,
            "the release must clear the key the engine was repeating"
        );
        assert_eq!(
            engine.due(harness.clock + 60_000),
            None,
            "no tick may produce a down after the release"
        );
    }

    #[tokio::test]
    async fn a_temporary_capture_arms_the_self_timeout_and_a_remote_capture_clears_it() {
        let mut harness = Harness::new().await;

        harness
            .send(Msg::BeginCapture(ipc::BeginCapture {
                kind: ::protocol::ipc::CaptureKind::Temporary as i32,
                lease_id: lease(7),
                self_timeout_ms: 500,
                dev_capture_injected: false,
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
                dev_capture_injected: false,
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
                dev_capture_injected: false,
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
                dev_capture_injected: false,
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
                dev_capture_injected: false,
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
                dev_capture_injected: false,
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
    async fn empty_lease_capture_release_preserves_receiver_state() {
        let mut harness = Harness::new().await;
        harness
            .send(Msg::PrepareInject(ipc::PrepareInject {
                lease_id: lease(7),
            }))
            .await;
        harness.recv().await;
        harness
            .send(Msg::Inject(ipc::Inject {
                lease_id: lease(7),
                input_batch_seq: 1,
                events: vec![ipc::LocalEvent {
                    ev: Some(ipc::local_event::Ev::Key(ipc::HidKey {
                        usage_page: 7,
                        usage: 0x04,
                        down: true,
                    })),
                    ..Default::default()
                }],
            }))
            .await;
        harness.recv().await;
        let timeout = (Instant::now() + Duration::from_secs(1), lease(7));
        harness.self_timeout = Some(timeout.clone());
        harness.deadline = Some(timeout.clone());
        harness
            .send(Msg::EndCapture(ipc::EndCapture {
                lease_id: Vec::new(),
                replay_buffered_locally: true,
            }))
            .await;
        let Msg::Released(released) = harness.recv().await else {
            panic!("expected Released")
        };
        assert!(released.lease_id.is_empty());
        assert_eq!(released.keys_released, 0);
        assert_eq!(released.buttons_released, 0);
        assert_eq!(harness.held, Some(lease(7)));
        assert_eq!(harness.self_timeout, Some(timeout.clone()));
        assert_eq!(harness.deadline, Some(timeout));
        assert_eq!(
            harness.repeat.as_ref().unwrap().holding(),
            Some(input_model::HidKey::keyboard(0x04))
        );
        // Dry injection has no native ledger. The assertions above exercise the real
        // session-owned receiver state; native EIS pressed-state needs a compositor.
    }

    /// Rule 9 (KVM-035): the receiver hands its ledger back *before* core hears about the
    /// takeover, and the frame names the lease that was injecting here.
    #[tokio::test]
    async fn a_receiver_takeover_releases_first_and_reports_the_lease() {
        let mut harness = Harness::new().await;
        harness
            .send(Msg::PrepareInject(ipc::PrepareInject {
                lease_id: lease(7),
            }))
            .await;
        harness.recv().await;

        harness
            .event(Event::LocalTakeover {
                reason: "key",
                distance_px: None,
            })
            .await;
        match harness.recv().await {
            Msg::LocalInput(detected) => assert_eq!(detected.lease_id, lease(7)),
            other => panic!("expected LocalInputDetected, got {other:?}"),
        }
        assert!(
            harness.held.is_none(),
            "the ledger is released before core is told (rule 9)"
        );
    }

    /// KVM-036 / AC-14: the guard frame is what stops the software engine, and silence after
    /// it never produces another repeat.
    #[tokio::test]
    async fn the_repeat_guard_stops_the_software_engine() {
        let mut harness = Harness::new().await;
        harness
            .send(Msg::Inject(ipc::Inject {
                lease_id: lease(9),
                input_batch_seq: 1,
                events: vec![ipc::LocalEvent {
                    ev: Some(ipc::local_event::Ev::Key(ipc::HidKey {
                        usage_page: 7,
                        usage: 0x04,
                        down: true,
                    })),
                    ..Default::default()
                }],
            }))
            .await;
        match harness.recv().await {
            Msg::InjectResult(result) => assert_eq!(result.applied_count, 1),
            other => panic!("expected an InjectResult, got {other:?}"),
        }
        let holding = input_model::HidKey::keyboard(0x04);
        assert_eq!(
            harness.repeat.as_ref().expect("software mode").holding(),
            Some(holding),
            "an injected key-down takes the repeat slot"
        );

        harness
            .send(Msg::StopRepeat(ipc::StopRepeat { lease_id: lease(9) }))
            .await;
        let engine = harness.repeat.as_mut().expect("software mode");
        assert_eq!(engine.holding(), None);
        assert_eq!(
            engine.due(u64::MAX / 2),
            None,
            "the guard ends the repeat for good"
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
                dev_capture_injected: false,
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
                takeover_detection: ipc::TakeoverDetection::Auto as i32,
                takeover_threshold_px: 12,
                repeat_mode: ipc::RepeatMode::ReceiverSoftware as i32,
                launch_token: vec![0u8; 32],
                mode: AgentMode::Normal as i32,
                emergency_hotkey: HOTKEY.to_owned(),
                backend_override: ::protocol::ipc::BackendKind::Auto as i32,
                restore_tokens: Vec::new(),
                require_emergency_hotkey: true,
                core_silence_ms: 300,
                allow_portal_sessions: false,
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
                dev_capture_injected: false,
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
