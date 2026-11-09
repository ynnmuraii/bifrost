//! The GUI endpoint: `GuiFrame` over the local channel of `docs/03-architecture.md`
//! («GUI ↔ core») on `\\.\pipe\bifrost-<SID>` / `gui.sock`.
//!
//! This module owns the transport half and nothing else: which endpoint is bound, who may
//! connect, one `seq`-stamped link per GUI, and the fan-out of pushed events. What a
//! request *means* is [`crate::session::gui`], which runs inside the session loop
//! next to the FSM — the GUI has no path to the FSM, the network or the input devices of
//! its own (`docs/03-architecture.md`: «GUI не участвует в горячем пути»).
//!
//! # Request, answer, event
//!
//! A connection is one task with one [`GuiLink`], and every frame it writes goes through
//! that link, so the `seq` the contract asks for is stamped in exactly one place. A request
//! is forwarded to the session with a clone of the connection's outbound queue
//! ([`Request::out`]); the session answers on it (possibly later — `UpdateTopology` waits
//! for the coordinator's verdict), and events from [`Server::publish`] arrive on the same
//! queue through the broadcast channel. Closing the GUI window drops the task and the queue
//! with it; the session's own sender keeps the request channel alive, so the loop never
//! spins on a closed one.
//!
//! # Why `recv` is polled in a `select!`
//!
//! [`ipc::Framed`] keeps its read buffer across calls and reads with an
//! `AsyncReadExt::read`, so a cancelled branch has either consumed the bytes into that
//! buffer or read nothing at all: the loop below can select over the inbound link, the
//! outbound queue and the events without losing a frame.

use ipc::peer::SameUser;
use ipc::transport::{self, ClientStream, Endpoint, GuiListener};
use ipc::{GuiLink, IpcError};
use protocol::ipc::{gui_frame, GuiFrame};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, watch};

/// One frame on its way to a GUI connection, with the request it answers (`0` = event).
pub type Outgoing = (u64, gui_frame::Msg);
/// Connection-owned reply delivery. Saturation closes this epoch instead of losing an ACK.
#[derive(Debug, Clone)]
pub struct Replies {
    tx: mpsc::Sender<Outgoing>,
    disconnect: watch::Sender<bool>,
}

impl Replies {
    pub(crate) fn channel(capacity: usize) -> (Self, mpsc::Receiver<Outgoing>) {
        let (tx, rx) = mpsc::channel(capacity);
        let (disconnect, _) = watch::channel(false);
        (Self { tx, disconnect }, rx)
    }

    pub(crate) fn is_closed(&self) -> bool {
        *self.disconnect.borrow() || self.tx.is_closed()
    }

    pub(crate) fn try_send(&self, message: Outgoing) -> Result<(), mpsc::error::TrySendError<()>> {
        if self.is_closed() {
            return Err(mpsc::error::TrySendError::Closed(()));
        }
        self.tx.try_send(message).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => {
                self.disconnect.send_replace(true);
                mpsc::error::TrySendError::Full(())
            }
            mpsc::error::TrySendError::Closed(_) => mpsc::error::TrySendError::Closed(()),
        })
    }

    pub(crate) async fn send(&self, message: Outgoing) -> Result<(), mpsc::error::SendError<()>> {
        self.try_send(message)
            .map_err(|_| mpsc::error::SendError(()))
    }

    /// Waits for queue capacity only from detached producers, never the session loop.
    pub(crate) async fn send_wait(
        &self,
        message: Outgoing,
    ) -> Result<(), mpsc::error::SendError<()>> {
        let mut disconnected = self.disconnect.subscribe();
        if self.is_closed() {
            return Err(mpsc::error::SendError(()));
        }
        tokio::select! {
            biased;
            _ = disconnected.changed() => Err(mpsc::error::SendError(())),
            result = self.tx.send(message) => result.map_err(|_| mpsc::error::SendError(())),
        }
    }
}

/// How many frames a connection may hold before its writer catches up.
///
/// A GUI that stops reading must not grow core's memory: the queue is bounded, and a
/// connection this far behind is dropped by the `send` that fails, exactly as
/// `docs/03-architecture.md` says a broken local channel is handled.
const OUTBOUND_QUEUE: usize = 64;
const WRITE_TIMEOUT: Duration = Duration::from_millis(250);

/// One request from a GUI connection, with the way to answer it.
pub struct Request {
    /// The `request_id` of the frame, echoed on every answer.
    pub request_id: u64,
    /// Fresh server-assigned identity of the connection, shared by all its requests.
    pub connection_id: [u8; 16],
    /// The message itself.
    pub message: gui_frame::Msg,
    /// Where every answer frame goes.
    ///
    /// The session keeps this rather than a `oneshot` because an answer is not always one
    /// frame (`UpdateTopology` is answered with `OpResult`, and the applied document
    /// follows as a pushed `TopologyDoc`), and because a deferred answer outlives the turn
    /// that received the request.
    pub out: Replies,
}

/// The GUI endpoint of a running core.
pub struct Server {
    listener: GuiListener,
    events: broadcast::Sender<Outgoing>,
}

impl Server {
    /// Binds the endpoint. Fails with [`IpcError::AlreadyRunning`] when another core of
    /// this user already owns it, which is what a second `bifrost-core run` must see.
    pub fn bind(endpoint: &Endpoint) -> Result<Self, IpcError> {
        let (events, _) = broadcast::channel(64);
        Ok(Server {
            listener: transport::listen(endpoint)?,
            events,
        })
    }

    /// Accepts the next GUI client.
    ///
    /// The kernel already checked the DACL on Windows; on Unix [`SameUser`] compares
    /// `SO_PEERCRED`.
    pub async fn accept(&mut self) -> Result<ClientStream, IpcError> {
        self.listener.accept(&SameUser::current()).await
    }

    /// A handle the session publishes events through.
    pub fn publisher(&self) -> broadcast::Sender<Outgoing> {
        self.events.clone()
    }

    /// Pushes a frame to every connected GUI.
    ///
    /// `request_id` is 0: an event answers nothing. With no GUI connected the send fails
    /// and is dropped on purpose — the state the GUI needs when it starts is what
    /// `GetStatus` asks for.
    pub fn publish(events: &broadcast::Sender<Outgoing>, message: gui_frame::Msg) {
        let _ = events.send((0, message));
    }

    /// Serves a connection until it closes.
    pub fn spawn(
        &self,
        stream: ClientStream,
        requests: mpsc::Sender<Request>,
    ) -> tokio::task::JoinHandle<()> {
        let events = self.events.subscribe();
        tokio::spawn(async move {
            let connection_id = match identity::random16() {
                Ok(id) => id,
                Err(error) => {
                    tracing::warn!(%error, "cannot assign a GUI connection identity");
                    return;
                }
            };
            serve(stream, requests, events, connection_id).await;
        })
    }
}

/// The read/write loop of one GUI connection.
async fn serve<S>(
    stream: S,
    requests: mpsc::Sender<Request>,
    mut events: broadcast::Receiver<Outgoing>,
    connection_id: [u8; 16],
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (out_tx, mut out_rx) = Replies::channel(OUTBOUND_QUEUE);
    let mut disconnected = out_tx.disconnect.subscribe();
    let mut link = GuiLink::new(stream);
    loop {
        if out_tx.is_closed() {
            return;
        }
        tokio::select! {
            _ = disconnected.changed() => return,
            incoming = link.recv() => match incoming {
                Ok(GuiFrame { request_id, msg, .. }) => {
                    let Some(message) = msg else {
                        // A frame with no message cannot be answered; the contract has no
                        // "protocol error" answer for the GUI, so it is counted and skipped
                        // rather than closing a window over nothing.
                        tracing::warn!(request_id, "a GUI frame carried no message");
                        continue;
                    };
                    if requests
                        .try_send(Request { request_id, connection_id, message, out: out_tx.clone() })
                        .is_err()
                    {
                        tracing::warn!("GUI request queue unavailable; closing connection");
                        return;
                    }
                }
                Err(error) => {
                    tracing::debug!(%error, "a GUI connection ended");
                    return;
                }
            },
            Some((request_id, message)) = out_rx.recv() => {
                let sent = tokio::select! {
                    biased;
                    _ = disconnected.changed() => return,
                    sent = tokio::time::timeout(WRITE_TIMEOUT, link.send(request_id, message)) => sent,
                };
                if !matches!(&sent, Ok(Ok(()))) {
                    tracing::debug!(?sent, "GUI reply write failed or stalled; closing connection");
                    return;
                }
            }
            event = events.recv() => match event {
                Ok((request_id, message)) => {
                    let sent = tokio::select! {
                        biased;
                        _ = disconnected.changed() => return,
                        sent = tokio::time::timeout(WRITE_TIMEOUT, link.send(request_id, message)) => sent,
                    };
                    if !matches!(&sent, Ok(Ok(()))) {
                        tracing::debug!(?sent, "GUI event write failed or stalled; closing connection");
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::warn!(skipped, "GUI event stream lagged; closing connection for resynchronization");
                    return;
                }
                Err(broadcast::error::RecvError::Closed) => return,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::ipc::{self as wire, gui_frame::Msg};

    #[tokio::test]
    async fn reply_overflow_cancels_the_epoch_even_if_a_slot_is_drained_afterwards() {
        let (out, mut frames) = Replies::channel(1);
        let mut disconnected = out.disconnect.subscribe();
        out.try_send((
            1,
            Msg::OpResult(wire::OpResult {
                ok: true,
                ..Default::default()
            }),
        ))
        .unwrap();
        assert!(matches!(
            out.try_send((2, Msg::OpResult(wire::OpResult::default()))),
            Err(mpsc::error::TrySendError::Full(_))
        ));
        frames.recv().await.unwrap();
        assert!(
            out.is_closed(),
            "freeing capacity cannot resurrect a lost correlated response"
        );
        disconnected.changed().await.unwrap();
        assert!(*disconnected.borrow());
        assert!(out
            .send((3, Msg::OpResult(wire::OpResult::default())))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn lagged_terminal_event_stream_disconnects_instead_of_silently_continuing() {
        let (stream, peer) = tokio::io::duplex(64);
        let (requests, _receiver) = mpsc::channel(4);
        let (events, observer) = broadcast::channel(1);
        for _ in 0..3 {
            events
                .send((
                    0,
                    Msg::PairingWindow(wire::PairingWindow {
                        open: false,
                        ..Default::default()
                    }),
                ))
                .unwrap();
        }
        tokio::time::timeout(
            Duration::from_secs(1),
            serve(stream, requests, observer, [1; 16]),
        )
        .await
        .expect("event loss ends the connection");
        let mut peer = GuiLink::new(peer);
        assert!(peer.recv().await.is_err());
    }

    #[tokio::test]
    async fn slow_gui_write_is_bounded_and_closes_the_actual_reply_sender() {
        let (stream, peer) = tokio::io::duplex(64);
        let (requests, mut receiver) = mpsc::channel(4);
        let (_events, observer) = broadcast::channel(4);
        let serving = tokio::spawn(serve(stream, requests, observer, [3; 16]));
        let mut peer = GuiLink::new(peer);
        peer.send(17, Msg::GetStatus(wire::GetStatus {}))
            .await
            .unwrap();
        let request = receiver.recv().await.unwrap();
        assert_eq!(request.connection_id, [3; 16]);
        assert_eq!(request.request_id, 17);
        request
            .out
            .send((
                17,
                Msg::OpResult(wire::OpResult {
                    ok: false,
                    error_code: "x".repeat(1024),
                }),
            ))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), serving)
            .await
            .expect("blocked write disconnects")
            .unwrap();
        assert!(request.out.is_closed());
    }
}
