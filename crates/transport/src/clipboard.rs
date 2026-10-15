//! The clipboard channel: port `47902`, ALPN `lan-workspace/clipboard/1`.
//!
//! `docs/05-protocol.md` («Clipboard канал») gives the clipboard its own TLS session,
//! separate from the input/control link, so a 1 MiB transfer cannot delay a key-up
//! (`KVM-042`). The relation between the two is a **grant**: the input link hands out a
//! `ClipboardGrant { channel_id, binding_token, ttl_ms }`, the peer dials `47902` — reusing
//! the parent input connection's `connection_id` — and its first frame is a `ClipboardBind`
//! that the grant is checked against.
//!
//! This module is deliberately split in three:
//!
//! * [`ClipboardMessage`] is the pure wire vocabulary: `Envelope` ↔ message, plus the
//!   post-decode limit checks ([`ClipboardMessage::validate`]).
//! * [`Grants`] is the pure bookkeeping: issue, accept exactly once, expire, close with the
//!   parent link.
//! * [`Outgoing`] is the pure sender: `clipboard::CHUNK_BYTES` chunks paced by
//!   `clipboard::Rate`.
//!
//! [`ClipboardConnection`], [`dial_clipboard`] and [`ClipboardListener`] add the sockets. The
//! listener stays policy-free: it accepts a connection whose first frame is a `Bind` and
//! returns both, leaving the `Grants` check to core.
//!
//! Content is never logged: only sizes and ids.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use geometry::NodeId;
use protocol::frame::{decode_message, encode_message, Channel};
use protocol::seq::SeqValidator;
use protocol::{limits, wire};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;
use tokio_rustls::{TlsAcceptor, TlsConnector};

use crate::convert::ConvertError;
use crate::link::Stream;
use crate::TransportError;

/// One message of the clipboard channel (docs/protocol.proto `Clipboard*`).
#[derive(Debug, Clone, PartialEq)]
pub enum ClipboardMessage {
    /// `ClipboardGrant`: the input link telling the peer to open a clipboard channel.
    Grant {
        /// The channel this grant is for.
        channel_id: [u8; 16],
        /// The one-use secret the `Bind` must carry.
        binding_token: [u8; 32],
        /// Lifetime of the grant in milliseconds.
        ttl_ms: u32,
        /// The port this node's clipboard listener is bound to; `0` means the default
        /// 47902. `network.port_clipboard` is configurable, so the dialer may not assume
        /// the default: it dials the address the input link comes from, at this port.
        clipboard_port: u32,
    },
    /// `ClipboardBind`: the clipboard channel's first frame.
    Bind {
        /// The parent input connection's `connection_id`.
        parent_connection_id: [u8; 16],
        /// The channel the grant named.
        channel_id: [u8; 16],
        /// The grant's binding token.
        binding_token: [u8; 32],
    },
    /// `ClipboardOffer`: the source announcing a payload.
    Offer {
        /// Identifies the transfer.
        offer_id: [u8; 16],
        /// The node that copied the content.
        origin_node_id: [u8; 32],
        /// The origin's version counter, for echo suppression.
        origin_version: u64,
        /// The MIME type; exactly `clipboard::MIME`.
        mime: String,
        /// Payload size in bytes, ≤ 1 MiB.
        size_bytes: u64,
    },
    /// `ClipboardRequest`: the sink asking for the payload.
    Request {
        /// Identifies the transfer.
        offer_id: [u8; 16],
    },
    /// `ClipboardData`: one chunk.
    Data {
        /// Identifies the transfer.
        offer_id: [u8; 16],
        /// Zero-based chunk index.
        chunk_index: u32,
        /// Number of chunks in the transfer, 1..=64.
        chunk_count: u32,
        /// The chunk, ≤ 16 KiB.
        data: Vec<u8>,
    },
    /// `ClipboardComplete`: the last frame of a transfer.
    Complete {
        /// Identifies the transfer.
        offer_id: [u8; 16],
        /// SHA-256 of the whole payload, exactly 32 bytes.
        sha256: Vec<u8>,
    },
    /// `ClipboardCancel`: either side ending a transfer.
    Cancel {
        /// Identifies the transfer.
        offer_id: [u8; 16],
        /// Why it was cancelled.
        reason: wire::ErrorCode,
    },
}

impl ClipboardMessage {
    /// Builds the `Envelope` for this message.
    ///
    /// `seq`/`connection_id` are the envelope's (the dialer's `connection_id` is reused from
    /// the parent input connection, per `docs/05-protocol.md`).
    pub fn to_envelope(
        &self,
        seq: u64,
        connection_id: &[u8; 16],
    ) -> Result<wire::Envelope, ConvertError> {
        use wire::envelope::Payload;

        let payload = match self {
            ClipboardMessage::Grant {
                channel_id,
                binding_token,
                ttl_ms,
                clipboard_port,
            } => Payload::ClipboardGrant(wire::ClipboardGrant {
                channel_id: channel_id.to_vec(),
                binding_token: binding_token.to_vec(),
                ttl_ms: *ttl_ms,
                clipboard_port: *clipboard_port,
            }),
            ClipboardMessage::Bind {
                parent_connection_id,
                channel_id,
                binding_token,
            } => Payload::ClipboardBind(wire::ClipboardBind {
                parent_connection_id: parent_connection_id.to_vec(),
                channel_id: channel_id.to_vec(),
                binding_token: binding_token.to_vec(),
            }),
            ClipboardMessage::Offer {
                offer_id,
                origin_node_id,
                origin_version,
                mime,
                size_bytes,
            } => Payload::ClipboardOffer(wire::ClipboardOffer {
                offer_id: offer_id.to_vec(),
                origin_node_id: origin_node_id.to_vec(),
                origin_version: *origin_version,
                mime: mime.clone(),
                size_bytes: *size_bytes,
            }),
            ClipboardMessage::Request { offer_id } => {
                Payload::ClipboardRequest(wire::ClipboardRequest {
                    offer_id: offer_id.to_vec(),
                })
            }
            ClipboardMessage::Data {
                offer_id,
                chunk_index,
                chunk_count,
                data,
            } => Payload::ClipboardData(wire::ClipboardData {
                offer_id: offer_id.to_vec(),
                chunk_index: *chunk_index,
                chunk_count: *chunk_count,
                data: data.clone(),
            }),
            ClipboardMessage::Complete { offer_id, sha256 } => {
                Payload::ClipboardComplete(wire::ClipboardComplete {
                    offer_id: offer_id.to_vec(),
                    sha256: sha256.clone(),
                })
            }
            ClipboardMessage::Cancel { offer_id, reason } => {
                Payload::ClipboardCancel(wire::ClipboardCancel {
                    offer_id: offer_id.to_vec(),
                    reason: *reason as i32,
                })
            }
        };
        Ok(wire::Envelope {
            major: limits::PROTOCOL_MAJOR,
            minor: limits::PROTOCOL_MINOR,
            connection_id: connection_id.to_vec(),
            seq,
            lease_id: Vec::new(),
            payload: Some(payload),
        })
    }

    /// Reads a message out of an `Envelope`.
    ///
    /// Any payload arm outside the seven clipboard messages (and an envelope with no
    /// payload at all) is [`ConvertError::Unsupported`]; use [`Self::validate`] for the
    /// post-decode limits.
    pub fn from_envelope(envelope: &wire::Envelope) -> Result<Self, ConvertError> {
        match classify(envelope) {
            Ok(message) => Ok(message),
            Err(DecodeError::Violation(violation)) => Err(violation.into()),
            Err(DecodeError::UnknownEnum { field, value }) => {
                Err(ConvertError::UnknownEnum { field, value })
            }
        }
    }

    /// The protocol limits, checked after decode.
    ///
    /// Offer size ≤ 1 MiB, data chunk ≤ 16 KiB, `chunk_count` 1..=64 with
    /// `chunk_index < chunk_count`, ids of the documented length, `sha256` exactly 32 bytes
    /// and the MIME exactly `clipboard::MIME`.
    pub fn validate(&self) -> Result<(), ClipboardViolation> {
        match self {
            ClipboardMessage::Grant {
                channel_id,
                binding_token,
                ..
            } => {
                arr16(channel_id)?;
                arr32(binding_token)?;
            }
            ClipboardMessage::Bind {
                parent_connection_id,
                channel_id,
                binding_token,
            } => {
                arr16(parent_connection_id)?;
                arr16(channel_id)?;
                arr32(binding_token)?;
            }
            ClipboardMessage::Offer {
                offer_id,
                origin_node_id,
                mime,
                size_bytes,
                ..
            } => {
                arr16(offer_id)?;
                arr32(origin_node_id)?;
                if *size_bytes > limits::MAX_CLIPBOARD_PAYLOAD_BYTES {
                    return Err(ClipboardViolation::TooLarge);
                }
                if mime != clipboard::MIME {
                    return Err(ClipboardViolation::BadMime);
                }
            }
            ClipboardMessage::Request { offer_id } => {
                arr16(offer_id)?;
            }
            ClipboardMessage::Data {
                offer_id,
                chunk_index,
                chunk_count,
                data,
            } => {
                arr16(offer_id)?;
                if !(limits::MIN_CLIPBOARD_CHUNK_COUNT..=limits::MAX_CLIPBOARD_CHUNK_COUNT)
                    .contains(chunk_count)
                {
                    return Err(ClipboardViolation::BadCount);
                }
                if chunk_index >= chunk_count {
                    return Err(ClipboardViolation::BadCount);
                }
                if data.len() > limits::MAX_CLIPBOARD_CHUNK_BYTES {
                    return Err(ClipboardViolation::BadChunk);
                }
            }
            ClipboardMessage::Complete { offer_id, sha256 } => {
                arr16(offer_id)?;
                if sha256.len() != 32 {
                    return Err(ClipboardViolation::BadHash);
                }
            }
            ClipboardMessage::Cancel { offer_id, .. } => {
                arr16(offer_id)?;
            }
        }
        Ok(())
    }
}

/// Why a decoded clipboard message was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ClipboardViolation {
    /// `ClipboardOffer.size_bytes` exceeds 1 MiB.
    #[error("clipboard offer exceeds 1 MiB")]
    TooLarge,
    /// `ClipboardData.data` exceeds 16 KiB.
    #[error("clipboard chunk exceeds 16 KiB")]
    BadChunk,
    /// `ClipboardData.chunk_count` is outside 1..=64, or `chunk_index >= chunk_count`.
    #[error("clipboard chunk count/index is out of range")]
    BadCount,
    /// A clipboard identifier has the wrong length.
    #[error("a clipboard identifier has the wrong length")]
    BadId,
    /// `ClipboardOffer.mime` is not `clipboard::MIME`.
    #[error("clipboard mime is not text/plain;charset=utf-8")]
    BadMime,
    /// `ClipboardComplete.sha256` is not exactly 32 bytes.
    #[error("clipboard sha256 is not 32 bytes")]
    BadHash,
    /// The envelope carries a payload arm that is not a clipboard message.
    #[error("the envelope is not a clipboard message")]
    NotClipboard,
    /// The envelope carries no payload arm at all.
    #[error("the envelope has no payload")]
    Empty,
}

impl From<ClipboardViolation> for ConvertError {
    fn from(violation: ClipboardViolation) -> Self {
        ConvertError::Unsupported(match violation {
            ClipboardViolation::TooLarge => "clipboard offer exceeds 1 MiB",
            ClipboardViolation::BadChunk => "clipboard chunk exceeds 16 KiB",
            ClipboardViolation::BadCount => "clipboard chunk count/index is out of range",
            ClipboardViolation::BadId => "a clipboard identifier has the wrong length",
            ClipboardViolation::BadMime => "clipboard mime is not text/plain;charset=utf-8",
            ClipboardViolation::BadHash => "clipboard sha256 is not 32 bytes",
            ClipboardViolation::NotClipboard => "the envelope is not a clipboard message",
            ClipboardViolation::Empty => "the envelope has no payload",
        })
    }
}

/// Why an `Envelope` did not become a [`ClipboardMessage`].
#[derive(Debug)]
enum DecodeError {
    /// A framing-level refusal.
    Violation(ClipboardViolation),
    /// An enum field carried a number the wire contract does not define.
    UnknownEnum { field: &'static str, value: i32 },
}

/// `Envelope` → the clipboard message it carries, or why not.
fn classify(envelope: &wire::Envelope) -> Result<ClipboardMessage, DecodeError> {
    use wire::envelope::Payload;

    let violation = DecodeError::Violation;
    match envelope.payload.as_ref() {
        Some(Payload::ClipboardGrant(grant)) => Ok(ClipboardMessage::Grant {
            channel_id: arr16(&grant.channel_id).map_err(violation)?,
            binding_token: arr32(&grant.binding_token).map_err(violation)?,
            ttl_ms: grant.ttl_ms,
            clipboard_port: grant.clipboard_port,
        }),
        Some(Payload::ClipboardBind(bind)) => Ok(ClipboardMessage::Bind {
            parent_connection_id: arr16(&bind.parent_connection_id).map_err(violation)?,
            channel_id: arr16(&bind.channel_id).map_err(violation)?,
            binding_token: arr32(&bind.binding_token).map_err(violation)?,
        }),
        Some(Payload::ClipboardOffer(offer)) => Ok(ClipboardMessage::Offer {
            offer_id: arr16(&offer.offer_id).map_err(violation)?,
            origin_node_id: arr32(&offer.origin_node_id).map_err(violation)?,
            origin_version: offer.origin_version,
            mime: offer.mime.clone(),
            size_bytes: offer.size_bytes,
        }),
        Some(Payload::ClipboardRequest(request)) => Ok(ClipboardMessage::Request {
            offer_id: arr16(&request.offer_id).map_err(violation)?,
        }),
        Some(Payload::ClipboardData(data)) => Ok(ClipboardMessage::Data {
            offer_id: arr16(&data.offer_id).map_err(violation)?,
            chunk_index: data.chunk_index,
            chunk_count: data.chunk_count,
            data: data.data.clone(),
        }),
        Some(Payload::ClipboardComplete(complete)) => Ok(ClipboardMessage::Complete {
            offer_id: arr16(&complete.offer_id).map_err(violation)?,
            sha256: complete.sha256.clone(),
        }),
        Some(Payload::ClipboardCancel(cancel)) => Ok(ClipboardMessage::Cancel {
            offer_id: arr16(&cancel.offer_id).map_err(violation)?,
            reason: wire::ErrorCode::try_from(cancel.reason).map_err(|_| {
                DecodeError::UnknownEnum {
                    field: "ClipboardCancel.reason",
                    value: cancel.reason,
                }
            })?,
        }),
        Some(_) => Err(violation(ClipboardViolation::NotClipboard)),
        None => Err(violation(ClipboardViolation::Empty)),
    }
}

fn arr16(bytes: &[u8]) -> Result<[u8; 16], ClipboardViolation> {
    <[u8; 16]>::try_from(bytes).map_err(|_| ClipboardViolation::BadId)
}

fn arr32(bytes: &[u8]) -> Result<[u8; 32], ClipboardViolation> {
    <[u8; 32]>::try_from(bytes).map_err(|_| ClipboardViolation::BadId)
}

/// One issued grant, waiting for its `ClipboardBind`.
#[derive(Debug)]
struct Grant {
    parent: [u8; 16],
    token: [u8; 32],
    expires_ms: u64,
}

/// The pure grant bookkeeping: `ClipboardGrant` issuance and one-use `ClipboardBind`
/// acceptance with the 60 s ttl (`clipboard::CHANNEL_TTL_MS`).
#[derive(Debug, Default)]
pub struct Grants {
    open: std::collections::BTreeMap<[u8; 16], Grant>,
}

impl Grants {
    /// Mints a fresh `(channel_id, binding_token)` bound to `parent_connection_id`.
    ///
    /// The identifiers come from the OS entropy source; the API is infallible, so a failed
    /// entropy source is unrecoverable here and panics rather than issuing a guessable token.
    pub fn issue(&mut self, parent_connection_id: [u8; 16], now_ms: u64) -> ([u8; 16], [u8; 32]) {
        let channel_id = identity::random16().expect("system entropy source");
        let mut binding_token = [0u8; 32];
        binding_token[..16].copy_from_slice(&identity::random16().expect("system entropy source"));
        binding_token[16..].copy_from_slice(&identity::random16().expect("system entropy source"));
        self.open.insert(
            channel_id,
            Grant {
                parent: parent_connection_id,
                token: binding_token,
                expires_ms: now_ms.saturating_add(clipboard::CHANNEL_TTL_MS),
            },
        );
        (channel_id, binding_token)
    }

    /// Accepts a bind exactly once: the channel must be known and unexpired, and the parent
    /// and token must match. A second bind with the same token fails with
    /// [`ClipboardViolation::BadId`].
    pub fn accept(
        &mut self,
        parent: &[u8; 16],
        channel_id: &[u8; 16],
        token: &[u8; 32],
        now_ms: u64,
    ) -> Result<(), ClipboardViolation> {
        let Some(grant) = self.open.get(channel_id) else {
            return Err(ClipboardViolation::BadId);
        };
        if now_ms > grant.expires_ms {
            return Err(ClipboardViolation::BadId);
        }
        if grant.parent != *parent || grant.token != *token {
            return Err(ClipboardViolation::BadId);
        }
        self.open.remove(channel_id);
        Ok(())
    }

    /// Drops every grant of a parent link: closing the input link closes its clipboard
    /// channel.
    pub fn close_parent(&mut self, parent: &[u8; 16]) {
        self.open.retain(|_, grant| grant.parent != *parent);
    }

    /// Grants still open (diagnostics/tests).
    pub fn open(&self) -> usize {
        self.open.len()
    }

    /// When the grant for `channel_id` expires, on the clock the issuer stamped it from
    /// (diagnostics/tests).
    ///
    /// One clock matters: [`Grants::accept`] compares this value, so the issuer and the
    /// acceptor must share it (audit F5).
    #[must_use]
    pub fn expires_ms(&self, channel_id: &[u8; 16]) -> Option<u64> {
        self.open.get(channel_id).map(|grant| grant.expires_ms)
    }
}

/// One dialed or accepted clipboard connection: TLS, ALPN clipboard/1,
/// `Channel::Clipboard` framing.
pub struct ClipboardConnection {
    /// The peer's `node_id`, taken from its certificate.
    pub peer: NodeId,
    /// The channel from the `Bind`.
    pub channel_id: [u8; 16],
    stream: Stream,
    /// The parent input connection's id, learned from the first frame.
    connection_id: Option<[u8; 16]>,
    /// The last outbound `seq`; the dialer's `Bind` is `1`.
    out_seq: u64,
    /// The last accepted inbound `seq`.
    in_seq: u64,
    inbound: SeqValidator,
}

impl ClipboardConnection {
    /// Sends one message, stamping the envelope `seq` and `connection_id`.
    pub async fn send(&mut self, message: ClipboardMessage) -> Result<(), TransportError> {
        send_message(
            &mut self.stream,
            &self.connection_id,
            &mut self.out_seq,
            message,
        )
        .await
    }

    /// Receives one message, checking the version, the parent `connection_id`, the strictly
    /// increasing `seq` and the post-decode limits.
    pub async fn recv(&mut self) -> Result<ClipboardMessage, TransportError> {
        recv_message(
            &mut self.stream,
            &mut self.connection_id,
            &mut self.in_seq,
            &mut self.inbound,
        )
        .await
    }

    /// Splits into the read half and the write half; the peer id and channel id stay on both.
    ///
    /// The halves share the TLS stream (`tokio::io::split`), so a reader blocked in
    /// [`ClipboardReader::recv`] never blocks a [`ClipboardWriter::send`] and vice versa.
    pub fn into_split(self) -> (ClipboardReader, ClipboardWriter) {
        let (read, write) = tokio::io::split(self.stream);
        let reader = ClipboardReader {
            peer: self.peer,
            channel_id: self.channel_id,
            stream: read,
            connection_id: self.connection_id,
            in_seq: self.in_seq,
            inbound: self.inbound,
        };
        let writer = ClipboardWriter {
            peer: self.peer,
            channel_id: self.channel_id,
            stream: write,
            connection_id: self.connection_id,
            out_seq: self.out_seq,
        };
        (reader, writer)
    }

    /// Closes the TLS stream.
    pub async fn close(mut self) {
        let _ = self.stream.shutdown().await;
    }

    /// The last sequence numbers sent and accepted (out, in).
    pub fn seq(&self) -> (u64, u64) {
        (self.out_seq, self.in_seq)
    }
}

/// The read half of a [`ClipboardConnection`].
pub struct ClipboardReader {
    /// The peer's `node_id`.
    pub peer: NodeId,
    /// The channel from the `Bind`.
    pub channel_id: [u8; 16],
    stream: tokio::io::ReadHalf<Stream>,
    connection_id: Option<[u8; 16]>,
    in_seq: u64,
    inbound: SeqValidator,
}

impl ClipboardReader {
    /// Receives one message; see [`ClipboardConnection::recv`].
    pub async fn recv(&mut self) -> Result<ClipboardMessage, TransportError> {
        recv_message(
            &mut self.stream,
            &mut self.connection_id,
            &mut self.in_seq,
            &mut self.inbound,
        )
        .await
    }
}

/// The write half of a [`ClipboardConnection`].
pub struct ClipboardWriter {
    /// The peer's `node_id`.
    pub peer: NodeId,
    /// The channel from the `Bind`.
    pub channel_id: [u8; 16],
    stream: tokio::io::WriteHalf<Stream>,
    connection_id: Option<[u8; 16]>,
    out_seq: u64,
}

impl ClipboardWriter {
    /// Sends one message; see [`ClipboardConnection::send`].
    pub async fn send(&mut self, message: ClipboardMessage) -> Result<(), TransportError> {
        send_message(
            &mut self.stream,
            &self.connection_id,
            &mut self.out_seq,
            message,
        )
        .await
    }
}

async fn send_message<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    connection_id: &Option<[u8; 16]>,
    out_seq: &mut u64,
    message: ClipboardMessage,
) -> Result<(), TransportError> {
    message
        .validate()
        .map_err(|violation| TransportError::Protocol(format!("{violation}")))?;
    let Some(connection_id) = connection_id else {
        return Err(TransportError::Protocol(
            "the clipboard connection_id is not established yet".into(),
        ));
    };
    let seq = *out_seq + 1;
    let envelope = message
        .to_envelope(seq, connection_id)
        .map_err(|error| TransportError::Protocol(format!("{error}")))?;
    let bytes = encode_message(Channel::Clipboard, &envelope)
        .map_err(|error| TransportError::Protocol(format!("{error:?}")))?;
    writer.write_all(&bytes).await?;
    *out_seq = seq;
    Ok(())
}

async fn recv_message<R: AsyncRead + Unpin>(
    reader: &mut R,
    connection_id: &mut Option<[u8; 16]>,
    in_seq: &mut u64,
    inbound: &mut SeqValidator,
) -> Result<ClipboardMessage, TransportError> {
    let envelope = read_frame(reader).await?;
    if envelope.major != limits::PROTOCOL_MAJOR || envelope.minor != limits::PROTOCOL_MINOR {
        return Err(TransportError::Protocol(format!(
            "clipboard envelope version {}.{} is not {}.{}",
            envelope.major,
            envelope.minor,
            limits::PROTOCOL_MAJOR,
            limits::PROTOCOL_MINOR
        )));
    }
    let seen = arr16(&envelope.connection_id)
        .map_err(|violation| TransportError::Protocol(format!("{violation}")))?;
    match *connection_id {
        Some(expected) if seen != expected => {
            return Err(TransportError::Protocol(
                "clipboard envelope connection_id does not match the parent input connection"
                    .into(),
            ));
        }
        // The first frame defines the connection: its envelope carries the parent's id.
        None => *connection_id = Some(seen),
        Some(_) => {}
    }
    inbound
        .accept(envelope.seq)
        .map_err(|error| TransportError::Protocol(format!("clipboard seq: {error:?}")))?;
    let message = match classify(&envelope) {
        Ok(message) => message,
        Err(error) => {
            return Err(TransportError::Protocol(format!(
                "clipboard envelope: {error:?}"
            )))
        }
    };
    message
        .validate()
        .map_err(|violation| TransportError::Protocol(format!("clipboard: {violation}")))?;
    *in_seq = envelope.seq;
    Ok(message)
}

/// Reads one `Channel::Clipboard` frame, refusing a length outside the channel limit before
/// allocating the body.
async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<wire::Envelope, TransportError> {
    let mut prefix = [0u8; 4];
    reader.read_exact(&mut prefix).await?;
    let length = u32::from_be_bytes(prefix) as usize;
    let limit = Channel::Clipboard.max_frame_len();
    if length == 0 || length > limit {
        return Err(TransportError::Protocol(format!(
            "clipboard frame length {length} outside 1..={limit}"
        )));
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).await?;
    // Re-attach the prefix so the protocol crate performs the authoritative checks.
    let mut framed = Vec::with_capacity(4 + length);
    framed.extend_from_slice(&prefix);
    framed.extend_from_slice(&body);
    decode_message::<wire::Envelope>(Channel::Clipboard, &framed)
        .map_err(|error| TransportError::Protocol(format!("{error:?}")))?
        .ok_or_else(|| TransportError::Protocol("incomplete clipboard frame".into()))
}

/// Dialer: TLS to `address` with ALPN clipboard/1, then the first frame is `Bind`.
pub async fn dial_clipboard(
    identity: &identity::NodeIdentity,
    trust: impl Into<identity::TrustPolicy>,
    peer: NodeId,
    address: SocketAddr,
    parent_connection_id: [u8; 16],
    channel_id: [u8; 16],
    binding_token: [u8; 32],
) -> Result<ClipboardConnection, TransportError> {
    let config = identity::clipboard_client_config(identity, trust)?;
    let connector = TlsConnector::from(Arc::new(config));
    let tcp = TcpStream::connect(address).await?;
    tcp.set_nodelay(true)?;
    let stream = connector
        .connect(identity::server_name(address.ip()), tcp)
        .await?;
    if stream.get_ref().1.alpn_protocol() != Some(limits::ALPN_CLIPBOARD.as_bytes()) {
        return Err(TransportError::Protocol(
            "the peer did not negotiate lan-workspace/clipboard/1".into(),
        ));
    }
    // The pin already refused anyone else; this makes the expected identity explicit, so a
    // caller that dials one of several addresses learns that the certificate was not the
    // peer it meant to reach.
    let pinned = stream
        .get_ref()
        .1
        .peer_certificates()
        .map(identity::peer_node_id)
        .transpose()
        .map_err(|error| TransportError::Protocol(format!("clipboard peer certificate: {error}")))?
        .ok_or_else(|| {
            TransportError::Protocol("the clipboard peer presented no certificate".into())
        })?;
    if pinned != peer {
        return Err(TransportError::Protocol(format!(
            "the clipboard peer pinned {pinned}, not the expected {peer}"
        )));
    }
    let mut connection = ClipboardConnection {
        peer,
        channel_id,
        stream: Stream::Client(Box::new(stream)),
        connection_id: Some(parent_connection_id),
        out_seq: 0,
        in_seq: 0,
        inbound: SeqValidator::new(),
    };
    // «Первый Envelope отправляет дозванивающийся узел с seq=1 и ClipboardBind».
    connection
        .send(ClipboardMessage::Bind {
            parent_connection_id,
            channel_id,
            binding_token,
        })
        .await?;
    Ok(connection)
}

/// Listener on `port_clipboard`: binds, TLS-accepts with ALPN clipboard/1, reads the first
/// frame (which MUST be a `Bind`, else the connection is dropped and `accept()` keeps
/// waiting), and returns the connection together with that `Bind`. Validation against
/// [`Grants`] belongs to the caller (core), so the listener stays policy-free.
///
/// Each accepted socket is handshaked in a task of its own with a deadline ([`ACCEPT_TIMEOUT`]),
/// so a peer that connects and then stalls — a claim on the TLS handshake, or a connection
/// that never sends its `Bind` — cannot hold up the connections behind it (head-of-line DoS).
pub struct ClipboardListener {
    listener: TcpListener,
    acceptor: TlsAcceptor,
    /// The connections currently being handshaked, one task each.
    pending: JoinSet<Option<(ClipboardConnection, ClipboardMessage)>>,
}

/// How long one clipboard TLS handshake (or its first `Bind`) may take.
///
/// A remote peer that opens the socket and then says nothing must not pin the listener: past
/// this window the connection is dropped and the loop keeps serving.
pub const ACCEPT_TIMEOUT: Duration = Duration::from_secs(5);

impl ClipboardListener {
    /// Binds `listen` and builds the pinned clipboard TLS acceptor.
    pub async fn bind(
        identity: Arc<identity::NodeIdentity>,
        trust: impl Into<identity::TrustPolicy>,
        listen: SocketAddr,
    ) -> Result<Self, TransportError> {
        let config = identity::clipboard_server_config(&identity, trust)?;
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = crate::manager::bind_listener(listen).await?;
        Ok(Self {
            listener,
            acceptor,
            pending: JoinSet::new(),
        })
    }

    /// The address the listener is bound to.
    pub fn local_addr(&self) -> SocketAddr {
        self.listener
            .local_addr()
            .expect("a bound listener has a local address")
    }

    /// Closes the listening socket, then aborts and joins every pending TLS/bind handshake.
    pub async fn shutdown(self) {
        let Self { listener, acceptor: _, mut pending } = self;
        drop(listener);
        pending.shutdown().await;
    }

    /// Accepts the next clipboard connection whose first frame is a `Bind`.
    ///
    /// A handshake on another ALPN, an unpinned peer, a `Bind`-less first frame, a frame that
    /// fails validation, or a peer that misses [`ACCEPT_TIMEOUT`] drops that connection; the
    /// loop keeps waiting. The handshakes run concurrently, so a stalled peer never delays a
    /// valid one.
    pub async fn accept(
        &mut self,
    ) -> Result<(ClipboardConnection, ClipboardMessage), TransportError> {
        loop {
            tokio::select! {
                accepted = self.listener.accept() => {
                    let (tcp, _address) = accepted?;
                    tcp.set_nodelay(true)?;
                    let acceptor = self.acceptor.clone();
                    self.pending
                        .spawn(async move { handshake_clipboard(acceptor, tcp).await });
                }
                joined = self.pending.join_next(), if !self.pending.is_empty() => {
                    match joined {
                        Some(Ok(Some(pair))) => return Ok(pair),
                        // A refused handshake, a missing `Bind`, a timeout, or a panic: the
                        // connection is gone, the loop keeps serving.
                        Some(Ok(None)) | Some(Err(_)) | None => continue,
                    }
                }
            }
        }
    }
}

/// TLS-accepts one clipboard connection and reads its first `Bind`, with a deadline on both.
///
/// `None` is any refusal: the handshake failed or timed out, another ALPN was negotiated, the
/// certificate was unpinned, or the first frame was not a `Bind`.
async fn handshake_clipboard(
    acceptor: TlsAcceptor,
    tcp: TcpStream,
) -> Option<(ClipboardConnection, ClipboardMessage)> {
    let stream = match tokio::time::timeout(ACCEPT_TIMEOUT, acceptor.accept(tcp)).await {
        Ok(Ok(stream)) => stream,
        _ => return None,
    };
    if stream.get_ref().1.alpn_protocol() != Some(limits::ALPN_CLIPBOARD.as_bytes()) {
        return None;
    }
    let certificate = stream
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|certificates| certificates.first().cloned())?;
    let peer = identity::peer_node_id(std::slice::from_ref(&certificate)).ok()?;
    let mut connection = ClipboardConnection {
        peer,
        channel_id: [0u8; 16],
        stream: Stream::Server(Box::new(stream)),
        connection_id: None,
        out_seq: 0,
        in_seq: 0,
        inbound: SeqValidator::new(),
    };
    match tokio::time::timeout(ACCEPT_TIMEOUT, connection.recv()).await {
        Ok(Ok(bind @ ClipboardMessage::Bind { channel_id, .. })) => {
            connection.channel_id = channel_id;
            Some((connection, bind))
        }
        // Not a `Bind` (or a malformed frame), or no first frame in time: drop it.
        _ => None,
    }
}

/// The sender's side of one transfer: splits the payload into `clipboard::CHUNK_BYTES`
/// chunks and paces them with `clipboard::Rate`.
pub struct Outgoing {
    offer_id: [u8; 16],
    payload: Vec<u8>,
    chunk_count: u32,
    next_index: u32,
    rate: clipboard::Rate,
    sha256: Vec<u8>,
    complete_sent: bool,
}

impl Outgoing {
    /// Plans a transfer.
    ///
    /// Fails with [`ClipboardViolation::TooLarge`] when `payload` exceeds `max_bytes` or the
    /// protocol 1 MiB cap.
    pub fn new(
        offer_id: [u8; 16],
        payload: Vec<u8>,
        rate_bytes_per_s: u64,
        max_bytes: u64,
        now_ms: u64,
    ) -> Result<Self, ClipboardViolation> {
        let size = payload.len() as u64;
        if size > max_bytes || size > limits::MAX_CLIPBOARD_PAYLOAD_BYTES {
            return Err(ClipboardViolation::TooLarge);
        }
        // An empty payload is one empty chunk, never zero chunks (`chunk_count` is 1..=64).
        let chunk_count = if payload.is_empty() {
            1
        } else {
            payload.len().div_ceil(clipboard::CHUNK_BYTES)
        };
        let chunk_count = u32::try_from(chunk_count).map_err(|_| ClipboardViolation::BadCount)?;
        let sha256 = clipboard::sha256(&payload);
        Ok(Outgoing {
            offer_id,
            payload,
            chunk_count,
            next_index: 0,
            rate: clipboard::Rate::new(rate_bytes_per_s, now_ms),
            sha256,
            complete_sent: false,
        })
    }

    /// The next message to send plus how long to wait first (`0` = now). `None` once the
    /// `Complete` has been handed out.
    pub fn next(&mut self, now_ms: u64) -> Option<(ClipboardMessage, u64)> {
        if self.next_index < self.chunk_count {
            let start = self.next_index as usize * clipboard::CHUNK_BYTES;
            let end = (start + clipboard::CHUNK_BYTES).min(self.payload.len());
            let data = self.payload[start..end].to_vec();
            let delay = self.rate.delay_ms(data.len() as u64, now_ms);
            let message = ClipboardMessage::Data {
                offer_id: self.offer_id,
                chunk_index: self.next_index,
                chunk_count: self.chunk_count,
                data,
            };
            self.next_index += 1;
            return Some((message, delay));
        }
        if !self.complete_sent {
            self.complete_sent = true;
            return Some((
                ClipboardMessage::Complete {
                    offer_id: self.offer_id,
                    sha256: self.sha256.clone(),
                },
                0,
            ));
        }
        None
    }

    /// The payload size in bytes.
    pub fn size_bytes(&self) -> u64 {
        self.payload.len() as u64
    }

    /// SHA-256 of the whole payload.
    pub fn sha256(&self) -> &[u8] {
        &self.sha256
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use identity::{IdentityPaths, NodeIdentity, TrustStore, TrustedPeer};

    #[tokio::test]
    async fn shutdown_joins_a_silent_handshake_and_releases_the_port() {
        use tokio::io::AsyncReadExt;
        let root = std::env::temp_dir().join(format!(
            "lws-clipboard-shutdown-{}-{:x}",
            std::process::id(),
            u128::from_le_bytes(identity::random16().expect("entropy"))
        ));
        let identity = Arc::new(
            NodeIdentity::load_or_create(&IdentityPaths::rooted_at(&root))
                .expect("identity").0,
        );
        let mut listener = ClipboardListener::bind(
            Arc::clone(&identity), TrustStore::new(), "127.0.0.1:0".parse().expect("address"),
        ).await.expect("listener");
        let address = listener.local_addr();
        let mut silent = TcpStream::connect(address).await.expect("connect");
        assert!(tokio::time::timeout(std::time::Duration::from_millis(50), listener.accept())
            .await.is_err(), "silent peer cannot complete TLS");
        assert_eq!(listener.pending.len(), 1, "handshake task must actually be active");
        tokio::time::timeout(std::time::Duration::from_secs(1), listener.shutdown())
            .await.expect("shutdown must not wait for the handshake deadline");
        let mut byte = [0];
        let read = tokio::time::timeout(std::time::Duration::from_secs(1), silent.read(&mut byte))
            .await.expect("silent socket closes");
        assert!(matches!(read, Ok(0) | Err(_)));
        let replacement = ClipboardListener::bind(identity, TrustStore::new(), address)
            .await.expect("exact port is released");
        replacement.shutdown().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn established_clipboard_links_disable_nagle_on_both_ends() {
        let root = std::env::temp_dir().join(format!(
            "lws-nodelay-{}-{:x}",
            std::process::id(),
            u128::from_le_bytes(identity::random16().expect("entropy"))
        ));
        let server = Arc::new(
            NodeIdentity::load_or_create(&IdentityPaths::rooted_at(root.join("server")))
                .expect("server identity")
                .0,
        );
        let client = NodeIdentity::load_or_create(&IdentityPaths::rooted_at(root.join("client")))
            .expect("client identity")
            .0;
        let mut server_trust = TrustStore::new();
        server_trust.trust(TrustedPeer::paired(client.node_id(), "client", None, 0));
        let mut client_trust = TrustStore::new();
        client_trust.trust(TrustedPeer::paired(server.node_id(), "server", None, 0));
        let mut listener = ClipboardListener::bind(
            Arc::clone(&server),
            server_trust,
            "127.0.0.1:0".parse().expect("address"),
        )
        .await
        .expect("listener");
        let address = listener.local_addr();
        let (accepted, dialed) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::join!(
                listener.accept(),
                dial_clipboard(
                    &client,
                    &client_trust,
                    server.node_id(),
                    address,
                    [1; 16],
                    [2; 16],
                    [3; 32],
                )
            )
        })
        .await
        .expect("TLS link deadline");
        let (accepted, _) = accepted.expect("accepted TLS link");
        let dialed = dialed.expect("dialed TLS link");
        let Stream::Server(accepted_stream) = &accepted.stream else {
            panic!("accepted stream");
        };
        let Stream::Client(dialed_stream) = &dialed.stream else {
            panic!("dialed stream");
        };
        assert!(accepted_stream
            .get_ref()
            .0
            .nodelay()
            .expect("accepted option"));
        assert!(dialed_stream.get_ref().0.nodelay().expect("dialed option"));
        drop((accepted, dialed, listener, server, client));
        std::fs::remove_dir_all(root).expect("remove identities");
    }
}
