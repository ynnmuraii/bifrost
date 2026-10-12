//! One peer's input/control link: TLS, `Hello`, framed envelopes, liveness.
//!
//! A link is a Tokio task plus a cloneable [`LinkHandle`]. The task owns the socket and the
//! per-direction `seq` counter; the handle owns the bounded outbound queue.
//!
//! # Why the reader is its own task, and why the coordinator never writes inside `select!`
//!
//! `read_exact` is not cancellation-safe: cancelling it mid-frame consumes bytes and
//! desynchronises the stream. The link therefore splits the TLS stream (`tokio::io::split`)
//! into a reader task that only reads whole frames, and a coordinator that picks a step with
//! `select!` and then performs the write *after* the `select!` returned.

use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use focus_router as fr;
use geometry::NodeId;
use input_model::InputEvent;
use protocol::frame::{decode_message, encode_message, Channel};
use protocol::hello::HelloHandshake;
use protocol::seq::SeqValidator;
use protocol::{limits, validate, wire};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf, ReadHalf};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::convert::{self, ConvertError, PeerInfo};
use crate::policy::{self, LivenessAction};
use crate::queue::{self, Outbound};
use crate::TransportError;

/// Why a link went down.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkDownReason {
    /// The local side closed it (manager shutdown, or a link that replaced this one).
    LocalClosed,
    /// The peer sent `Shutdown`.
    PeerShutdown,
    /// No `Pong` within the configured offline window ([`policy::LinkLiveness`]).
    Timeout,
    /// A framing, sequence or conversion violation; the link is closed.
    Protocol(String),
    /// Socket error.
    Io(String),
    /// Refused: the workspace is already at `max_members`.
    MembershipFull,
}

/// What the application sees from a link.
#[derive(Debug, Clone, PartialEq)]
pub enum LinkEvent {
    /// TLS and the `Hello` exchange finished.
    Up {
        /// The peer.
        peer: NodeId,
        /// Its `Hello` gossip.
        hello: PeerInfo,
        /// The negotiated `connection_id`: the dialer's random 16 bytes, which the acceptor
        /// adopts. Every `Envelope` of this link carries it, and a clipboard channel is bound
        /// to it as its parent (`docs/05-protocol.md`, «Clipboard канал»).
        connection_id: [u8; 16],
    },
    /// One message from the peer, in the layer that owns it.
    Message {
        /// The peer it came from.
        peer: NodeId,
        /// The message.
        message: crate::convert::Inbound,
    },
    /// The link is gone.
    Down {
        /// The peer.
        peer: NodeId,
        /// Why.
        reason: LinkDownReason,
    },
}

/// Who dialled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Direction {
    /// This node dialled.
    Outgoing,
    /// The peer dialled; we accepted.
    Incoming,
}

impl Direction {
    /// `true` for [`Direction::Outgoing`].
    pub const fn is_outgoing(self) -> bool {
        matches!(self, Direction::Outgoing)
    }
}

/// Either side of a TLS connection, so one code path serves both directions.
pub enum Stream {
    /// Dialer side.
    Client(Box<tokio_rustls::client::TlsStream<TcpStream>>),
    /// Acceptor side.
    Server(Box<tokio_rustls::server::TlsStream<TcpStream>>),
}

impl AsyncRead for Stream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match &mut *self {
            Stream::Client(stream) => Pin::new(stream).poll_read(cx, buf),
            Stream::Server(stream) => Pin::new(stream).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match &mut *self {
            Stream::Client(stream) => Pin::new(stream).poll_write(cx, buf),
            Stream::Server(stream) => Pin::new(stream).poll_write(cx, buf),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match &mut *self {
            Stream::Client(stream) => Pin::new(stream).poll_flush(cx),
            Stream::Server(stream) => Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match &mut *self {
            Stream::Client(stream) => Pin::new(stream).poll_shutdown(cx),
            Stream::Server(stream) => Pin::new(stream).poll_shutdown(cx),
        }
    }
}

/// Reads one framed `Envelope`, refusing a length outside the channel's limit before
/// allocating the body.
pub async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<wire::Envelope, TransportError> {
    let mut prefix = [0u8; 4];
    reader.read_exact(&mut prefix).await?;
    let length = u32::from_be_bytes(prefix) as usize;
    let limit = Channel::InputControl.max_frame_len();
    if length == 0 || length > limit {
        return Err(TransportError::Protocol(format!(
            "frame length {length} outside 1..={limit}"
        )));
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).await?;
    // Re-attach the prefix so the protocol crate performs the authoritative checks.
    let mut framed = Vec::with_capacity(4 + length);
    framed.extend_from_slice(&prefix);
    framed.extend_from_slice(&body);
    decode_message::<wire::Envelope>(Channel::InputControl, &framed)
        .map_err(|error| TransportError::Protocol(format!("{error:?}")))?
        .ok_or_else(|| TransportError::Protocol("incomplete frame".into()))
}

/// Writes one framed `Envelope`.
pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    envelope: &wire::Envelope,
) -> Result<(), TransportError> {
    let bytes = encode_message(Channel::InputControl, envelope)
        .map_err(|error| TransportError::Protocol(format!("{error:?}")))?;
    writer.write_all(&bytes).await?;
    Ok(())
}

/// A cloneable handle to one link's outbound queue.
#[derive(Debug, Clone)]
pub struct LinkHandle {
    peer: NodeId,
    tx: mpsc::Sender<Outbound>,
    dropped_motion: Arc<AtomicU64>,
}

impl LinkHandle {
    /// The peer this handle talks to.
    pub const fn peer(&self) -> &NodeId {
        &self.peer
    }

    /// Enqueues a message.
    ///
    /// Control messages and any batch containing a key/button/scroll edge wait for space
    /// (bounded backpressure, never a silent drop). A **motion-only** batch is dropped when
    /// the queue is saturated, because the next position supersedes it; the drop is counted
    /// and visible through [`LinkHandle::dropped_motion_batches`].
    pub async fn send(&self, message: fr::Message) -> Result<(), TransportError> {
        match queue::outbound_of(message) {
            Outbound::Motion(events) => {
                self.send_motion(events);
                Ok(())
            }
            Outbound::Guaranteed(message) => self
                .tx
                .send(Outbound::Guaranteed(message))
                .await
                .map_err(|_| TransportError::LinkClosed),
            // `outbound_of` never produces this arm; it exists because `Outbound` is shared.
            Outbound::Layout(message) => self.send_layout(message).await,
            // The same: a clipboard message is queued through `send_clipboard`, never here.
            Outbound::Clipboard(message) => self.send_clipboard(message).await,
        }
    }

    /// Sends one layout-document message (KVM-072).
    ///
    /// Always guaranteed and never coalesced: a document, its CAS and the confirmations around
    /// them are control traffic, and a dropped `TopologyApplied` would leave the coordinator
    /// waiting forever.
    pub async fn send_layout(
        &self,
        message: crate::convert::LayoutMessage,
    ) -> Result<(), TransportError> {
        self.tx
            .send(Outbound::Layout(message))
            .await
            .map_err(|_| TransportError::LinkClosed)
    }

    /// Sends one clipboard message on this link.
    ///
    /// The grant is the only clipboard message the input link carries, and it has to reach the
    /// peer *before* the channel it authorises exists (`docs/05-protocol.md`, «Clipboard канал»);
    /// every later clipboard message travels on that channel instead.
    pub async fn send_clipboard(
        &self,
        message: crate::clipboard::ClipboardMessage,
    ) -> Result<(), TransportError> {
        self.tx
            .send(Outbound::Clipboard(message))
            .await
            .map_err(|_| TransportError::LinkClosed)
    }

    /// Enqueues pointer motion without waiting; returns whether it was queued.
    pub fn send_motion(&self, events: Vec<InputEvent>) -> bool {
        if events.is_empty() {
            return true;
        }
        match self.tx.try_send(Outbound::Motion(events)) {
            Ok(()) => true,
            Err(_) => {
                self.dropped_motion.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
    }

    /// Motion batches dropped because the queue was saturated.
    pub fn dropped_motion_batches(&self) -> u64 {
        self.dropped_motion.load(Ordering::Relaxed)
    }

    /// Asks the link to close: a `Shutdown` is queued, and the task ends after writing it.
    pub fn close(&self) {
        let _ = self
            .tx
            .try_send(Outbound::Guaranteed(fr::Message::Shutdown {
                reason: fr::LeaveReason::Shutdown,
            }));
    }
}

/// A link after it has been spawned.
pub struct Link {
    /// The app-facing handle.
    pub handle: LinkHandle,
    /// The task's join handle; whoever wants to reconnect keeps it.
    pub task: JoinHandle<()>,
}

/// Where a link reports to (the manager's internal channel).
#[derive(Debug)]
pub enum LinkReport {
    /// Handshake done.
    Up {
        /// The peer.
        peer: NodeId,
        /// Who dialled this link.
        direction: Direction,
        /// Its gossip.
        hello: PeerInfo,
        /// The negotiated `connection_id` (see [`LinkEvent::Up`]).
        connection_id: [u8; 16],
    },
    /// A focus/lease message.
    Message {
        /// The peer.
        peer: NodeId,
        /// The message, in the layer that owns it.
        message: crate::convert::Inbound,
    },
    /// The link ended.
    Down {
        /// The peer.
        peer: NodeId,
        /// Who dialled *this* link, so the manager tells a `Down` from one of two
        /// simultaneous links to the same peer apart.
        direction: Direction,
        /// Why.
        reason: LinkDownReason,
    },
}

/// Everything a link needs to start.
pub struct LinkConfig {
    /// The peer identity, taken from its certificate by the manager.
    pub peer: NodeId,
    /// Our identity.
    pub local: NodeId,
    /// Who dialled.
    pub direction: Direction,
    /// The `Hello` we send.
    pub local_hello: wire::Hello,
    /// The established TLS stream.
    pub stream: Stream,
    /// Where link reports go.
    pub reports: mpsc::Sender<LinkReport>,
    /// Bounded queue capacity.
    pub queue_capacity: usize,
    /// The configured liveness cadence for this link.
    pub liveness: policy::LinkLiveness,
}

/// Spawns a link and returns its handle and task.
pub fn spawn(config: LinkConfig) -> Result<Link, TransportError> {
    let (tx, rx) = mpsc::channel::<Outbound>(config.queue_capacity.max(1));
    let dropped_motion = Arc::new(AtomicU64::new(0));
    let handle = LinkHandle {
        peer: config.peer,
        tx,
        dropped_motion,
    };
    let task = tokio::spawn(run(config, rx));
    Ok(Link { handle, task })
}

async fn run(config: LinkConfig, outbound: mpsc::Receiver<Outbound>) {
    let LinkConfig {
        peer,
        direction,
        local_hello,
        stream,
        reports,
        liveness,
        ..
    } = config;

    let connection_id = if direction.is_outgoing() {
        match identity::random16() {
            Ok(id) => id,
            Err(error) => {
                report_down(
                    &reports,
                    peer,
                    direction,
                    LinkDownReason::Protocol(error.to_string()),
                )
                .await;
                return;
            }
        }
    } else {
        // The acceptor adopts the dialer's id during the handshake; until then it writes
        // nothing, so the placeholder can never reach the wire.
        [0u8; 16]
    };
    let mut handshake = if direction.is_outgoing() {
        HelloHandshake::dialer(
            limits::PROTOCOL_MAJOR,
            limits::PROTOCOL_MINOR,
            connection_id,
        )
    } else {
        HelloHandshake::acceptor(limits::PROTOCOL_MAJOR, limits::PROTOCOL_MINOR)
    };

    let (reader, mut writer) = tokio::io::split(stream);
    let (frames_tx, mut frames_rx) = mpsc::channel::<Result<wire::Envelope, TransportError>>(8);
    let reader_task = tokio::spawn(read_loop(reader, frames_tx));

    let mut state = LinkState::new(peer, direction, connection_id);

    // The dialer speaks first: its `Hello` is the first frame of its direction (`seq = 1`).
    if direction.is_outgoing() {
        match state
            .write_hello(&mut writer, &mut handshake, local_hello.clone())
            .await
        {
            Ok(()) => {}
            Err(reason) => {
                reader_task.abort();
                report_down(&reports, peer, direction, reason).await;
                return;
            }
        }
    }

    let mut outbound = outbound;
    let reason = loop {
        let now_ms = state.started.elapsed().as_millis() as u64;
        let wake =
            policy::next_liveness_wakeup(now_ms, state.last_pong_ms, state.last_ping_ms, liveness);
        let sleep = Duration::from_millis(wake.saturating_sub(now_ms).max(1));

        enum Step {
            Frame(Option<Result<wire::Envelope, TransportError>>),
            Outbound(Option<Outbound>),
            Tick,
        }

        let step = tokio::select! {
            frame = frames_rx.recv() => Step::Frame(frame),
            item = outbound.recv() => Step::Outbound(item),
            () = tokio::time::sleep(sleep) => Step::Tick,
        };

        match step {
            Step::Frame(None) => break LinkDownReason::Io("reader stopped".into()),
            Step::Frame(Some(Err(error))) => break reason_from(error),
            Step::Frame(Some(Ok(envelope))) => {
                match state
                    .on_frame(
                        &mut writer,
                        &mut handshake,
                        &envelope,
                        &local_hello,
                        &reports,
                    )
                    .await
                {
                    Ok(()) => {}
                    Err(reason) => break reason,
                }
            }
            Step::Outbound(None) => break LinkDownReason::LocalClosed,
            Step::Outbound(Some(first)) => {
                if let Err(reason) =
                    drain_outbound(&mut writer, &mut outbound, first, &mut state).await
                {
                    break reason;
                }
            }
            Step::Tick => {
                let now_ms = state.started.elapsed().as_millis() as u64;
                match policy::liveness_action(
                    now_ms,
                    state.last_pong_ms,
                    state.last_ping_ms,
                    liveness,
                ) {
                    LivenessAction::Ping => {
                        if let Err(reason) = state.send_ping(&mut writer).await {
                            break reason;
                        }
                    }
                    LivenessAction::Offline => break LinkDownReason::Timeout,
                    LivenessAction::Idle => {}
                }
            }
        }
    };

    reader_task.abort();
    report_down(&reports, peer, direction, reason).await;
}

/// The coordinator's mutable state.
struct LinkState {
    peer: NodeId,
    direction: Direction,
    tx_seq: u64,
    connection_id: [u8; 16],
    announced: bool,
    started: tokio::time::Instant,
    last_pong_ms: u64,
    last_ping_ms: Option<u64>,
    /// Counter for `Ping` nonces (the wire field is a `uint64`).
    ping_nonce: u64,
    /// Display stamped on outbound batch positions; empty until the model carries one.
    batch_display: String,
    /// Per-direction sequence validation: «`rx_seq` строго +1. Нарушение приводит к
    /// закрытию CONNECTION».
    rx_seq: SeqValidator,
}

impl LinkState {
    fn new(peer: NodeId, direction: Direction, connection_id: [u8; 16]) -> Self {
        Self {
            peer,
            direction,
            tx_seq: 1,
            connection_id,
            announced: false,
            started: tokio::time::Instant::now(),
            last_pong_ms: 0,
            last_ping_ms: None,
            ping_nonce: 0,
            batch_display: String::new(),
            rx_seq: SeqValidator::new(),
        }
    }

    async fn write_hello<W: AsyncWrite + Unpin>(
        &mut self,
        writer: &mut W,
        handshake: &mut HelloHandshake,
        hello: wire::Hello,
    ) -> Result<(), LinkDownReason> {
        let envelope = handshake
            .local_hello(hello)
            .map_err(|error| LinkDownReason::Protocol(format!("{error:?}")))?;
        self.tx_seq = 2;
        write_frame(writer, &envelope).await.map_err(reason_from)
    }

    async fn send_ping<W: AsyncWrite + Unpin>(
        &mut self,
        writer: &mut W,
    ) -> Result<(), LinkDownReason> {
        // `wire::Ping.nonce` is a plain `uint64`; a monotonic counter is enough to match a
        // `Pong` and it cannot collide within a link's lifetime.
        self.ping_nonce = self.ping_nonce.wrapping_add(1);
        let envelope = self.envelope(wire::envelope::Payload::Ping(wire::Ping {
            nonce: self.ping_nonce,
        }));
        self.last_ping_ms = Some(self.started.elapsed().as_millis() as u64);
        write_frame(writer, &envelope).await.map_err(reason_from)
    }

    fn envelope(&mut self, payload: wire::envelope::Payload) -> wire::Envelope {
        let seq = self.tx_seq;
        self.tx_seq += 1;
        wire::Envelope {
            major: limits::PROTOCOL_MAJOR,
            minor: limits::PROTOCOL_MINOR,
            connection_id: self.connection_id.to_vec(),
            seq,
            lease_id: Vec::new(),
            payload: Some(payload),
        }
    }

    async fn on_frame<W: AsyncWrite + Unpin>(
        &mut self,
        writer: &mut W,
        handshake: &mut HelloHandshake,
        envelope: &wire::Envelope,
        local_hello: &wire::Hello,
        reports: &mpsc::Sender<LinkReport>,
    ) -> Result<(), LinkDownReason> {
        // «`rx_seq` строго +1. Нарушение приводит к закрытию CONNECTION» counts *every*
        // frame of the peer's direction, and the sender bumps its `tx_seq` for `Ping`
        // and `Pong` as well (they are ordinary envelopes there). Accepting the
        // sequence only for the frames that reach the application would therefore
        // leave every liveness frame unaccounted for and make the next real frame
        // look like a gap.
        if self.announced {
            self.rx_seq
                .accept(envelope.seq)
                .map_err(|error| LinkDownReason::Protocol(format!("{error:?}")))?;
        }

        // Liveness frames belong to the transport and never reach the application.
        match &envelope.payload {
            Some(wire::envelope::Payload::Ping(ping)) => {
                let pong = self.envelope(wire::envelope::Payload::Pong(wire::Pong {
                    nonce: ping.nonce,
                }));
                return write_frame(writer, &pong).await.map_err(reason_from);
            }
            Some(wire::envelope::Payload::Pong(_)) => {
                self.last_pong_ms = self.started.elapsed().as_millis() as u64;
                return Ok(());
            }
            _ => {}
        }

        if !self.announced {
            let Some(wire::envelope::Payload::Hello(hello)) = &envelope.payload else {
                return Err(LinkDownReason::Protocol(
                    "a frame arrived before Hello".into(),
                ));
            };
            validate::hello(hello)
                .map_err(|error| LinkDownReason::Protocol(format!("{error:?}")))?;
            let info = PeerInfo::from_hello(hello)
                .map_err(|error| LinkDownReason::Protocol(format!("{error:?}")))?;
            if info.node_id != self.peer {
                // The certificate is the identity; `Hello` may not claim another one.
                return Err(LinkDownReason::Protocol(
                    "Hello.node_id does not match the certificate SPKI hash".into(),
                ));
            }
            let negotiated = handshake
                .on_remote_hello(envelope)
                .map_err(|error| LinkDownReason::Protocol(format!("{error:?}")))?;
            self.connection_id = negotiated.connection_id;
            self.announced = true;
            // The `Hello` is the first frame of the peer's direction, and
            // `on_remote_hello` already rejected anything but `seq = 1`; prime the validator
            // with it so the next frame (which is `seq = 2`) is accepted.
            self.rx_seq
                .accept(envelope.seq)
                .map_err(|error| LinkDownReason::Protocol(format!("{error:?}")))?;
            // The acceptor answers the dialer's `Hello` with its own (its `seq = 1`): «узел,
            // открывший окно сопряжения ... отправляет `PairHello` первым, второй узел
            // дозванивается и отвечает тем же сообщением» is the pairing analogue, and for
            // the link the docs say «Первым отправляет `Hello` TCP-инициатор ... принимающая
            // сторона отвечает своим `Hello` с тем же `connection_id`».
            if !handshake.is_established() {
                self.write_hello(writer, handshake, local_hello.clone())
                    .await?;
            }
            return reports
                .send(LinkReport::Up {
                    peer: self.peer,
                    direction: self.direction,
                    hello: info,
                    connection_id: self.connection_id,
                })
                .await
                .map_err(|_| LinkDownReason::LocalClosed);
        }

        validate::envelope(envelope)
            .map_err(|error| LinkDownReason::Protocol(format!("{error:?}")))?;
        match &envelope.payload {
            Some(wire::envelope::Payload::Shutdown(_)) => {
                return Err(LinkDownReason::PeerShutdown);
            }
            Some(wire::envelope::Payload::Hello(_)) => {
                return Err(LinkDownReason::Protocol("a second Hello".into()));
            }
            // «пятый член получает `MEMBERSHIP_FULL`»: the refusal is the reason this link
            // ends, so the refused side reports it as such instead of a bare `Shutdown`
            // (or a protocol failure). Every other `ProtocolError` code stays an ordinary
            // message: only the membership refusal terminates the link.
            Some(wire::envelope::Payload::Error(error))
                if matches!(
                    convert::error_code_from_i32(error.code),
                    Ok(fr::ErrorCode::MembershipFull)
                ) =>
            {
                return Err(LinkDownReason::MembershipFull);
            }
            _ => {}
        }
        let message = convert::inbound_from_envelope(envelope)
            .map_err(|error: ConvertError| LinkDownReason::Protocol(error.to_string()))?;
        reports
            .send(LinkReport::Message {
                peer: self.peer,
                message,
            })
            .await
            .map_err(|_| LinkDownReason::LocalClosed)
    }
}

async fn read_loop<R: AsyncRead + Unpin>(
    mut reader: ReadHalf<R>,
    frames: mpsc::Sender<Result<wire::Envelope, TransportError>>,
) {
    loop {
        let frame = read_frame(&mut reader).await;
        let stop = frame.is_err();
        if frames.send(frame).await.is_err() || stop {
            return;
        }
    }
}

async fn drain_outbound<W: AsyncWrite + Unpin>(
    writer: &mut W,
    outbound: &mut mpsc::Receiver<Outbound>,
    first: Outbound,
    state: &mut LinkState,
) -> Result<(), LinkDownReason> {
    // Collect what is already queued (bounded by the channel capacity) so consecutive motion
    // runs can be merged while every other item keeps its exact position.
    let mut queued = vec![first];
    while let Ok(item) = outbound.try_recv() {
        queued.push(item);
    }
    let mut index = 0;
    while index < queued.len() {
        match &queued[index] {
            Outbound::Guaranteed(_) => {
                let Outbound::Guaranteed(message) = &queued[index] else {
                    unreachable!()
                };
                if matches!(message, fr::Message::Shutdown { .. }) {
                    write_message(writer, state, message.clone()).await?;
                    return Err(LinkDownReason::LocalClosed);
                }
                write_message(writer, state, message.clone()).await?;
                index += 1;
            }
            Outbound::Layout(message) => {
                write_layout_message(writer, state, message.clone()).await?;
                index += 1;
            }
            Outbound::Clipboard(message) => {
                write_clipboard_message(writer, state, message.clone()).await?;
                index += 1;
            }
            Outbound::Motion(_) => {
                // Merge the contiguous motion run that starts here.
                let mut events = Vec::new();
                while let Some(Outbound::Motion(next)) = queued.get(index) {
                    events.extend(next.iter().copied());
                    index += 1;
                }
                let batch = fr::Message::InputBatch {
                    lease_id: [0u8; 16],
                    input_batch_seq: 0,
                    events: queue::coalesce_motion(events),
                };
                write_message(writer, state, batch).await?;
            }
        }
    }
    Ok(())
}

/// Writes one clipboard message: same framing, no lease (it belongs to the connection).
async fn write_clipboard_message<W: AsyncWrite + Unpin>(
    writer: &mut W,
    state: &mut LinkState,
    message: crate::clipboard::ClipboardMessage,
) -> Result<(), LinkDownReason> {
    let envelope = message
        .to_envelope(state.tx_seq, &state.connection_id)
        .map_err(|error| LinkDownReason::Protocol(error.to_string()))?;
    state.tx_seq += 1;
    write_frame(writer, &envelope).await.map_err(reason_from)
}

/// Writes one layout message: same framing, no lease and no batch display.
async fn write_layout_message<W: AsyncWrite + Unpin>(
    writer: &mut W,
    state: &mut LinkState,
    message: crate::convert::LayoutMessage,
) -> Result<(), LinkDownReason> {
    let envelope = convert::layout_envelope(&message, state.tx_seq, &state.connection_id);
    state.tx_seq += 1;
    write_frame(writer, &envelope).await.map_err(reason_from)
}

async fn write_message<W: AsyncWrite + Unpin>(
    writer: &mut W,
    state: &mut LinkState,
    message: fr::Message,
) -> Result<(), LinkDownReason> {
    let lease_id = convert::lease_id_of_message(&message);
    let seq = state.tx_seq;
    // A lease is entered on one of the peer's displays, and every absolute position the
    // source sends afterwards is local to it; `EnterPrepare` is where that display is
    // known, so it becomes the batch display of this link (see `convert::batch_display_after`).
    let batch_display = convert::batch_display_after(&message, state.batch_display.as_str());
    let envelope = convert::to_envelope(
        &message,
        seq,
        &state.connection_id,
        lease_id.as_ref(),
        batch_display.as_str(),
    )
    .map_err(|error| LinkDownReason::Protocol(error.to_string()))?;
    state.batch_display = batch_display;
    state.tx_seq += 1;
    write_frame(writer, &envelope).await.map_err(reason_from)
}

fn reason_from(error: TransportError) -> LinkDownReason {
    match error {
        TransportError::Protocol(detail) => LinkDownReason::Protocol(detail),
        other => LinkDownReason::Io(other.to_string()),
    }
}

async fn report_down(
    reports: &mpsc::Sender<LinkReport>,
    peer: NodeId,
    direction: Direction,
    reason: LinkDownReason,
) {
    let _ = reports
        .send(LinkReport::Down {
            peer,
            direction,
            reason,
        })
        .await;
}
