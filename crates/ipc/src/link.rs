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

use tokio::io::{AsyncRead, AsyncWrite};

use protocol::ipc::{agent_frame, gui_frame, AgentFrame, GuiFrame};
use protocol::SeqValidator;

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
