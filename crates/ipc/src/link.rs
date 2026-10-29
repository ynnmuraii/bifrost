//! `seq` and version handling for the two IPC channels.
//!
//! `docs/03-architecture.md`: «Свой `seq` в каждом направлении; при разрыве
//! последовательности соединение закрывается, и для agent это означает release.
//! Версии кадра — `major`/`minor` в `AgentFrame`; несовпадение major закрывает
//! канал с понятной ошибкой.»
//!
//! [`AgentLink`] and [`GuiLink`] own the counters: every sent frame gets the next
//! `seq` stamped in, every received frame is checked against
//! [`protocol::SeqValidator`], so a gap, a repeat or a regression fails the link
//! instead of silently reordering work.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncWrite};

use protocol::ipc::{agent_frame, gui_frame, AgentFrame, GuiFrame};
use protocol::SeqValidator;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::error::IpcError;
use crate::framed::Framed;

/// The core ↔ agent link: `AgentFrame` in both directions, versioned by
/// `major`/`minor`.
#[derive(Debug)]
pub struct AgentLink<S> {
    framed: Framed<S>,
    major: u32,
    minor: u32,
    inbound: SeqValidator,
    outbound_next: u64,
}

impl<S> AgentLink<S> {
    /// A link that stamps `major`/`minor` into outgoing frames and rejects
    /// incoming frames with a different `major`.
    ///
    /// Both directions start at `seq = 1`.
    pub fn new(io: S, major: u32, minor: u32) -> Self {
        AgentLink {
            framed: Framed::ipc(io),
            major,
            minor,
            inbound: SeqValidator::new(),
            outbound_next: 1,
        }
    }

    /// The frame version this side speaks.
    pub const fn version(&self) -> (u32, u32) {
        (self.major, self.minor)
    }

    /// `seq` the next outgoing frame will carry.
    pub const fn outbound_next(&self) -> u64 {
        self.outbound_next
    }

    /// `seq` the next incoming frame must carry.
    pub const fn inbound_expected(&self) -> u64 {
        self.inbound.expected()
    }

    /// The underlying framed transport, for handshake-level needs.
    pub fn framed(&mut self) -> &mut Framed<S> {
        &mut self.framed
    }

    /// Send the first frame of the channel, `AgentAttach`.
    pub async fn attach(&mut self, attach: protocol::ipc::AgentAttach) -> Result<(), IpcError>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        self.send(agent_frame::Msg::Attach(attach)).await
    }

    /// Send one message with the next `seq`.
    pub async fn send(&mut self, message: agent_frame::Msg) -> Result<(), IpcError>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let frame = AgentFrame {
            major: self.major,
            minor: self.minor,
            seq: self.outbound_next,
            msg: Some(message),
        };
        self.framed.send(&frame).await?;
        self.outbound_next = self.outbound_next.checked_add(1).ok_or_else(|| {
            IpcError::protocol("outgoing seq would exceed u64; open a new channel")
        })?;
        Ok(())
    }

    /// Receive one frame, checking `major` and `seq`.
    pub async fn recv(&mut self) -> Result<AgentFrame, IpcError>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let frame: AgentFrame = self.framed.recv().await?;
        if frame.major != self.major {
            return Err(IpcError::protocol(format!(
                "AgentFrame major {} does not match the negotiated {}",
                frame.major, self.major
            )));
        }
        self.inbound.accept(frame.seq)?;
        Ok(frame)
    }

    /// Move writes to one owned task; handshake counters and buffered reads survive.
    pub fn into_split(self) -> AgentSplit<S>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (reader, writer) = self.framed.into_split();
        let (control_tx, control_rx) = mpsc::channel(32);
        let (bulk_tx, bulk_rx) = mpsc::channel(2);
        let (failure_tx, failure_rx) = watch::channel(None);
        let generation = Arc::new(AtomicU64::new(0));
        let task = tokio::spawn(agent_writer(
            Framed::ipc(writer),
            self.major,
            self.minor,
            self.outbound_next,
            control_rx,
            bulk_rx,
            Arc::clone(&generation),
            failure_tx,
        ));
        AgentSplit {
            reader,
            major: self.major,
            inbound: self.inbound,
            sender: AgentSender {
                control: control_tx,
                bulk: bulk_tx,
                generation,
            },
            failure: failure_rx,
            writer: task,
        }
    }
}

/// Bounded queues feeding the sole sequence-owning writer.
#[derive(Clone)]
pub struct AgentSender {
    control: mpsc::Sender<agent_frame::Msg>,
    bulk: mpsc::Sender<(u64, agent_frame::Msg)>,
    generation: Arc<AtomicU64>,
}

impl AgentSender {
    /// Queue control/input without waiting behind clipboard payload frames.
    pub async fn send(&self, message: agent_frame::Msg) -> Result<(), IpcError> {
        self.control
            .send(message)
            .await
            .map_err(|_| IpcError::Closed)
    }

    /// Invalidate queued chunks before starting a replacement producer or release.
    pub fn replace_bulk(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// At most two clipboard chunks may wait for the writer.
    pub async fn send_bulk(
        &self,
        generation: u64,
        message: agent_frame::Msg,
    ) -> Result<(), IpcError> {
        if self.generation.load(Ordering::SeqCst) != generation {
            return Err(IpcError::Closed);
        }
        self.bulk
            .send((generation, message))
            .await
            .map_err(|_| IpcError::Closed)
    }
}

/// Validated reader plus the lifetime of its independent owned writer.
pub struct AgentSplit<S> {
    reader: Framed<tokio::io::ReadHalf<S>>,
    major: u32,
    inbound: SeqValidator,
    sender: AgentSender,
    failure: watch::Receiver<Option<String>>,
    writer: JoinHandle<()>,
}

impl<S: AsyncRead + AsyncWrite + Unpin> AgentSplit<S> {
    pub fn sender(&self) -> AgentSender {
        self.sender.clone()
    }

    pub async fn send(&self, message: agent_frame::Msg) -> Result<(), IpcError> {
        if self.failure.borrow().is_some() {
            return Err(IpcError::Closed);
        }
        self.sender.send(message).await
    }

    /// Cancellation preserves read buffering; write failure ends this same link.
    pub async fn recv(&mut self) -> Result<AgentFrame, IpcError> {
        if let Some(error) = self.failure.borrow().as_ref() {
            return Err(IpcError::protocol(error.clone()));
        }
        let frame: AgentFrame = tokio::select! {
            biased;
            _ = self.failure.changed() => return Err(IpcError::Closed),
            frame = self.reader.recv() => match frame {
                Ok(frame) => frame,
                Err(error) => {
                    self.writer.abort();
                    return Err(error);
                }
            },
        };
        if frame.major != self.major {
            self.writer.abort();
            return Err(IpcError::protocol(format!(
                "AgentFrame major {} does not match the negotiated {}",
                frame.major, self.major
            )));
        }
        if let Err(error) = self.inbound.accept(frame.seq) {
            self.writer.abort();
            return Err(error.into());
        }
        Ok(frame)
    }
}

impl<S> Drop for AgentSplit<S> {
    fn drop(&mut self) {
        // Cancellation is only allowed when the entire link is being discarded.
        self.writer.abort();
    }
}

#[allow(clippy::too_many_arguments)]
async fn agent_writer<W: AsyncWrite + Unpin>(
    mut writer: Framed<W>,
    major: u32,
    minor: u32,
    mut next: u64,
    mut control: mpsc::Receiver<agent_frame::Msg>,
    mut bulk: mpsc::Receiver<(u64, agent_frame::Msg)>,
    generation: Arc<AtomicU64>,
    failure: watch::Sender<Option<String>>,
) {
    loop {
        let message = tokio::select! {
            biased;
            message = control.recv() => match message {
                Some(message) => message,
                None => break,
            },
            message = bulk.recv() => match message {
                Some((current, message)) => {
                    if current != generation.load(Ordering::SeqCst) {
                        continue;
                    }
                    message
                }
                None => break,
            },
        };
        let frame = AgentFrame {
            major,
            minor,
            seq: next,
            msg: Some(message),
        };
        // Never race/cancel a partial write to service a higher-priority message.
        if let Err(error) = writer.send(&frame).await {
            let _ = failure.send(Some(error.to_string()));
            return;
        }
        match next.checked_add(1) {
            Some(value) => next = value,
            None => {
                let _ = failure.send(Some("outgoing seq exhausted".to_owned()));
                return;
            }
        }
    }
}

/// The core ↔ gui link: `GuiFrame` in both directions, `seq` only (the frame has
/// no version fields; the GUI is a local process of the same build).
#[derive(Debug)]
pub struct GuiLink<S> {
    framed: Framed<S>,
    inbound: SeqValidator,
    outbound_next: u64,
}

impl<S> GuiLink<S> {
    /// A link whose both directions start at `seq = 1`.
    pub fn new(io: S) -> Self {
        GuiLink {
            framed: Framed::ipc(io),
            inbound: SeqValidator::new(),
            outbound_next: 1,
        }
    }

    /// `seq` the next outgoing frame will carry.
    pub const fn outbound_next(&self) -> u64 {
        self.outbound_next
    }

    /// `seq` the next incoming frame must carry.
    pub const fn inbound_expected(&self) -> u64 {
        self.inbound.expected()
    }

    /// The underlying framed transport.
    pub fn framed(&mut self) -> &mut Framed<S> {
        &mut self.framed
    }

    /// Send one message with the next `seq`; `request_id` correlates a response
    /// with its request and is `0` for events.
    pub async fn send(&mut self, request_id: u64, message: gui_frame::Msg) -> Result<(), IpcError>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let frame = GuiFrame {
            seq: self.outbound_next,
            request_id,
            msg: Some(message),
        };
        self.framed.send(&frame).await?;
        self.outbound_next = self.outbound_next.checked_add(1).ok_or_else(|| {
            IpcError::protocol("outgoing seq would exceed u64; open a new channel")
        })?;
        Ok(())
    }

    /// Receive one frame, checking `seq`.
    pub async fn recv(&mut self) -> Result<GuiFrame, IpcError>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let frame: GuiFrame = self.framed.recv().await?;
        self.inbound.accept(frame.seq)?;
        Ok(frame)
    }
}

#[cfg(test)]
mod split_tests {
    use super::*;
    use protocol::ipc::{Ping, SetClipboardText};

    fn ping(nonce: u64) -> agent_frame::Msg {
        agent_frame::Msg::Ping(Ping { nonce })
    }

    fn chunk(version: u64) -> agent_frame::Msg {
        agent_frame::Msg::SetClipboardText(SetClipboardText {
            origin_version: version,
            chunk_index: 0,
            chunk_count: 1,
            data: vec![7; 32768],
            sha256: vec![8; 32],
            ..Default::default()
        })
    }

    #[tokio::test]
    async fn split_preserves_handshake_sequence_and_inbound_validation() {
        let (a, b) = tokio::io::duplex(1024);
        let mut original = AgentLink::new(a, 0, 3);
        let mut peer = AgentLink::new(b, 0, 3);
        original.send(ping(1)).await.unwrap();
        peer.recv().await.unwrap();
        peer.send(ping(2)).await.unwrap();
        original.recv().await.unwrap();
        let mut split = original.into_split();
        split.send(ping(3)).await.unwrap();
        assert_eq!(peer.recv().await.unwrap().seq, 2);
        peer.send(ping(4)).await.unwrap();
        assert_eq!(split.recv().await.unwrap().seq, 2);
        peer.framed()
            .send(&AgentFrame {
                major: 0,
                minor: 3,
                seq: 4,
                msg: Some(ping(5)),
            })
            .await
            .unwrap();
        assert!(split.recv().await.is_err());
    }

    #[tokio::test]
    async fn control_preempts_bulk_only_after_complete_frame() {
        let (a, b) = tokio::io::duplex(1);
        let split = AgentLink::new(a, 0, 3).into_split();
        let sender = split.sender();
        let generation = sender.replace_bulk();
        sender.send_bulk(generation, chunk(1)).await.unwrap();
        // With one byte of capacity the first frame cannot finish before the read.
        tokio::task::yield_now().await;
        sender.send_bulk(generation, chunk(2)).await.unwrap();
        sender.send(ping(99)).await.unwrap();
        let mut peer = AgentLink::new(b, 0, 3);
        let first = peer.recv().await.unwrap();
        assert_eq!(first.seq, 1);
        assert!(matches!(
            first.msg,
            Some(agent_frame::Msg::SetClipboardText(_))
        ));
        let second = peer.recv().await.unwrap();
        assert_eq!(second.seq, 2);
        assert!(matches!(second.msg, Some(agent_frame::Msg::Ping(_))));
        assert_eq!(peer.recv().await.unwrap().seq, 3);
    }

    #[tokio::test]
    async fn bounded_bulk_replacement_discards_old_queued_chunks() {
        let (a, b) = tokio::io::duplex(1);
        let split = AgentLink::new(a, 0, 3).into_split();
        let sender = split.sender();
        let old = sender.replace_bulk();
        sender.send_bulk(old, chunk(1)).await.unwrap();
        tokio::task::yield_now().await;
        sender.send_bulk(old, chunk(2)).await.unwrap();
        sender.send_bulk(old, chunk(3)).await.unwrap();
        assert_eq!(sender.bulk.capacity(), 0);
        {
            let blocked = sender.send_bulk(old, chunk(4));
            tokio::pin!(blocked);
            tokio::select! {
                biased;
                _ = &mut blocked => panic!("bulk queue exceeded its bound"),
                _ = tokio::task::yield_now() => {}
            }
        }
        let new = sender.replace_bulk();
        let producer = tokio::spawn(async move {
            sender.send_bulk(new, chunk(5)).await.unwrap();
        });
        let mut peer = AgentLink::new(b, 0, 3);
        assert_eq!(peer.recv().await.unwrap().seq, 1);
        let replacement = peer.recv().await.unwrap();
        assert_eq!(replacement.seq, 2);
        assert!(matches!(
            replacement.msg,
            Some(agent_frame::Msg::SetClipboardText(chunk)) if chunk.origin_version == 5
        ));
        producer.await.unwrap();
    }
}

#[cfg(test)]
mod split_failure_tests {
    use super::*;

    #[tokio::test]
    async fn failed_link_never_reuses_writer_for_another_frame() {
        let (core, peer) = tokio::io::duplex(1);
        let mut link = AgentLink::new(core, 0, 3).into_split();
        drop(peer);
        link.send(agent_frame::Msg::Ping(protocol::ipc::Ping { nonce: 1 }))
            .await
            .unwrap();
        assert!(link.recv().await.is_err());
        tokio::task::yield_now().await;
        assert!(link
            .send(agent_frame::Msg::Ping(protocol::ipc::Ping { nonce: 2 }))
            .await
            .is_err());
    }
}
