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

use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

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

use crate::convert::{self, PeerInfo};
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
///
/// The [`LinkEvent::Pairing`] variant owns a TLS stream, which is neither `Clone` nor
/// `PartialEq`, so this enum deliberately does not derive either.
#[derive(Debug)]
pub enum LinkEvent {
    /// An accepted `pair/1` connection the application must run the ceremony on
    /// (`docs/05-protocol.md`: `pair/1` shares `47901`, so the listener tells it apart by
    /// ALPN). The manager has already admitted the dialer through
    /// [`crate::PairingGate::admit`]; the application runs [`identity::accept_stream`] on
    /// the stream and calls [`crate::PairingGate::finish`] once the ceremony concludes.
    Pairing(Box<crate::pairing::PairingConnection>),
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
        /// The peer's address on this connection. A listener the peer owns — the clipboard
        /// one — is reached at *this* address's IP with the port the peer advertises, not at
        /// the address a trust record remembers (`docs/05-protocol.md`, «Clipboard канал»).
        remote: SocketAddr,
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

impl Stream {
    /// The peer's address on this connection: the *input link's* remote address, which is
    /// the address the peer's listeners are reachable at (`docs/05-protocol.md`).
    pub fn peer_addr(&self) -> std::io::Result<SocketAddr> {
        match self {
            Stream::Client(stream) => stream.get_ref().0.peer_addr(),
            Stream::Server(stream) => stream.get_ref().0.peer_addr(),
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

/// Writes one framed `Envelope` and flushes buffered transports such as TLS.
pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    envelope: &wire::Envelope,
) -> Result<(), TransportError> {
    let bytes = encode_message(Channel::InputControl, envelope)
        .map_err(|error| TransportError::Protocol(format!("{error:?}")))?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    #[test]
    fn connection_trace_is_bounded_and_recovers_a_poisoned_diagnostic_lock() {
        let counters = LeaseTransportCounters::default();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = counters.trace.ring.lock().unwrap();
            panic!("poison diagnostic storage");
        }));
        for nonce in 0..20u8 {
            let envelope = wire::Envelope {
                lease_id: vec![7; 16],
                payload: Some(wire::envelope::Payload::LeaseRenew(wire::LeaseRenew {
                    nonce: vec![nonce; 16],
                })),
                ..Default::default()
            };
            counters.record_trace(&envelope, true, false);
        }
        let snapshot = format!("{:?}", counters.lease_challenge_trace([7; 16]));
        assert_eq!(snapshot.matches("Challenge {").count(), 16);
        assert!(!snapshot.contains("nonce: [0,"));
        assert!(snapshot.contains("nonce: [19,"));
    }

    #[tokio::test]
    async fn read_loop_records_raw_decode_before_coordinator_dispatch() {
        for payload in [
            wire::envelope::Payload::LeaseChallenge(wire::LeaseChallenge {
                nonce: vec![3; 16],
                ttl_ms: 1000,
            }),
            wire::envelope::Payload::LeaseRenew(wire::LeaseRenew { nonce: vec![3; 16] }),
        ] {
            let counters = Arc::new(LeaseTransportCounters::default());
            let (mut sender, receiver) = tokio::io::duplex(4096);
            let (reader, _writer) = tokio::io::split(receiver);
            let (frames_tx, mut frames_rx) = mpsc::channel(1);
            let task = tokio::spawn(read_loop(reader, frames_tx, Arc::clone(&counters)));
            let envelope = wire::Envelope {
                lease_id: vec![7; 16],
                payload: Some(payload),
                ..Default::default()
            };
            write_frame(&mut sender, &envelope).await.unwrap();
            let decoded = frames_rx.recv().await.unwrap().unwrap();
            let raw = format!("{:?}", counters.lease_challenge_trace([7; 16]));
            let raw_stage = match &decoded.payload {
                Some(wire::envelope::Payload::LeaseChallenge(_)) => "challenge_raw_decode",
                _ => "renew_raw_decode",
            };
            assert!(raw.contains(raw_stage));
            assert!(!raw.contains("dispatch"));
            counters.record_trace(&decoded, false, false);
            assert!(format!("{:?}", counters.lease_challenge_trace([7; 16])).contains("dispatch"));
            task.abort();
        }
    }

    #[test]
    fn lease_counters_classify_each_direction_and_ignore_other_payloads() {
        use wire::envelope::Payload;
        let counters = LeaseTransportCounters::default();
        assert_eq!(counters.snapshot(), LeaseTransportSnapshot::default());
        let challenge = Payload::LeaseChallenge(Default::default());
        let renew = Payload::LeaseRenew(Default::default());
        let status = Payload::LeaseStatus(Default::default());
        for (payload, written, decoded) in [(&challenge, 1, 4), (&renew, 2, 5), (&status, 3, 6)] {
            for _ in 0..written {
                counters.record(Some(payload), true);
            }
            for _ in 0..decoded {
                counters.record(Some(payload), false);
            }
        }
        let ping = Payload::Ping(Default::default());
        for written in [true, false] {
            counters.record(None, written);
            counters.record(Some(&ping), written);
        }
        assert_eq!(
            counters.snapshot(),
            LeaseTransportSnapshot {
                challenge_written: 1,
                renew_written: 2,
                status_written: 3,
                challenge_decoded: 4,
                renew_decoded: 5,
                status_decoded: 6,
                ..LeaseTransportSnapshot::default()
            }
        );
    }

    #[test]
    fn input_edge_counters_keep_both_click_edges_across_batches_and_directions() {
        let counters = LeaseTransportCounters::default();
        for state in [wire::KeyState::Down, wire::KeyState::Up] {
            let payload = wire::envelope::Payload::InputBatch(wire::InputBatch {
                events: vec![
                    wire::InputEvent {
                        event: Some(wire::input_event::Event::Button(wire::Button {
                            button: wire::ButtonKind::Left as i32,
                            state: state as i32,
                        })),
                        ..Default::default()
                    },
                    wire::InputEvent {
                        event: Some(wire::input_event::Event::Key(wire::Key {
                            usage_page: 7,
                            usage: 4,
                            state: state as i32,
                        })),
                        ..Default::default()
                    },
                    wire::InputEvent {
                        event: Some(wire::input_event::Event::Scroll(wire::Scroll {
                            horizontal_units120: 0,
                            vertical_units120: 120,
                        })),
                        ..Default::default()
                    },
                ],
            });
            counters.record(Some(&payload), true);
            counters.record(Some(&payload), false);
        }
        let expected = input_model::EdgeCounters {
            keys_down: 1,
            keys_up: 1,
            buttons_down: 1,
            buttons_up: 1,
            wheel_v: 2,
            wheel_h: 0,
        };
        assert_eq!(counters.snapshot().input_tx, expected);
        assert_eq!(counters.snapshot().input_rx, expected);
    }

    #[test]
    fn lease_counters_share_connection_state_but_reset_for_new_connections() {
        let connection = Arc::new(LeaseTransportCounters::default());
        let handle = Arc::clone(&connection);
        let renew = wire::envelope::Payload::LeaseRenew(Default::default());
        connection.record(Some(&renew), true);
        handle.record(Some(&renew), false);
        assert_eq!(handle.snapshot(), connection.snapshot());
        assert_eq!(handle.snapshot().renew_written, 1);
        assert_eq!(handle.snapshot().renew_decoded, 1);
        let next_connection = LeaseTransportCounters::default();
        assert_eq!(
            next_connection.snapshot(),
            LeaseTransportSnapshot::default()
        );
        assert_eq!(handle.snapshot().renew_written, 1);
    }

    struct BufferedWriter {
        pending: Vec<u8>,
        delivered: Vec<u8>,
    }

    impl AsyncWrite for BufferedWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.pending.extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            let pending = std::mem::take(&mut self.pending);
            self.delivered.extend_from_slice(&pending);
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            self.poll_flush(cx)
        }
    }

    #[tokio::test]
    async fn write_frame_flushes_buffered_output() {
        let mut writer = BufferedWriter {
            pending: Vec::new(),
            delivered: Vec::new(),
        };
        let envelope = wire::Envelope {
            major: limits::PROTOCOL_MAJOR,
            minor: limits::PROTOCOL_MINOR,
            connection_id: vec![0; 16],
            seq: 1,
            payload: Some(wire::envelope::Payload::Hello(wire::Hello::default())),
            ..Default::default()
        };
        let expected = encode_message(Channel::InputControl, &envelope).unwrap();

        write_frame(&mut writer, &envelope).await.unwrap();

        assert_eq!(writer.delivered, expected);
    }
}

/// Cumulative lease-frame counters for one connection, reset on reconnect.
///
/// Written means the framed write and flush succeeded; decoded means envelope validation
/// and inbound conversion succeeded. Independent relaxed loads are diagnostic observations,
/// not a transactionally consistent view. Updating and sampling never allocate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LeaseTransportSnapshot {
    /// Successfully flushed lease challenges.
    pub challenge_written: u64,
    /// Successfully flushed lease renewals.
    pub renew_written: u64,
    /// Successfully flushed lease statuses.
    pub status_written: u64,
    /// Validated and converted inbound lease challenges.
    pub challenge_decoded: u64,
    /// Validated and converted inbound lease renewals.
    pub renew_decoded: u64,
    /// Validated and converted inbound lease statuses.
    pub status_decoded: u64,
    pub input_tx: input_model::EdgeCounters,
    pub input_rx: input_model::EdgeCounters,
}

#[derive(Debug, Default)]
pub(crate) struct LeaseTransportCounters {
    trace: LeaseConnectionTrace,
    challenge_written: AtomicU64,
    renew_written: AtomicU64,
    status_written: AtomicU64,
    challenge_decoded: AtomicU64,
    renew_decoded: AtomicU64,
    status_decoded: AtomicU64,
    input_tx: [AtomicU64; 6],
    input_rx: [AtomicU64; 6],
}

#[derive(Debug)]
struct LeaseConnectionTrace {
    started: tokio::time::Instant,
    ring: Mutex<fr::LeaseTraceRing>,
}

impl Default for LeaseConnectionTrace {
    fn default() -> Self {
        Self {
            started: tokio::time::Instant::now(),
            ring: Mutex::new(fr::LeaseTraceRing::default()),
        }
    }
}

impl LeaseTransportCounters {
    /// Records only lease challenge/renew stages: unrelated frames never lock the ring.
    fn record_trace(&self, envelope: &wire::Envelope, raw: bool, written: bool) {
        use fr::LeaseTraceStage;
        use wire::envelope::Payload;
        let (nonce, stage) = match (envelope.payload.as_ref(), raw, written) {
            (Some(Payload::LeaseChallenge(value)), true, false) => {
                (&value.nonce, LeaseTraceStage::ChallengeRawDecode)
            }
            (Some(Payload::LeaseChallenge(value)), false, false) => {
                (&value.nonce, LeaseTraceStage::ChallengeDispatch)
            }
            (Some(Payload::LeaseRenew(value)), true, false) => {
                (&value.nonce, LeaseTraceStage::RenewRawDecode)
            }
            (Some(Payload::LeaseRenew(value)), false, false) => {
                (&value.nonce, LeaseTraceStage::RenewDispatch)
            }
            (Some(Payload::LeaseRenew(value)), false, true) => {
                (&value.nonce, LeaseTraceStage::RenewWrite)
            }
            _ => return,
        };
        let (Ok(lease_id), Ok(nonce)) = (
            envelope.lease_id.as_slice().try_into(),
            nonce.as_slice().try_into(),
        ) else {
            return;
        };
        let now_ms = self.trace.started.elapsed().as_millis() as u64;
        self.trace
            .ring
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .record(lease_id, nonce, stage, now_ms);
    }

    pub(crate) fn lease_challenge_trace(&self, lease_id: fr::LeaseId) -> fr::LeaseTraceSnapshot {
        let now_ms = self.trace.started.elapsed().as_millis() as u64;
        self.trace
            .ring
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .snapshot(lease_id, now_ms)
    }

    pub(crate) fn record(&self, payload: Option<&wire::envelope::Payload>, written: bool) {
        use wire::envelope::Payload;
        if let Some(Payload::InputBatch(batch)) = payload {
            let counters = if written {
                &self.input_tx
            } else {
                &self.input_rx
            };
            for event in &batch.events {
                let (offset, state) = match &event.event {
                    Some(wire::input_event::Event::Key(key)) => (0, key.state),
                    Some(wire::input_event::Event::Button(button)) => (2, button.state),
                    // The wire's `Scroll` has no edge: it counts one per axis that carried
                    // units, so a wheel that never arrives is visible at the link too.
                    Some(wire::input_event::Event::Scroll(scroll)) => {
                        if scroll.vertical_units120 != 0 {
                            counters[4].fetch_add(1, Ordering::Relaxed);
                        }
                        if scroll.horizontal_units120 != 0 {
                            counters[5].fetch_add(1, Ordering::Relaxed);
                        }
                        continue;
                    }
                    _ => continue,
                };
                let index = match wire::KeyState::try_from(state) {
                    Ok(wire::KeyState::Down) => offset,
                    Ok(wire::KeyState::Up) => offset + 1,
                    _ => continue,
                };
                counters[index].fetch_add(1, Ordering::Relaxed);
            }
            return;
        }
        let counter = match (payload, written) {
            (Some(Payload::LeaseChallenge(_)), true) => &self.challenge_written,
            (Some(Payload::LeaseRenew(_)), true) => &self.renew_written,
            (Some(Payload::LeaseStatus(_)), true) => &self.status_written,
            (Some(Payload::LeaseChallenge(_)), false) => &self.challenge_decoded,
            (Some(Payload::LeaseRenew(_)), false) => &self.renew_decoded,
            (Some(Payload::LeaseStatus(_)), false) => &self.status_decoded,
            _ => return,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn snapshot(&self) -> LeaseTransportSnapshot {
        LeaseTransportSnapshot {
            challenge_written: self.challenge_written.load(Ordering::Relaxed),
            renew_written: self.renew_written.load(Ordering::Relaxed),
            status_written: self.status_written.load(Ordering::Relaxed),
            challenge_decoded: self.challenge_decoded.load(Ordering::Relaxed),
            renew_decoded: self.renew_decoded.load(Ordering::Relaxed),
            status_decoded: self.status_decoded.load(Ordering::Relaxed),
            input_tx: edge_snapshot(&self.input_tx),
            input_rx: edge_snapshot(&self.input_rx),
        }
    }
}

fn edge_snapshot(counters: &[AtomicU64; 6]) -> input_model::EdgeCounters {
    input_model::EdgeCounters {
        keys_down: counters[0].load(Ordering::Relaxed),
        keys_up: counters[1].load(Ordering::Relaxed),
        buttons_down: counters[2].load(Ordering::Relaxed),
        buttons_up: counters[3].load(Ordering::Relaxed),
        wheel_v: counters[4].load(Ordering::Relaxed),
        wheel_h: counters[5].load(Ordering::Relaxed),
    }
}

/// A cloneable handle to one link's outbound queue.
#[derive(Debug, Clone)]
pub struct LinkHandle {
    peer: NodeId,
    tx: mpsc::Sender<Outbound>,
    dropped_motion: Arc<AtomicU64>,
    lease_diagnostics: Arc<LeaseTransportCounters>,
    /// Ends the link's task without depending on the queue's capacity.
    ///
    /// The bounded outbound queue is the normal path (`close` puts a `Shutdown` in it), but a
    /// saturated queue must not be able to swallow the request: the link would then run on
    /// with nothing left to close it, and the peer would see the socket go instead of a
    /// `Shutdown` (audit F3).
    shutdown: Arc<tokio::sync::Notify>,
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
            Outbound::Motion { lease_id, events } => {
                self.send_motion(lease_id, events);
                Ok(())
            }
            Outbound::Guaranteed(message) => {
                static LOG_RATE: logging::LogRateLimit = logging::LogRateLimit::new();
                let started = tracing::enabled!(tracing::Level::DEBUG).then(Instant::now);
                let result = self.tx.send(Outbound::Guaranteed(message)).await;
                let elapsed = started.map_or(Duration::ZERO, |start| start.elapsed());
                if elapsed >= Duration::from_millis(10)
                    && tracing::enabled!(tracing::Level::DEBUG)
                    && LOG_RATE.allow()
                {
                    tracing::debug!(
                        peer = ?self.peer,
                        elapsed_ms = elapsed.as_millis(),
                        remaining_capacity = self.tx.capacity(),
                        "outbound queue blocked"
                    );
                }
                result.map_err(|_| TransportError::LinkClosed)
            }
            // `outbound_of` never produces this arm; it exists because `Outbound` is shared.
            Outbound::Layout(message) => self.send_layout(message).await,
            // The same: a clipboard message is queued through `send_clipboard`, never here.
            Outbound::Clipboard(message) => self.send_clipboard(message).await,
            Outbound::CapabilitiesChanged(capabilities) => {
                self.send_capabilities(capabilities).await
            }
        }
    }

    /// Sends the replacement capability set on the bounded, guaranteed control queue.
    pub async fn send_capabilities(&self, capabilities: Vec<i32>) -> Result<(), TransportError> {
        self.tx
            .send(Outbound::CapabilitiesChanged(capabilities))
            .await
            .map_err(|_| TransportError::LinkClosed)
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
    ///
    /// `lease_id` is the lease the motion belongs to and rides with it into the queue so the
    /// written batch keeps it (see [`Outbound::Motion`]).
    pub fn send_motion(&self, lease_id: [u8; 16], events: Vec<InputEvent>) -> bool {
        if events.is_empty() {
            return true;
        }
        match self.tx.try_send(Outbound::Motion { lease_id, events }) {
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

    /// Samples this connection's cumulative lease counters without allocating.
    pub fn lease_diagnostics(&self) -> LeaseTransportSnapshot {
        self.lease_diagnostics.snapshot()
    }

    /// Final diagnostic snapshot, with stage ages relative to this connection's clock.
    /// Retains at most sixteen distinct lease/nonce pairs; sampling may allocate.
    pub fn lease_challenge_trace(&self, lease_id: fr::LeaseId) -> fr::LeaseTraceSnapshot {
        self.lease_diagnostics.lease_challenge_trace(lease_id)
    }

    pub(crate) fn lease_diagnostics_counters(&self) -> Arc<LeaseTransportCounters> {
        Arc::clone(&self.lease_diagnostics)
    }

    #[cfg(test)]
    pub(crate) fn for_counter_test(peer: NodeId) -> Self {
        let (tx, _rx) = mpsc::channel(1);
        Self {
            peer,
            tx,
            dropped_motion: Arc::new(AtomicU64::new(0)),
            lease_diagnostics: Arc::new(LeaseTransportCounters::default()),
            shutdown: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Asks the link to close: a `Shutdown` is queued, and the task ends after writing it.
    ///
    /// The queued frame is the in-order path (whatever is already queued goes out first). If
    /// the queue is saturated the request must not be lost: the link's own task is the only
    /// writer, so it is signalled instead and writes the `Shutdown` itself.
    pub fn close(&self) {
        let shutdown = Outbound::Guaranteed(fr::Message::Shutdown {
            reason: fr::LeaveReason::Shutdown,
        });
        if self.tx.try_send(shutdown).is_err() {
            self.shutdown.notify_one();
        }
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
        /// The key the manager gave this link (see [`LinkConfig::key`]).
        key: [u8; 16],
        /// The peer.
        peer: NodeId,
        /// Its gossip.
        hello: PeerInfo,
        /// The negotiated `connection_id` (see [`LinkEvent::Up`]).
        connection_id: [u8; 16],
        /// The peer's address on this connection (see [`LinkEvent::Up`]).
        remote: SocketAddr,
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
        /// The key the manager gave this link (see [`LinkConfig::key`]), so this report can
        /// only remove the link that sent it — not a live link in the same direction.
        key: [u8; 16],
        /// The peer.
        peer: NodeId,
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
    /// What the manager knows this link by.
    ///
    /// Unique per spawned link, and echoed in every [`LinkReport`], so a report can only ever
    /// change the state of the link that sent it. `(peer, direction)` cannot do that: a peer
    /// that dials twice is two links in one direction, and the late one used to overwrite the
    /// earlier one's bookkeeping.
    pub key: [u8; 16],
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
    let lease_diagnostics = Arc::new(LeaseTransportCounters::default());
    let shutdown = Arc::new(tokio::sync::Notify::new());
    let handle = LinkHandle {
        peer: config.peer,
        tx,
        dropped_motion,
        lease_diagnostics: Arc::clone(&lease_diagnostics),
        shutdown: Arc::clone(&shutdown),
    };
    let task = tokio::spawn(run(config, rx, lease_diagnostics, shutdown));
    Ok(Link { handle, task })
}

async fn run(
    config: LinkConfig,
    outbound: mpsc::Receiver<Outbound>,
    lease_diagnostics: Arc<LeaseTransportCounters>,
    shutdown: Arc<tokio::sync::Notify>,
) {
    let LinkConfig {
        key,
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
                    key,
                    peer,
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

    // Read before the split: both halves are needed afterwards, and the address is what a
    // later clipboard dial is built from.
    let remote = stream.peer_addr().ok();
    let (reader, mut writer) = tokio::io::split(stream);
    let (frames_tx, mut frames_rx) = mpsc::channel::<Result<wire::Envelope, TransportError>>(8);
    let reader_task = tokio::spawn(read_loop(reader, frames_tx, Arc::clone(&lease_diagnostics)));

    let mut state = LinkState::new(peer, connection_id, key, lease_diagnostics);
    state.remote = remote;

    // The dialer speaks first: its `Hello` is the first frame of its direction (`seq = 1`).
    if direction.is_outgoing() {
        match state
            .write_hello(&mut writer, &mut handshake, local_hello.clone())
            .await
        {
            Ok(()) => {}
            Err(reason) => {
                reader_task.abort();
                report_down(&reports, key, peer, reason).await;
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
            /// `close()` could not use the queue (it was full): the request came out of band.
            Shutdown,
        }

        let step = tokio::select! {
            frame = frames_rx.recv() => Step::Frame(frame),
            item = outbound.recv() => Step::Outbound(item),
            () = tokio::time::sleep(sleep) => Step::Tick,
            () = shutdown.notified() => Step::Shutdown,
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
            Step::Shutdown => {
                // `close()` found the queue full. This task is the link's only writer, so the
                // frame is written here rather than lost (audit F3); the peer sees a
                // `Shutdown` instead of a bare socket close. Failing to write it (a dead
                // socket) still ends the link.
                //
                // What the queue already holds goes first: a queued key or button edge must
                // not be lost to a close, and `select!` would otherwise be free to pick this
                // branch before `Step::Outbound` and drop it (found by the link_close test
                // under load: the peer read the `Hello` and the `Shutdown` but not the motion
                // batch that was waiting between them).
                if let Ok(first) = outbound.try_recv() {
                    if let Err(reason) =
                        drain_outbound(&mut writer, &mut outbound, first, &mut state).await
                    {
                        break reason;
                    }
                }
                let _ = write_message(
                    &mut writer,
                    &mut state,
                    fr::Message::Shutdown {
                        reason: fr::LeaveReason::Shutdown,
                    },
                )
                .await;
                break LinkDownReason::LocalClosed;
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

    if reason == LinkDownReason::LocalClosed {
        // Our own close, so the frames we just wrote are ours to deliver: a `Shutdown`, or the
        // membership refusal that made the manager close this link. Aborting the reader and
        // dropping the socket here is not enough — on Windows a close with anything of the
        // peer's still unread sends a **reset**, and a reset discards the frames the peer has
        // not read yet. The refused member then reported `Io(... 10054)` instead of
        // `MEMBERSHIP_FULL` (1 failure in ~100 loaded runs of
        // `mesh::the_membership_cap_refuses_the_extra_member`; every failure had that shape).
        //
        // So: TLS `close_notify` (the peer sees a clean end, not a truncation), then read what
        // the peer already sent until it ends or the grace runs out, so the receive queue is
        // empty when the socket goes and the OS has no reason to reset it.
        let _ = tokio::time::timeout(GRACEFUL_CLOSE_GRACE, writer.shutdown()).await;
        let _ = tokio::time::timeout(GRACEFUL_CLOSE_GRACE, async {
            while frames_rx.recv().await.is_some() {}
        })
        .await;
    }
    reader_task.abort();
    report_down(&reports, key, peer, reason).await;
}

/// How long a link we are closing ourselves gives the peer to read its last frames.
///
/// Bounded, and short: the peer is either reading or it is gone, and a link that refuses to
/// end would hold the session's cleanup behind it.
const GRACEFUL_CLOSE_GRACE: Duration = Duration::from_millis(1_000);

/// The coordinator's mutable state.
struct LinkState {
    peer: NodeId,
    /// The key the manager gave this link (see [`LinkConfig::key`]).
    key: [u8; 16],
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
    /// The peer's address on this connection, when the socket reports one.
    remote: Option<SocketAddr>,
    last_batch_log: Option<tokio::time::Instant>,
    last_ack_log: Option<tokio::time::Instant>,
    lease_diagnostics: Arc<LeaseTransportCounters>,
}

impl LinkState {
    fn new(
        peer: NodeId,
        connection_id: [u8; 16],
        key: [u8; 16],
        lease_diagnostics: Arc<LeaseTransportCounters>,
    ) -> Self {
        Self {
            peer,
            key,
            tx_seq: 1,
            connection_id,
            announced: false,
            started: lease_diagnostics.trace.started,
            last_pong_ms: 0,
            last_ping_ms: None,
            ping_nonce: 0,
            batch_display: String::new(),
            rx_seq: SeqValidator::new(),
            remote: None,
            last_batch_log: None,
            last_ack_log: None,
            lease_diagnostics,
        }
    }
    fn report_input_batch(&mut self, direction: &'static str, seq: u64, events: &[InputEvent]) {
        let now = tokio::time::Instant::now();
        if self
            .last_batch_log
            .is_some_and(|last| now.duration_since(last) < Duration::from_secs(1))
        {
            return;
        }
        self.last_batch_log = Some(now);
        let mut keys = 0;
        let mut pointers = 0;
        let mut buttons = 0;
        let mut scrolls = 0;
        for event in events {
            match event {
                InputEvent::Key { .. } => keys += 1,
                InputEvent::PointerAbsolute(_) | InputEvent::PointerRelative { .. } => {
                    pointers += 1;
                }
                InputEvent::Button { .. } => buttons += 1,
                InputEvent::Scroll { .. } => scrolls += 1,
            }
        }
        tracing::info!(
            peer = %self.peer,
            direction,
            seq,
            events = events.len(),
            keys,
            pointers,
            buttons,
            scrolls,
            "transport InputBatch"
        );
    }

    fn report_inject_ack(
        &mut self,
        direction: &'static str,
        seq: u64,
        input_batch_seq: u64,
        applied_count: u32,
        status: &fr::InjectStatus,
    ) {
        let now = tokio::time::Instant::now();
        if self
            .last_ack_log
            .is_some_and(|last| now.duration_since(last) < Duration::from_secs(1))
        {
            return;
        }
        self.last_ack_log = Some(now);
        tracing::info!(
            peer = %self.peer,
            direction,
            seq,
            input_batch_seq,
            applied_count,
            ?status,
            "transport InjectAck"
        );
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
        self.lease_diagnostics.record_trace(envelope, false, false);
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
                    key: self.key,
                    peer: self.peer,
                    hello: info,
                    connection_id: self.connection_id,
                    remote: self.remote.expect("a connected socket has a peer address"),
                })
                .await
                .map_err(|_| LinkDownReason::LocalClosed);
        }

        validate::envelope(envelope)
            .map_err(|error| LinkDownReason::Protocol(format!("{error:?}")))?;
        match &envelope.payload {
            Some(wire::envelope::Payload::CapabilitiesChanged(_))
                if envelope.connection_id.as_slice() != self.connection_id
                    || envelope.major != protocol::limits::PROTOCOL_MAJOR
                    || !envelope.lease_id.is_empty() =>
            {
                return Err(LinkDownReason::Protocol(
                    "capability update does not belong to this input connection".into(),
                ));
            }
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
        // Recorded before the vocabulary check: the diagnostics counter describes what arrived
        // on the wire, whether or not this build could read it (audit F4 keeps the link up).
        self.lease_diagnostics
            .record(envelope.payload.as_ref(), false);
        let message = match convert::inbound_from_envelope(envelope) {
            Ok(message) => message,
            Err(error) => {
                // A payload this node cannot read — an event with no wire form, an absolute
                // position without a display id, a vocabulary member from another build — is
                // that frame's problem, not the link's: dropping it costs one batch, while
                // closing the connection costs every lease on it (audit F4). Framing and
                // sequence violations are still refused above and end the link.
                static LOG_RATE: logging::LogRateLimit = logging::LogRateLimit::new();
                if LOG_RATE.allow() {
                    tracing::warn!(
                        peer = ?self.peer,
                        seq = envelope.seq,
                        error = %error,
                        "a frame could not be read into the protocol vocabulary; the frame was dropped"
                    );
                }
                return Ok(());
            }
        };
        match &message {
            convert::Inbound::Focus(fr::Message::InputBatch { events, .. }) => {
                self.report_input_batch("rx", envelope.seq, events);
            }
            convert::Inbound::Focus(fr::Message::InjectAck {
                input_batch_seq,
                applied_count,
                status,
                ..
            }) => {
                self.report_inject_ack("rx", envelope.seq, *input_batch_seq, *applied_count, status)
            }
            _ => {}
        }
        let report_started = tracing::enabled!(tracing::Level::DEBUG).then(Instant::now);
        static LOG_RATE: logging::LogRateLimit = logging::LogRateLimit::new();
        let result = reports
            .send(LinkReport::Message {
                peer: self.peer,
                message,
            })
            .await;
        let report_wait = report_started.map_or(Duration::ZERO, |start| start.elapsed());
        if report_wait >= Duration::from_millis(10)
            && tracing::enabled!(tracing::Level::DEBUG)
            && LOG_RATE.allow()
        {
            tracing::debug!(
                peer = ?self.peer,
                seq = envelope.seq,
                elapsed_ms = report_wait.as_millis(),
                remaining_capacity = reports.capacity(),
                "link report queue blocked"
            );
        }
        result.map_err(|_| LinkDownReason::LocalClosed)
    }
}

async fn read_loop<R: AsyncRead + Unpin>(
    mut reader: ReadHalf<R>,
    frames: mpsc::Sender<Result<wire::Envelope, TransportError>>,
    lease_diagnostics: Arc<LeaseTransportCounters>,
) {
    loop {
        let frame = read_frame(&mut reader).await;
        if let Ok(envelope) = &frame {
            lease_diagnostics.record_trace(envelope, true, false);
        }
        let stop = frame.is_err();
        let queued = frames.send(frame).await;
        if queued.is_err() || stop {
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
            Outbound::CapabilitiesChanged(capabilities) => {
                let envelope = state.envelope(wire::envelope::Payload::CapabilitiesChanged(
                    wire::CapabilitiesChanged {
                        capabilities: capabilities.clone(),
                    },
                ));
                write_frame(writer, &envelope).await.map_err(reason_from)?;
                index += 1;
            }
            Outbound::Motion { .. } => {
                // Merge the contiguous motion run that shares this lease. Motion from
                // different leases is never merged: the receiver checks the lease of every
                // batch, and a batch that mixed two leases (or lost the lease) would be
                // refused as a `LeaseMismatch`.
                let lease_id = match &queued[index] {
                    Outbound::Motion { lease_id, .. } => *lease_id,
                    _ => unreachable!(),
                };
                let mut events = Vec::new();
                while let Some(Outbound::Motion {
                    lease_id: next_lease,
                    events: next,
                }) = queued.get(index)
                {
                    if *next_lease != lease_id {
                        break;
                    }
                    events.extend(next.iter().copied());
                    index += 1;
                }
                let batch = queue::motion_batch(lease_id, events);
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
    let seq = state.tx_seq;
    // A batch carrying an event this layer cannot express must not fail — and must not be
    // able to end the link (audit F4). The events with a wire form still travel, so a key or
    // button edge in the same batch is not lost; only the unrepresentable ones (a relative
    // pointer delta) are dropped, and the count is logged rate-limited.
    let message = match message {
        fr::Message::InputBatch {
            lease_id,
            input_batch_seq,
            events,
        } => {
            let (events, dropped) = convert::split_encodable(&events);
            if dropped > 0 {
                static LOG_RATE: logging::LogRateLimit = logging::LogRateLimit::new();
                if LOG_RATE.allow() {
                    tracing::warn!(
                        peer = ?state.peer,
                        dropped,
                        kept = events.len(),
                        "input events with no wire form were dropped from the batch"
                    );
                }
            }
            if events.is_empty() {
                // Nothing this layer can send: the frame is skipped, the link stays up.
                return Ok(());
            }
            fr::Message::InputBatch {
                lease_id,
                input_batch_seq,
                events,
            }
        }
        other => other,
    };
    let lease_id = convert::lease_id_of_message(&message);
    // A lease is entered on one of the peer's displays, and every absolute position the
    // source sends afterwards is local to it; `EnterPrepare` is where that display is
    // known, so it becomes the batch display of this link (see `convert::batch_display_after`).
    let batch_display = convert::batch_display_after(&message, state.batch_display.as_str());
    let envelope = match convert::to_envelope(
        &message,
        seq,
        &state.connection_id,
        lease_id.as_ref(),
        batch_display.as_str(),
    ) {
        Ok(envelope) => envelope,
        Err(error) => {
            // Unreachable for an `InputBatch` (the events were split above), and no other
            // message has an unrepresentable field: a conversion refusal is a programming
            // error, not a peer's, so it must not take the link down (audit F4).
            static LOG_RATE: logging::LogRateLimit = logging::LogRateLimit::new();
            if LOG_RATE.allow() {
                tracing::error!(
                    peer = ?state.peer,
                    seq,
                    error = %error,
                    "a message could not be encoded; the frame was skipped"
                );
            }
            return Ok(());
        }
    };
    match &message {
        fr::Message::InputBatch { events, .. } => {
            state.report_input_batch("tx", seq, events);
        }
        fr::Message::InjectAck {
            input_batch_seq,
            applied_count,
            status,
            ..
        } => state.report_inject_ack("tx", seq, *input_batch_seq, *applied_count, status),
        _ => {}
    }
    state.batch_display = batch_display;
    state.tx_seq += 1;
    write_frame(writer, &envelope).await.map_err(reason_from)?;
    state.lease_diagnostics.record_trace(&envelope, false, true);
    state
        .lease_diagnostics
        .record(envelope.payload.as_ref(), true);
    Ok(())
}

fn reason_from(error: TransportError) -> LinkDownReason {
    match error {
        TransportError::Protocol(detail) => LinkDownReason::Protocol(detail),
        other => LinkDownReason::Io(other.to_string()),
    }
}

async fn report_down(
    reports: &mpsc::Sender<LinkReport>,
    key: [u8; 16],
    peer: NodeId,
    reason: LinkDownReason,
) {
    let _ = reports.send(LinkReport::Down { key, peer, reason }).await;
}
