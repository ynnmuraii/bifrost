//! Framed `AgentFrame`/`GuiFrame` transport over any async byte stream.
//!
//! `docs/03-architecture.md`: «Framing: 4 байта big-endian длины + Protobuf
//! (`AgentFrame`/`GuiFrame`), максимум 64 KiB.» The framing itself lives in the
//! `protocol` crate ([`protocol::frame`]) and is reused unchanged — this module
//! only adds the async read loop, so the length is still validated *before*
//! anything is buffered and a hostile prefix cannot make the link allocate.
//!
//! The read buffer is bounded by `max_frame + 4 + READ_CHUNK` bytes: a frame is
//! extracted and removed as soon as it is complete, and an invalid length is
//! rejected from the four prefix bytes alone.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use protocol::frame::{FrameDecoder, LENGTH_PREFIX_LEN};
use protocol::Channel;

use crate::error::IpcError;
use zeroize::{Zeroize, Zeroizing};

/// How much is read at once. Bounded so a stalled peer cannot grow the buffer.
pub const READ_CHUNK: usize = 8 * 1024;

/// One framed link over `io`, for one channel's frame limit.
#[derive(Debug)]
pub struct Framed<S> {
    io: S,
    channel: Channel,
    decoder: FrameDecoder,
    buffer: Vec<u8>,
}

impl<S> Framed<S> {
    /// A link over `io` validating against `channel`'s frame limit.
    pub fn new(io: S, channel: Channel) -> Self {
        Framed {
            io,
            channel,
            decoder: FrameDecoder::new(channel),
            buffer: Vec::new(),
        }
    }

    /// A link for the local IPC channel ([`Channel::Ipc`], 65536-byte frames).
    pub fn ipc(io: S) -> Self {
        Framed::new(io, Channel::Ipc)
    }

    /// Fixed-capacity receive storage for a private channel carrying key material.
    pub fn sensitive_ipc(io: S) -> Self {
        let mut framed = Self::ipc(io);
        framed.buffer = Vec::with_capacity(framed.max_buffered_len());
        framed
    }

    pub(crate) fn clear_sensitive(&mut self) {
        self.buffer.resize(self.buffer.capacity(), 0);
        self.buffer.zeroize();
    }

    /// The channel this link validates against.
    pub fn channel(&self) -> Channel {
        self.channel
    }

    /// Bytes currently buffered but not yet a complete frame.
    pub fn buffered_len(&self) -> usize {
        self.buffer.len()
    }

    /// Upper bound of [`Framed::buffered_len`].
    pub fn max_buffered_len(&self) -> usize {
        self.channel.max_frame_len() + LENGTH_PREFIX_LEN + READ_CHUNK
    }

    /// The underlying stream. Writes through it bypass the length prefix, which
    /// is what a handshake-level or hostile-peer test needs; normal traffic must
    /// go through [`Framed::send`].
    pub fn get_mut(&mut self) -> &mut S {
        &mut self.io
    }

    /// Give the stream back.
    pub fn into_inner(self) -> S {
        self.io
    }

    /// Split ownership without discarding already buffered incoming bytes.
    pub fn into_split(self) -> (Framed<tokio::io::ReadHalf<S>>, tokio::io::WriteHalf<S>)
    where
        S: AsyncRead + AsyncWrite,
    {
        let (reader, writer) = tokio::io::split(self.io);
        (
            Framed {
                io: reader,
                channel: self.channel,
                decoder: self.decoder,
                buffer: self.buffer,
            },
            writer,
        )
    }
}

impl<S> Framed<S>
where
    S: AsyncWrite + Unpin,
{
    /// Encode and write one message.
    ///
    /// The length is checked against the channel limit before anything is
    /// written, so an oversized message leaves the stream untouched.
    pub async fn send<M: prost::Message>(&mut self, message: &M) -> Result<(), IpcError> {
        let bytes = protocol::frame::encode_message(self.channel, message)?;
        self.write_all(&bytes).await
    }

    /// The encoded body/prefix are erased after I/O, including cancelled/failed sends.
    pub async fn send_sensitive<M: prost::Message>(&mut self, message: &M) -> Result<(), IpcError> {
        let bytes = Zeroizing::new(protocol::frame::encode_message(self.channel, message)?);
        self.write_all(&bytes).await
    }

    /// Encode and write an already-encoded body.
    pub async fn send_bytes(&mut self, body: &[u8]) -> Result<(), IpcError> {
        let bytes = protocol::frame::encode(self.channel, body)?;
        self.write_all(&bytes).await
    }

    async fn write_all(&mut self, bytes: &[u8]) -> Result<(), IpcError> {
        self.io
            .write_all(bytes)
            .await
            .map_err(|source| IpcError::io("writing an IPC frame", source))?;
        self.io
            .flush()
            .await
            .map_err(|source| IpcError::io("flushing an IPC frame", source))
    }
}

impl<S: AsyncRead + Unpin> Framed<S> {
    /// Read one message.
    pub async fn recv<M>(&mut self) -> Result<M, IpcError>
    where
        M: prost::Message + Default,
    {
        let body = self.recv_bytes().await?;
        M::decode(body.as_slice()).map_err(|_| IpcError::Malformed {
            expected: core::any::type_name::<M>(),
        })
    }

    /// Read one frame body.
    pub async fn recv_bytes(&mut self) -> Result<Vec<u8>, IpcError> {
        loop {
            let (body, consumed) = match self.decoder.decode(&self.buffer)? {
                Some(frame) => (frame.body().to_vec(), frame.consumed_len()),
                None => {
                    self.fill().await?;
                    continue;
                }
            };
            self.buffer.drain(..consumed);
            return Ok(body);
        }
    }

    /// Reuses the same framing/length checks, but erases consumed bytes and stack reads.
    pub async fn recv_sensitive_bytes(&mut self) -> Result<Zeroizing<Vec<u8>>, IpcError> {
        loop {
            if let Some(frame) = self.decoder.decode(&self.buffer)? {
                let consumed = frame.consumed_len();
                let body = Zeroizing::new(frame.body().to_vec());
                let old_len = self.buffer.len();
                self.buffer[..consumed].zeroize();
                self.buffer.drain(..consumed);
                let remaining = self.buffer.len();
                self.buffer.resize(old_len, 0);
                self.buffer[remaining..].zeroize();
                self.buffer.truncate(remaining);
                return Ok(body);
            }
            let mut chunk = Zeroizing::new([0u8; READ_CHUNK]);
            let read = self
                .io
                .read(&mut chunk[..])
                .await
                .map_err(|source| IpcError::io("reading private IPC", source))?;
            if read == 0 {
                return Err(IpcError::Closed);
            }
            self.buffer.extend_from_slice(&chunk[..read]);
        }
    }

    async fn fill(&mut self) -> Result<(), IpcError> {
        debug_assert!(self.buffer.len() <= self.max_buffered_len());
        let mut chunk = [0u8; READ_CHUNK];
        let read = self
            .io
            .read(&mut chunk)
            .await
            .map_err(|source| IpcError::io("reading an IPC frame", source))?;
        if read == 0 {
            return Err(IpcError::Closed);
        }
        self.buffer.extend_from_slice(&chunk[..read]);
        Ok(())
    }
}
