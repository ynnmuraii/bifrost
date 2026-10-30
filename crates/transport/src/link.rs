//! One peer's input/control link: TLS, `Hello`, framed envelopes, liveness.
//!
//! A cloneable handle admits bounded intents without waiting for socket capacity.
//! Reader, coordinator and writer are independent tasks. The writer exclusively owns the
//! WriteHalf, outbound sequence and display state; whole frames are never interleaved.
//! The reader never cancels a partially consumed frame. Terminal timeouts destroy the
//! stream rather than attempting to resume a partial write.

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
use crate::input_metrics::{AckTracker, Counter, InputMetrics, Stage};
use crate::policy::{self, LivenessAction};
use crate::queue::{InputQueue, InputSample, Intent, Outbound};
use crate::rtt::{RttMeter, RttSnapshot};
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
    /// A prompt-only request channel; never registered as a normal link.
    PairingRequest(Box<crate::pairing::RequestConnection>),
    /// The remote TLS endpoint explicitly rejected our certificate as unknown.
    PeerRejected { peer: NodeId },
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

/// The input stream is terminal after either deadline; partial frames are never resumed.
pub const INPUT_WRITE_IDLE_TIMEOUT: Duration = Duration::from_millis(600);
pub const INPUT_FRAME_DEADLINE: Duration = Duration::from_millis(1000);
pub const GRACEFUL_CLOSE_BUDGET: Duration = Duration::from_millis(100);

pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    envelope: &wire::Envelope,
) -> Result<(), TransportError> {
    let bytes = encode_message(Channel::InputControl, envelope)
        .map_err(|error| TransportError::Protocol(format!("{error:?}")))?;
    let deadline = tokio::time::Instant::now() + INPUT_FRAME_DEADLINE;
    let mut idle = tokio::time::Instant::now() + INPUT_WRITE_IDLE_TIMEOUT;
    let mut offset = 0;
    while offset < bytes.len() {
        let n = tokio::select! {
            biased;
            () = tokio::time::sleep_until(deadline) => return Err(write_timeout("frame deadline")),
            () = tokio::time::sleep_until(idle) => return Err(write_timeout("idle timeout")),
            result = writer.write(&bytes[offset..]) => result?,
        };
        if n == 0 {
            return Err(std::io::Error::from(std::io::ErrorKind::WriteZero).into());
        }
        offset += n;
        idle = tokio::time::Instant::now() + INPUT_WRITE_IDLE_TIMEOUT;
    }
    tokio::select! {
        biased;
        () = tokio::time::sleep_until(deadline) => Err(write_timeout("frame deadline")),
        () = tokio::time::sleep_until(idle) => Err(write_timeout("idle timeout")),
        result = writer.flush() => result.map_err(Into::into),
    }
}

fn write_timeout(reason: &'static str) -> TransportError {
    std::io::Error::new(std::io::ErrorKind::TimedOut, reason).into()
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
            let (decoded, _) = frames_rx.recv().await.unwrap().unwrap();
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
    local: NodeId,
    queue: Arc<InputQueue>,
    terminal: tokio::sync::watch::Sender<Option<LinkDownReason>>,
    dropped_motion: Arc<AtomicU64>,
    lease_diagnostics: Arc<LeaseTransportCounters>,
    metrics: Arc<Mutex<InputMetrics>>,
    ack_tracker: Arc<Mutex<AckTracker>>,
    /// This connection's `Ping`/`Pong` measurements; a reconnect starts a new one.
    rtt: Arc<Mutex<RttMeter>>,
    /// Ends the link's task without depending on the queue's capacity.
    ///
    /// Closing is out-of-band even when all bounded queues are full.
    shutdown: Arc<tokio::sync::Notify>,
}

impl LinkHandle {
    /// Published aggregate diagnostics contain durations and counters only.
    pub fn input_metrics(&self) -> crate::input_metrics::InputMetricsSnapshot {
        self.metrics
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .snapshot()
    }

    /// Immediately invalidates pending input without delaying local release.
    pub fn abort_input_lease(&self, lease_id: [u8; 16]) {
        let discarded = self.queue.abort_lease(lease_id);
        self.metrics
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .increment(Counter::MotionDiscardedOnLeaseEnd, discarded);
        self.ack_tracker
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear_lease(0, lease_id);
    }
    /// Success means the notice was flushed, not merely admitted.
    pub async fn send_unpaired(&self, notice: wire::Unpaired) -> Result<(), TransportError> {
        if !self.queue.unpair_allowed()
            || notice.requester_node_id.as_slice() != self.local.as_bytes()
        {
            return Err(TransportError::LinkClosed);
        }
        let (reply, mut result) = tokio::sync::oneshot::channel();
        let mut terminal = self.terminal.subscribe();
        self.admit(self.queue.urgent(Intent::Unpair(notice, reply)))?;
        let deadline = self.queue.begin_close();
        self.shutdown.notify_one();
        let result = tokio::time::timeout_at(deadline, async {
            loop {
                tokio::select! {
                    biased;
                    result = &mut result => return result.unwrap_or(false),
                    _ = terminal.changed() => {
                        if terminal.borrow().as_ref().is_some_and(|reason| *reason != LinkDownReason::LocalClosed) {
                            return false;
                        }
                    }
                }
            }
        }).await.unwrap_or(false);
        if result {
            Ok(())
        } else {
            Err(TransportError::LinkClosed)
        }
    }

    /// The peer this handle talks to.
    pub const fn peer(&self) -> &NodeId {
        &self.peer
    }

    /// Admission never waits for network capacity. Critical overflow fails closed.
    pub async fn send(&self, message: fr::Message) -> Result<(), TransportError> {
        match message {
            fr::Message::InputBatch {
                lease_id, events, ..
            } => {
                self.send_timed_input(
                    lease_id,
                    events.into_iter().map(|e| (e, None)).collect(),
                    None,
                )
                .await
            }
            message => {
                let closing = matches!(message, fr::Message::Shutdown { .. });
                let result = self.admit(self.queue.push(Outbound::Guaranteed(message)));
                if result.is_ok() && closing {
                    self.close();
                }
                result
            }
        }
    }

    fn admit(&self, result: Result<(), TransportError>) -> Result<(), TransportError> {
        if let Err(TransportError::Protocol(reason)) = &result {
            self.metrics
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .increment(Counter::CriticalOverflow, 1);
            set_terminal_once(&self.terminal, || LinkDownReason::Protocol(reason.clone()));
            self.queue.abort();
        }
        if result.is_ok() {
            self.metrics
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .increment(Counter::QueueHighWater, self.queue.depth() as u64);
        }
        result
    }

    /// Capture metadata is source-host diagnostic time, never receiver-clock time.
    pub async fn send_timed_input(
        &self,
        lease_id: [u8; 16],
        events: Vec<(InputEvent, Option<u64>)>,
        enqueue_host_us: Option<u64>,
    ) -> Result<(), TransportError> {
        let now = tokio::time::Instant::now();
        let samples = events
            .into_iter()
            .map(|(event, capture)| {
                let capture = capture.filter(|value| *value != 0);
                InputSample {
                    event,
                    capture_host_us: capture,
                    capture_to_enqueue_us: enqueue_host_us
                        .zip(capture)
                        .and_then(|(now, capture)| now.checked_sub(capture)),
                    enqueued_at: now,
                }
            })
            .collect::<Vec<_>>();
        {
            let mut metrics = self.metrics.lock().unwrap_or_else(|e| e.into_inner());
            for sample in &samples {
                if let Some(age) = sample.capture_to_enqueue_us {
                    metrics.record(Stage::CaptureToEnqueue, age);
                } else {
                    metrics.unknown(Stage::CaptureToEnqueue);
                }
            }
        }
        let result = self.queue.input(lease_id, samples);
        match result {
            Ok(overwritten) => {
                self.dropped_motion
                    .fetch_add(overwritten, Ordering::Relaxed);
                self.metrics
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .increment(Counter::MotionOverwritten, overwritten);
                self.metrics
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .increment(Counter::Coalesced, overwritten);
                self.metrics
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .increment(Counter::QueueHighWater, self.queue.depth() as u64);
                Ok(())
            }
            Err(error) => self.admit(Err(error)),
        }
    }

    pub async fn send_capabilities(&self, capabilities: Vec<i32>) -> Result<(), TransportError> {
        self.admit(self.queue.push(Outbound::CapabilitiesChanged(capabilities)))
    }
    pub async fn send_update_request(
        &self,
        request: wire::UpdateRequest,
    ) -> Result<(), TransportError> {
        convert::validate_update_version(&request.target_version)
            .map_err(|e| TransportError::Protocol(e.to_string()))?;
        self.admit(self.queue.push(Outbound::UpdateRequest(request)))
    }
    pub async fn send_update_result(
        &self,
        result: wire::UpdateResult,
    ) -> Result<(), TransportError> {
        convert::validate_update_result(&result)
            .map_err(|e| TransportError::Protocol(e.to_string()))?;
        self.admit(self.queue.push(Outbound::UpdateResult(result)))
    }
    pub async fn send_clipboard_applied(
        &self,
        applied: wire::ClipboardApplied,
    ) -> Result<(), TransportError> {
        convert::validate_clipboard_applied(&applied)
            .map_err(|e| TransportError::Protocol(e.to_string()))?;
        self.admit(self.queue.push(Outbound::ClipboardApplied(applied)))
    }
    pub async fn send_clipboard_route(
        &self,
        route: wire::ClipboardRoute,
    ) -> Result<(), TransportError> {
        convert::validate_clipboard_route(&route)
            .map_err(|e| TransportError::Protocol(e.to_string()))?;
        self.admit(self.queue.push(Outbound::ClipboardRoute(route)))
    }
    pub async fn send_layout(
        &self,
        message: crate::convert::LayoutMessage,
    ) -> Result<(), TransportError> {
        self.admit(self.queue.push(Outbound::Layout(message)))
    }
    pub async fn send_clipboard(
        &self,
        message: crate::clipboard::ClipboardMessage,
    ) -> Result<(), TransportError> {
        self.admit(self.queue.push(Outbound::Clipboard(message)))
    }

    /// Replaces the latest absolute position even while the ordered FIFO is full.
    pub fn send_motion(&self, lease_id: [u8; 16], events: Vec<InputEvent>) -> bool {
        if events
            .iter()
            .any(|event| !matches!(event, InputEvent::PointerAbsolute(_)))
        {
            return false;
        }
        let sample_count = events.len();
        let now = tokio::time::Instant::now();
        let samples = events
            .into_iter()
            .map(|event| InputSample {
                event,
                capture_host_us: None,
                capture_to_enqueue_us: None,
                enqueued_at: now,
            })
            .collect();
        match self.queue.input(lease_id, samples) {
            Ok(overwritten) => {
                self.dropped_motion
                    .fetch_add(overwritten, Ordering::Relaxed);
                let mut metrics = self.metrics.lock().unwrap_or_else(|e| e.into_inner());
                metrics.increment(Counter::MotionOverwritten, overwritten);
                metrics.increment(Counter::Coalesced, overwritten);
                metrics.increment(Counter::QueueHighWater, self.queue.depth() as u64);
                for _ in 0..sample_count {
                    metrics.unknown(Stage::CaptureToEnqueue);
                }
                true
            }
            Err(error) => {
                let _ = self.admit(Err(error));
                false
            }
        }
    }

    /// Superseded motion samples (the latest resting point is retained, not dropped).
    pub fn dropped_motion_batches(&self) -> u64 {
        self.dropped_motion.load(Ordering::Relaxed)
    }

    /// Samples this connection's cumulative lease counters without allocating.
    pub fn lease_diagnostics(&self) -> LeaseTransportSnapshot {
        self.lease_diagnostics.snapshot()
    }

    /// The latency this connection measured over its last
    /// [`crate::rtt::WINDOW`], as of now.
    ///
    /// Zero fields mean *unknown* (fewer than [`crate::rtt::MIN_SAMPLES`] answered pings), and
    /// the measurement starts again from nothing when the link reconnects: latency describes a
    /// connection, not a peer.
    pub fn rtt(&self) -> RttSnapshot {
        self.rtt
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .snapshot(Instant::now())
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
        let (terminal, _) = tokio::sync::watch::channel(None);
        Self {
            peer,
            queue: Arc::new(InputQueue::new(1)),
            terminal,
            local: NodeId::new([0; 32]),
            dropped_motion: Arc::new(AtomicU64::new(0)),
            lease_diagnostics: Arc::new(LeaseTransportCounters::default()),
            metrics: Arc::new(Mutex::new(InputMetrics::default())),
            ack_tracker: Arc::new(Mutex::new(AckTracker::default())),
            rtt: Arc::new(Mutex::new(RttMeter::default())),
            shutdown: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Requests bounded drain and Shutdown without waiting for queue capacity.
    pub fn close(&self) {
        self.queue.begin_close();
        self.shutdown.notify_one();
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
    /// An exact unknown-peer TLS alert, not a generic handshake failure.
    PeerRejected { peer: NodeId },
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

/// Spawns independent reader, coordinator and writer tasks.
pub fn spawn(config: LinkConfig) -> Result<Link, TransportError> {
    let queue = Arc::new(InputQueue::new(config.queue_capacity));
    let (terminal, _) = tokio::sync::watch::channel(None);
    let diagnostics = Arc::new(LeaseTransportCounters::default());
    let metrics = Arc::new(Mutex::new(InputMetrics::default()));
    let tracker = Arc::new(Mutex::new(AckTracker::default()));
    let rtt = Arc::new(Mutex::new(RttMeter::default()));
    let shutdown = Arc::new(tokio::sync::Notify::new());
    let handle = LinkHandle {
        peer: config.peer,
        queue,
        terminal,
        local: config.local,
        dropped_motion: Arc::new(AtomicU64::new(0)),
        lease_diagnostics: diagnostics,
        metrics,
        ack_tracker: tracker,
        rtt,
        shutdown,
    };
    let task = tokio::spawn(run(config, handle.clone()));
    Ok(Link { handle, task })
}

/// Dropping a Tokio JoinHandle detaches it; aborting the coordinator must close both halves.
struct LinkTaskLifetime {
    reader: tokio::task::AbortHandle,
    writer: tokio::task::AbortHandle,
    queue: Arc<InputQueue>,
    terminal: tokio::sync::watch::Sender<Option<LinkDownReason>>,
}

impl Drop for LinkTaskLifetime {
    fn drop(&mut self) {
        self.reader.abort();
        self.writer.abort();
        self.queue.abort();
        set_terminal_once(&self.terminal, || {
            LinkDownReason::Io("coordinator stopped".into())
        });
    }
}

async fn run(config: LinkConfig, handle: LinkHandle) {
    let LinkHandle {
        queue,
        terminal,
        lease_diagnostics: diagnostics,
        metrics,
        ack_tracker: tracker,
        rtt,
        shutdown,
        ..
    } = handle;
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
        [0; 16]
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
    let remote = stream.peer_addr().ok();
    let (reader, writer) = tokio::io::split(stream);
    let (frames_tx, mut frames_rx) = mpsc::channel(8);
    let reader_task = tokio::spawn(read_loop(reader, frames_tx, Arc::clone(&diagnostics)));
    let writer_state = WriterState {
        tx_seq: 1,
        connection_id,
        batch_display: String::new(),
        lease_diagnostics: Arc::clone(&diagnostics),
        rtt: Arc::clone(&rtt),
    };
    let mut writer_task = tokio::spawn(writer_loop(
        writer,
        Arc::clone(&queue),
        terminal.clone(),
        writer_state,
        Arc::clone(&metrics),
        Arc::clone(&tracker),
        local_hello.clone(),
    ));
    let _task_lifetime = LinkTaskLifetime {
        reader: reader_task.abort_handle(),
        writer: writer_task.abort_handle(),
        queue: Arc::clone(&queue),
        terminal: terminal.clone(),
    };
    let mut state = LinkState::new(
        peer,
        connection_id,
        key,
        diagnostics,
        rtt,
        Arc::clone(&metrics),
        Arc::clone(&tracker),
    );
    state.remote = remote;
    let mut failure = terminal.subscribe();
    let initial = if direction.is_outgoing() {
        state
            .write_hello(&queue, &mut handshake, local_hello.clone())
            .err()
    } else {
        None
    };
    let reason = if let Some(reason) = initial {
        reason
    } else {
        loop {
            if let Some(reason) = failure.borrow().clone() {
                break reason;
            }
            let now_ms = state.started.elapsed().as_millis() as u64;
            let wake = policy::next_liveness_wakeup(
                now_ms,
                state.last_pong_ms,
                state.last_ping_ms,
                liveness,
            );
            let sleep = Duration::from_millis(wake.saturating_sub(now_ms).max(1));
            tokio::select! {
                biased;
                _ = failure.changed() => break failure.borrow().clone().unwrap_or(LinkDownReason::LocalClosed),
                () = shutdown.notified() => break LinkDownReason::LocalClosed,
                frame = frames_rx.recv() => {
                    match frame {
                        None => break LinkDownReason::Io("reader stopped".into()),
                        Some(Err(error)) => {
                            if matches!(&error, TransportError::Io(error) if crate::unpair::unknown_peer_alert(error)) {
                                let _ = reports.try_send(LinkReport::PeerRejected { peer });
                            }
                            break reason_from(error);
                        }
                        Some(Ok((envelope, decoded_at))) => {
                            metrics.lock().unwrap_or_else(|e| e.into_inner())
                                .record(Stage::InboundDispatch, duration_us(tokio::time::Instant::now().duration_since(decoded_at)));
                            let outcome = tokio::select! {
                                biased;
                                _ = failure.changed() => Err(failure.borrow().clone().unwrap_or(LinkDownReason::LocalClosed)),
                                () = shutdown.notified() => Err(LinkDownReason::LocalClosed),
                                result = state.on_frame(&queue, &mut handshake, &envelope, &local_hello, &reports) => result,
                            };
                            if let Err(reason) = outcome { break reason; }
                        }
                    }
                }
                () = tokio::time::sleep(sleep) => {
                    let now_ms = state.started.elapsed().as_millis() as u64;
                    match policy::liveness_action(now_ms, state.last_pong_ms, state.last_ping_ms, liveness) {
                        LivenessAction::Ping if state.announced => {
                            state.ping_nonce = state.ping_nonce.wrapping_add(1);
                            state.last_ping_ms = Some(now_ms);
                            if let Err(error) = queue.urgent(Intent::Payload(wire::envelope::Payload::Ping(
                                wire::Ping { nonce: state.ping_nonce },
                            ))) { break reason_from(error); }
                        }
                        LivenessAction::Offline => break LinkDownReason::Timeout,
                        _ => {}
                    }
                    let expired = {
                        let mut tracker = tracker.lock().unwrap_or_else(|e| e.into_inner());
                        let before = tracker.pending_expired;
                        tracker.expire(tokio::time::Instant::now());
                        tracker.pending_expired.saturating_sub(before)
                    };
                    metrics.lock().unwrap_or_else(|e| e.into_inner())
                        .increment(Counter::AckPendingExpired, expired);
                    publish_metrics(&metrics, tokio::time::Instant::now(), false);
                }
            }
        }
    };
    let deadline = queue.begin_close();
    set_terminal_once(&terminal, || reason);
    let reason = terminal
        .borrow()
        .clone()
        .expect("terminal reason was just set");
    if reason != LinkDownReason::LocalClosed {
        queue.abort();
    }
    let cleanup = async {
        if reason == LinkDownReason::LocalClosed {
            if tokio::time::timeout_at(deadline, &mut writer_task)
                .await
                .is_err()
            {
                metrics
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .increment(Counter::Failed, 1);
                set_terminal_once(&terminal, || {
                    LinkDownReason::Io("graceful close budget exhausted".into())
                });
            }
            let _ = tokio::time::timeout_at(deadline, async {
                while frames_rx.recv().await.is_some() {}
            })
            .await;
        }
        writer_task.abort();
        reader_task.abort();
    };
    tokio::join!(report_down(&reports, key, peer, reason.clone()), cleanup);
    queue.abort();
    tracker.lock().unwrap_or_else(|e| e.into_inner()).clear();
    publish_metrics(&metrics, tokio::time::Instant::now(), true);
}

fn set_terminal_once(
    terminal: &tokio::sync::watch::Sender<Option<LinkDownReason>>,
    reason: impl FnOnce() -> LinkDownReason,
) {
    terminal.send_if_modified(|current| {
        if current.is_some() {
            return false;
        }
        *current = Some(reason());
        true
    });
}

fn publish_metrics(metrics: &Mutex<InputMetrics>, now: tokio::time::Instant, final_snapshot: bool) {
    let snapshot = {
        metrics
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take_publication(now, final_snapshot)
    };
    if let Some(snapshot) = snapshot {
        snapshot.publish(final_snapshot);
    }
}

fn duration_us(duration: Duration) -> u64 {
    duration.as_micros().min(u128::from(u64::MAX)) as u64
}

/// The coordinator's mutable state.
struct LinkState {
    peer: NodeId,
    /// The key the manager gave this link (see [`LinkConfig::key`]).
    key: [u8; 16],
    connection_id: [u8; 16],
    announced: bool,
    unpair_notice: bool,
    started: tokio::time::Instant,
    last_pong_ms: u64,
    last_ping_ms: Option<u64>,
    /// Counter for `Ping` nonces (the wire field is a `uint64`).
    ping_nonce: u64,
    /// Per-direction sequence validation: «`rx_seq` строго +1. Нарушение приводит к
    /// закрытию CONNECTION».
    rx_seq: SeqValidator,
    /// The peer's address on this connection, when the socket reports one.
    remote: Option<SocketAddr>,
    last_batch_log: Option<tokio::time::Instant>,
    last_ack_log: Option<tokio::time::Instant>,
    lease_diagnostics: Arc<LeaseTransportCounters>,
    /// This connection's `Ping`/`Pong` measurement, shared with the handle.
    rtt: Arc<Mutex<RttMeter>>,
    metrics: Arc<Mutex<InputMetrics>>,
    ack_tracker: Arc<Mutex<AckTracker>>,
}

impl LinkState {
    fn new(
        peer: NodeId,
        connection_id: [u8; 16],
        key: [u8; 16],
        lease_diagnostics: Arc<LeaseTransportCounters>,
        rtt: Arc<Mutex<RttMeter>>,
        metrics: Arc<Mutex<InputMetrics>>,
        ack_tracker: Arc<Mutex<AckTracker>>,
    ) -> Self {
        Self {
            peer,
            key,
            connection_id,
            announced: false,
            unpair_notice: false,
            started: lease_diagnostics.trace.started,
            last_pong_ms: 0,
            last_ping_ms: None,
            ping_nonce: 0,
            rx_seq: SeqValidator::new(),
            remote: None,
            last_batch_log: None,
            last_ack_log: None,
            lease_diagnostics,
            rtt,
            metrics,
            ack_tracker,
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

    fn write_hello(
        &mut self,
        queue: &InputQueue,
        handshake: &mut HelloHandshake,
        hello: wire::Hello,
    ) -> Result<(), LinkDownReason> {
        let envelope = handshake
            .local_hello(hello)
            .map_err(|error| LinkDownReason::Protocol(format!("{error:?}")))?;
        queue.hello(envelope);
        Ok(())
    }

    async fn on_frame(
        &mut self,
        queue: &InputQueue,
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

        // The Hello barrier applies to liveness frames as well.
        if !self.announced && !matches!(envelope.payload, Some(wire::envelope::Payload::Hello(_))) {
            return Err(LinkDownReason::Protocol(
                "a frame arrived before Hello".into(),
            ));
        }
        if self.announced {
            validate::envelope(envelope).map_err(|e| LinkDownReason::Protocol(format!("{e:?}")))?;
            if envelope.connection_id.as_slice() != self.connection_id
                || envelope.major != limits::PROTOCOL_MAJOR
            {
                return Err(LinkDownReason::Protocol(
                    "frame belongs to another connection".into(),
                ));
            }
        }
        match &envelope.payload {
            Some(wire::envelope::Payload::Ping(ping)) => {
                return queue
                    .urgent(Intent::Payload(wire::envelope::Payload::Pong(wire::Pong {
                        nonce: ping.nonce,
                    })))
                    .map_err(reason_from);
            }
            Some(wire::envelope::Payload::Pong(pong)) => {
                self.last_pong_ms = self.started.elapsed().as_millis() as u64;
                // An unknown, duplicate or late nonce is ignored by the meter itself: only a
                // `Pong` that pairs with a ping still inside its bounds becomes a sample.
                let _ = self
                    .rtt
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .received(pong.nonce, Instant::now());
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
            self.unpair_notice = hello
                .capabilities
                .contains(&(wire::Capability::UnpairNotice as i32))
                && local_hello
                    .capabilities
                    .contains(&(wire::Capability::UnpairNotice as i32));
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
                self.write_hello(queue, handshake, local_hello.clone())?;
            }
            queue.ready(self.unpair_notice);
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
            Some(wire::envelope::Payload::Unpaired(notice)) => {
                if !self.unpair_notice
                    || !crate::unpair::authenticated_notice(
                        envelope,
                        self.peer,
                        self.connection_id,
                        notice,
                    )
                {
                    return Ok(());
                }
            }
            Some(wire::envelope::Payload::CapabilitiesChanged(_))
            | Some(wire::envelope::Payload::UpdateRequest(_))
            | Some(wire::envelope::Payload::UpdateResult(_))
            | Some(wire::envelope::Payload::ClipboardApplied(_))
            | Some(wire::envelope::Payload::ClipboardRoute(_))
                if envelope.connection_id.as_slice() != self.connection_id
                    || envelope.major != protocol::limits::PROTOCOL_MAJOR
                    || !envelope.lease_id.is_empty() =>
            {
                return Err(LinkDownReason::Protocol(
                    "control update does not belong to this input connection".into(),
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
                lease_id,
                input_batch_seq,
                applied_count,
                status,
                ..
            }) => {
                let (timing, expired) = {
                    let mut tracker = self.ack_tracker.lock().unwrap_or_else(|e| e.into_inner());
                    let before = tracker.pending_expired;
                    let timing = tracker.acknowledge(
                        0,
                        *lease_id,
                        *input_batch_seq,
                        tokio::time::Instant::now(),
                    );
                    (timing, tracker.pending_expired.saturating_sub(before))
                };
                {
                    let mut metrics = self.metrics.lock().unwrap_or_else(|e| e.into_inner());
                    metrics.increment(Counter::AckPendingExpired, expired);
                    if let Some((rtt, capture)) = timing {
                        metrics.increment(Counter::Acked, 1);
                        metrics.record(Stage::InjectAckRtt, rtt);
                        if let Some(age) = capture {
                            metrics.record(Stage::CaptureToAck, age);
                        } else {
                            metrics.unknown(Stage::CaptureToAck);
                        }
                    } else {
                        metrics.increment(Counter::AckUnmatched, 1);
                    }
                }
                self.report_inject_ack(
                    "rx",
                    envelope.seq,
                    *input_batch_seq,
                    *applied_count,
                    status,
                );
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

/// Only this state, moved into the writer task, can allocate outbound sequence numbers.
struct WriterState {
    tx_seq: u64,
    connection_id: [u8; 16],
    batch_display: String,
    lease_diagnostics: Arc<LeaseTransportCounters>,
    rtt: Arc<Mutex<RttMeter>>,
}

impl WriterState {
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
}

async fn read_loop<R: AsyncRead + Unpin>(
    mut reader: ReadHalf<R>,
    frames: mpsc::Sender<Result<(wire::Envelope, tokio::time::Instant), TransportError>>,
    lease_diagnostics: Arc<LeaseTransportCounters>,
) {
    loop {
        let frame = read_frame(&mut reader).await;
        let decoded_at = tokio::time::Instant::now();
        if let Ok(envelope) = &frame {
            lease_diagnostics.record_trace(envelope, true, false);
        }
        let stop = frame.is_err();
        let frame = frame.map(|envelope| (envelope, decoded_at));
        let queued = frames.send(frame).await;
        if queued.is_err() || stop {
            return;
        }
    }
}

async fn writer_loop<W: AsyncWrite + Unpin>(
    mut writer: W,
    queue: Arc<InputQueue>,
    terminal: tokio::sync::watch::Sender<Option<LinkDownReason>>,
    mut state: WriterState,
    metrics: Arc<Mutex<InputMetrics>>,
    tracker: Arc<Mutex<AckTracker>>,
    local_hello: wire::Hello,
) {
    let mut closing = terminal.subscribe();
    let mut local_close = false;
    let result = async {
        loop {
            // Register before inspecting state: producers cannot lose the final wakeup.
            let notified = queue.wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let reason = closing.borrow().clone();
            if let Some(reason) = reason {
                if reason != LinkDownReason::LocalClosed {
                    return Err(TransportError::LinkClosed);
                }
                local_close = true;
            }
            if let Some(intent) = queue.pop() {
                if let Intent::Unpair(notice, reply) = intent {
                    if !local_hello
                        .capabilities
                        .contains(&(wire::Capability::UnpairNotice as i32))
                        || notice.requester_node_id != local_hello.node_id
                    {
                        let _ = reply.send(false);
                        continue;
                    }
                    let envelope = state.envelope(wire::envelope::Payload::Unpaired(notice));
                    let sent = write_measured(&mut writer, &envelope, &metrics).await;
                    let _ = reply.send(sent.is_ok());
                    sent?;
                    set_terminal_once(&terminal, || LinkDownReason::LocalClosed);
                    local_close = true;
                    break;
                }
                let shutdown = matches!(
                    &intent,
                    Intent::Outbound(Outbound::Guaranteed(fr::Message::Shutdown { .. }))
                );
                write_intent(&mut writer, intent, &mut state, &queue, &metrics, &tracker).await?;
                if shutdown {
                    set_terminal_once(&terminal, || LinkDownReason::LocalClosed);
                    local_close = true;
                    break;
                }
                continue;
            }
            if local_close {
                if state.tx_seq > 1 {
                    let message = fr::Message::Shutdown {
                        reason: fr::LeaveReason::Shutdown,
                    };
                    let intent = Intent::Outbound(Outbound::Guaranteed(message));
                    write_intent(&mut writer, intent, &mut state, &queue, &metrics, &tracker)
                        .await?;
                }
                break;
            }
            tokio::select! {
                () = &mut notified => {}
                _ = closing.changed() => {}
            }
        }
        writer.shutdown().await?;
        Ok::<(), TransportError>(())
    }
    .await;
    if let Err(error) = result {
        if !local_close {
            set_terminal_once(&terminal, || reason_from(error));
        }
    }
}

fn focus_envelope(
    message: &fr::Message,
    state: &mut WriterState,
) -> Result<wire::Envelope, TransportError> {
    let lease_id = convert::lease_id_of_message(message);
    let display = convert::batch_display_after(message, &state.batch_display);
    let envelope = convert::to_envelope(
        message,
        state.tx_seq,
        &state.connection_id,
        lease_id.as_ref(),
        &display,
    )
    .map_err(|error| TransportError::Protocol(error.to_string()))?;
    state.batch_display = display;
    state.tx_seq += 1;
    Ok(envelope)
}

async fn write_intent<W: AsyncWrite + Unpin>(
    writer: &mut W,
    intent: Intent,
    state: &mut WriterState,
    queue: &InputQueue,
    metrics: &Mutex<InputMetrics>,
    tracker: &Mutex<AckTracker>,
) -> Result<(), TransportError> {
    let mut input = None;
    let envelope = match intent {
        Intent::Hello(mut envelope) => {
            state.connection_id =
                envelope.connection_id.as_slice().try_into().map_err(|_| {
                    TransportError::Protocol("invalid local Hello connection".into())
                })?;
            envelope.seq = state.tx_seq;
            state.tx_seq += 1;
            envelope
        }
        Intent::Payload(payload) => state.envelope(payload),
        Intent::Input {
            generation,
            lease_id,
            samples,
        } => {
            if !queue.valid_generation(generation) {
                return Ok(());
            }
            let envelope = convert::timed_input_envelope(
                state.tx_seq,
                &state.connection_id,
                &lease_id,
                &state.batch_display,
                samples
                    .iter()
                    .map(|sample| (&sample.event, sample.capture_host_us)),
            )
            .map_err(|error| TransportError::Protocol(error.to_string()))?;
            state.tx_seq += 1;
            input = Some((generation, lease_id, samples));
            envelope
        }
        Intent::Outbound(item) => match item {
            Outbound::Guaranteed(message) => {
                if let fr::Message::Leave { lease_id, .. } | fr::Message::LeaveAck { lease_id } =
                    &message
                {
                    tracker
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clear_lease(0, *lease_id);
                }
                focus_envelope(&message, state)?
            }
            Outbound::Layout(message) => {
                let envelope =
                    convert::layout_envelope(&message, state.tx_seq, &state.connection_id);
                state.tx_seq += 1;
                envelope
            }
            Outbound::Clipboard(message) => {
                let envelope = message
                    .to_envelope(state.tx_seq, &state.connection_id)
                    .map_err(|e| TransportError::Protocol(e.to_string()))?;
                state.tx_seq += 1;
                envelope
            }
            Outbound::CapabilitiesChanged(capabilities) => {
                state.envelope(wire::envelope::Payload::CapabilitiesChanged(
                    wire::CapabilitiesChanged { capabilities },
                ))
            }
            Outbound::UpdateRequest(value) => {
                state.envelope(wire::envelope::Payload::UpdateRequest(value))
            }
            Outbound::UpdateResult(value) => {
                state.envelope(wire::envelope::Payload::UpdateResult(value))
            }
            Outbound::ClipboardApplied(value) => {
                state.envelope(wire::envelope::Payload::ClipboardApplied(value))
            }
            Outbound::ClipboardRoute(value) => {
                state.envelope(wire::envelope::Payload::ClipboardRoute(value))
            }
            Outbound::Motion { .. } => unreachable!("motion is admitted through the latest slot"),
        },
        Intent::Unpair(..) => unreachable!("unpair completion belongs to writer loop"),
    };
    let started = tokio::time::Instant::now();
    if let Some(wire::envelope::Payload::Ping(ping)) = &envelope.payload {
        state
            .rtt
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .sent(ping.nonce, Instant::now());
    }
    if let Some((_, lease_id, samples)) = &input {
        let mut oldest = None;
        {
            let mut metrics = metrics.lock().unwrap_or_else(|e| e.into_inner());
            for sample in samples {
                let elapsed = duration_us(started.duration_since(sample.enqueued_at));
                metrics.record(Stage::EnqueueToWrite, elapsed);
                if let Some(age) = sample
                    .capture_to_enqueue_us
                    .and_then(|age| age.checked_add(elapsed))
                {
                    metrics.record(Stage::CaptureToWrite, age);
                    oldest = Some(oldest.map_or(age, |previous: u64| previous.max(age)));
                } else {
                    metrics.unknown(Stage::CaptureToWrite);
                }
            }
            metrics.increment(Counter::Sent, 1);
        }
        let (evicted, expired) = {
            let mut tracker = tracker.lock().unwrap_or_else(|e| e.into_inner());
            let before = (tracker.evicted, tracker.pending_expired);
            tracker.insert(0, *lease_id, envelope.seq, started, oldest);
            (
                tracker.evicted.saturating_sub(before.0),
                tracker.pending_expired.saturating_sub(before.1),
            )
        };
        let mut metrics = metrics.lock().unwrap_or_else(|e| e.into_inner());
        metrics.increment(Counter::AckTrackingEvicted, evicted);
        metrics.increment(Counter::AckPendingExpired, expired);
    }
    write_measured(writer, &envelope, metrics).await?;
    state.lease_diagnostics.record_trace(&envelope, false, true);
    state
        .lease_diagnostics
        .record(envelope.payload.as_ref(), true);
    Ok(())
}

async fn write_measured<W: AsyncWrite + Unpin>(
    writer: &mut W,
    envelope: &wire::Envelope,
    metrics: &Mutex<InputMetrics>,
) -> Result<(), TransportError> {
    let started = tokio::time::Instant::now();
    let result = write_frame(writer, envelope).await;
    let mut metrics = metrics.lock().unwrap_or_else(|e| e.into_inner());
    let elapsed = duration_us(started.elapsed());
    let stage = match &result {
        Ok(()) => Stage::TlsWrite,
        Err(TransportError::Io(error)) if error.kind() == std::io::ErrorKind::TimedOut => {
            Stage::TlsWriteTimeout
        }
        Err(_) => Stage::TlsWriteFailed,
    };
    metrics.record(stage, elapsed);
    if let Err(error) = &result {
        metrics.increment(Counter::Failed, 1);
        if matches!(error, TransportError::Io(error) if error.kind() == std::io::ErrorKind::TimedOut)
        {
            let counter = if error.to_string().contains("frame deadline") {
                Counter::WriterFrameTimeout
            } else {
                Counter::WriterIdleTimeout
            };
            metrics.increment(counter, 1);
        }
    }
    result
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

#[cfg(test)]
#[path = "link/input_pipeline_tests.rs"]
mod input_pipeline_tests;
