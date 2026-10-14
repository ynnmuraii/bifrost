//! The GUI endpoint: `GuiFrame` over the local channel of `docs/03-architecture.md`
//! («GUI ↔ core») on `\\.\pipe\lan-workspace-<SID>` / `gui.sock`.
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
use tokio::sync::{broadcast, mpsc};

/// One frame on its way to a GUI connection, with the request it answers (`0` = event).
pub type Outgoing = (u64, gui_frame::Msg);

/// How many frames a connection may hold before its writer catches up.
///
/// A GUI that stops reading must not grow core's memory: the queue is bounded, and a
/// connection this far behind is dropped by the `send` that fails, exactly as
/// `docs/03-architecture.md` says a broken local channel is handled.
const OUTBOUND_QUEUE: usize = 64;

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
    pub out: mpsc::Sender<Outgoing>,
}

/// The GUI endpoint of a running core.
pub struct Server {
    listener: GuiListener,
    events: broadcast::Sender<Outgoing>,
}

impl Server {
    /// Binds the endpoint. Fails with [`IpcError::AlreadyRunning`] when another core of
    /// this user already owns it, which is what a second `lws-core run` must see.
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
async fn serve(
    stream: ClientStream,
    requests: mpsc::Sender<Request>,
    mut events: broadcast::Receiver<Outgoing>,
    connection_id: [u8; 16],
) {
    let (out_tx, mut out_rx) = mpsc::channel::<Outgoing>(OUTBOUND_QUEUE);
    let mut link = GuiLink::new(stream);
    loop {
        tokio::select! {
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
                        .send(Request { request_id, connection_id, message, out: out_tx.clone() })
                        .await
                        .is_err()
                    {
                        // Core is shutting down.
                        return;
                    }
                }
                Err(error) => {
                    tracing::debug!(%error, "a GUI connection ended");
                    return;
                }
            },
            Some((request_id, message)) = out_rx.recv() => {
                if let Err(error) = link.send(request_id, message).await {
                    tracing::debug!(%error, "cannot answer the GUI");
                    return;
                }
            }
            event = events.recv() => match event {
                Ok((request_id, message)) => {
                    if let Err(error) = link.send(request_id, message).await {
                        tracing::debug!(%error, "cannot push an event to the GUI");
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    // The GUI is not reading fast enough. Its next `GetStatus`/`GetTopology`
                    // gives it the state again, so the gap is reported and not invented.
                    tracing::warn!(skipped, "a GUI connection fell behind the events");
                }
                Err(broadcast::error::RecvError::Closed) => return,
            },
        }
    }
}
