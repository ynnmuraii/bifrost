//! The async driver: tokio + tokio-rustls around the sans-IO ceremony.
//!
//! This module owns everything that touches the world: the TCP socket, the TLS
//! handshake, the exporter query and the `PairingEnvelope` framing. It feeds
//! [`crate::pairing::PairingFsm`] and executes its outputs, so the ceremony rules stay
//! testable without a socket while the demo still exercises the real stack.
//!
//! # Message order (`docs/05-protocol.md`)
//!
//! «узел, открывший окно сопряжения, — TLS-сервер и отправляет `PairHello` первым, второй
//! узел дозванивается и отвечает тем же сообщением». The dialer therefore learns the
//! `attempt_id` from the opener's first envelope — and only then can it compute the
//! comparison code, because the code's exporter context *is* that `attempt_id`.
//!
//! # Framing
//!
//! Every message is `4-byte big-endian length || PairingEnvelope` on
//! [`Channel::Pairing`], whose limit is `MAX_FRAME_PAIRING = 8192` bytes
//! (`crates/protocol`). The driver reads the four prefix bytes first and refuses a length
//! outside `1..=limit` before allocating anything of that size, then delegates the decode
//! to `protocol::frame::decode_message` so the authoritative limit checks live in exactly
//! one place.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use protocol::frame::{decode_message, encode_message, Channel};
use protocol::wire::{pairing_envelope, PairHello, PairingEnvelope};
use rustls::pki_types::CertificateDer;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::{TlsAcceptor, TlsConnector};

use geometry::NodeId;

use crate::config::{pairing_client_config, pairing_server_config, server_name};
use crate::key::{IdentityError, NodeIdentity};
use crate::pairing::{
    AttemptId, PairingFsm, PairingInput, PairingOutput, PairingState, PairingWindow,
    RejectionReason,
};
use crate::sas::{SasCode, EXPORTER_BYTES, EXPORTER_LABEL};
use crate::spki::{fingerprint, fingerprint_grouped};
use crate::trust::TrustedPeer;
use crate::verify::peer_node_id;

/// Failures of the pairing driver.
#[derive(Debug)]
pub enum PairError {
    /// Socket or stream I/O failed.
    Io(std::io::Error),
    /// The TLS handshake failed.
    Tls(rustls::Error),
    /// A peer certificate was unusable (see [`crate::verify::pin_error`] for a pin
    /// refusal inside a handshake error).
    Pin(crate::verify::PinError),
    /// A frame was malformed, oversized, or an unexpected message arrived.
    Protocol(String),
    /// The ceremony rejected the peer.
    Rejected(RejectionReason),
    /// The local identity could not be prepared.
    Identity(IdentityError),
    /// The ceremony did not finish inside the window.
    Timeout,
}

impl std::fmt::Display for PairError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PairError::Io(error) => write!(f, "socket error: {error}"),
            PairError::Tls(error) => write!(f, "tls handshake failed: {error}"),
            PairError::Pin(error) => write!(f, "{error}"),
            PairError::Protocol(detail) => write!(f, "pairing protocol error: {detail}"),
            PairError::Rejected(reason) => write!(f, "pairing rejected: {reason:?}"),
            PairError::Identity(error) => write!(f, "{error}"),
            PairError::Timeout => write!(f, "the pairing window ran out"),
        }
    }
}

impl std::error::Error for PairError {}

impl From<std::io::Error> for PairError {
    fn from(error: std::io::Error) -> Self {
        PairError::Io(error)
    }
}

impl From<IdentityError> for PairError {
    fn from(error: IdentityError) -> Self {
        PairError::Identity(error)
    }
}

/// A completed ceremony, from one side's point of view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairedPeer {
    /// The peer's `node_id`.
    pub node_id: NodeId,
    /// The peer's `Hello.display_name`.
    pub display_name: String,
    /// The comparison code both users saw.
    pub sas: SasCode,
    /// Plain hex fingerprint of `node_id`, for "Подробнее".
    pub fingerprint: String,
}

impl PairedPeer {
    /// The `TRUSTED_PEER` record to persist (`paired_at` is the caller's wall clock).
    pub fn trusted_peer(
        &self,
        paired_at_unix_ms: i64,
        last_address: Option<String>,
    ) -> TrustedPeer {
        TrustedPeer::paired(
            self.node_id,
            self.display_name.clone(),
            last_address,
            paired_at_unix_ms,
        )
    }
}

/// Either side of a live pairing connection.
enum Stream {
    /// The window opener's side (TLS server).
    Server(Box<tokio_rustls::server::TlsStream<TcpStream>>),
    /// The dialer's side.
    Client(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

impl Stream {
    fn peer_certificate(&self) -> Option<CertificateDer<'static>> {
        match self {
            Stream::Server(stream) => stream.get_ref().1.peer_certificates()?.first().cloned(),
            Stream::Client(stream) => stream.get_ref().1.peer_certificates()?.first().cloned(),
        }
    }

    /// The RFC 8446 §7.5 exporter, with `attempt_id` as the context.
    fn export_attempt_material(
        &self,
        attempt_id: &AttemptId,
    ) -> Result<[u8; EXPORTER_BYTES], PairError> {
        let output = [0u8; EXPORTER_BYTES];
        let filled =
            match self {
                Stream::Server(stream) => stream.get_ref().1.export_keying_material(
                    output,
                    EXPORTER_LABEL,
                    Some(attempt_id),
                ),
                Stream::Client(stream) => stream.get_ref().1.export_keying_material(
                    output,
                    EXPORTER_LABEL,
                    Some(attempt_id),
                ),
            };
        filled.map_err(|error| PairError::Protocol(format!("export_keying_material: {error}")))
    }

    async fn write_frame(&mut self, envelope: &PairingEnvelope) -> Result<(), PairError> {
        let bytes = encode_message(Channel::Pairing, envelope)
            .map_err(|error| PairError::Protocol(format!("{error:?}")))?;
        match self {
            Stream::Server(stream) => stream.write_all(&bytes).await?,
            Stream::Client(stream) => stream.write_all(&bytes).await?,
        }
        Ok(())
    }

    async fn read_frame(&mut self) -> Result<PairingEnvelope, PairError> {
        let mut prefix = [0u8; 4];
        match self {
            Stream::Server(stream) => stream.read_exact(&mut prefix).await?,
            Stream::Client(stream) => stream.read_exact(&mut prefix).await?,
        };
        let length = u32::from_be_bytes(prefix) as usize;
        let limit = Channel::Pairing.max_frame_len();
        if length == 0 || length > limit {
            return Err(PairError::Protocol(format!(
                "pairing frame length {length} outside 1..={limit}"
            )));
        }
        let mut body = vec![0u8; length];
        match self {
            Stream::Server(stream) => stream.read_exact(&mut body).await?,
            Stream::Client(stream) => stream.read_exact(&mut body).await?,
        };
        // Re-attach the prefix so the protocol crate performs the authoritative checks.
        let mut framed = Vec::with_capacity(4 + length);
        framed.extend_from_slice(&prefix);
        framed.extend_from_slice(&body);
        decode_message::<PairingEnvelope>(Channel::Pairing, &framed)
            .map_err(|error| PairError::Protocol(format!("{error:?}")))?
            .ok_or_else(|| PairError::Protocol("incomplete pairing frame".into()))
    }
}

/// A live ceremony: the code is known, the user decides.
pub struct PairingDriver {
    stream: Stream,
    fsm: PairingFsm,
    peer_node_id: NodeId,
    peer_display_name: String,
    sas: SasCode,
    address: Option<String>,
    /// This node's own `network.port_input`, advertised in `PairHello.listen_port` so the
    /// opener can record a dialer that does not listen on the documented 47901.
    listen_port: u16,
    /// The raw `PairHello.listen_port` the peer sent (0 = it listens on the default).
    peer_listen_port: u16,
}

impl PairingDriver {
    /// The code both users must compare.
    pub const fn sas(&self) -> SasCode {
        self.sas
    }

    /// The peer identity the code authenticates.
    pub const fn peer_node_id(&self) -> &NodeId {
        &self.peer_node_id
    }

    /// The peer's advertised name (non-authoritative: «Название устройства/адрес не
    /// являются идентичностью»).
    pub fn peer_display_name(&self) -> &str {
        &self.peer_display_name
    }

    /// `aa:bb:...` fingerprint, for the "Подробнее" view (ADR-010).
    pub fn peer_fingerprint(&self) -> String {
        fingerprint_grouped(&self.peer_node_id)
    }

    /// The ceremony state.
    pub const fn state(&self) -> PairingState {
        self.fsm.state()
    }

    /// The `attempt_id` of this ceremony.
    pub const fn attempt_id(&self) -> &AttemptId {
        self.fsm.attempt_id()
    }

    /// The address this ceremony used, for `TRUSTED_PEER.last_address`.
    pub fn address(&self) -> Option<&str> {
        self.address.as_deref()
    }

    /// The port the peer says it listens on (`PairHello.listen_port`).
    ///
    /// `0` means the peer left it at the documented default, and the caller turns that
    /// into [`protocol::PORT_INPUT_CONTROL`] when it builds the address. The opener needs
    /// this because it only ever sees the dialer's *source* port, which is ephemeral.
    pub const fn peer_listen_port(&self) -> u16 {
        self.peer_listen_port
    }

    /// Approves locally and finishes with the peer's decision.
    ///
    /// This is the GUI's "the codes are equal" button: it writes `PairDecision{approved}`
    /// and then waits for the peer's answer. The caller persists the returned record
    /// atomically ([`PairedPeer::trusted_peer`] + [`crate::trust::TrustStore::trust`]).
    pub async fn approve(mut self) -> Result<PairedPeer, PairError> {
        let outputs = self
            .fsm
            .handle(PairingInput::LocalDecision { approved: true });
        self.execute(outputs).await?;
        if self.fsm.state() == PairingState::Committed {
            return Ok(self.paired());
        }
        if self.fsm.state() != PairingState::LocalApproved {
            return Err(self.failure());
        }
        let envelope = self.stream.read_frame().await?;
        let decision = decision_of(&envelope)?;
        let addressee = decision_addressee(&envelope)?;
        let outputs = self.fsm.handle(PairingInput::RemoteDecision {
            approved: decision,
            peer_node_id: addressee,
        });
        self.execute(outputs).await?;
        if self.fsm.state() == PairingState::Committed {
            Ok(self.paired())
        } else {
            Err(self.failure())
        }
    }

    /// Rejects locally, tells the peer, and writes nothing (nothing is ever written on
    /// this side, matching «Rejected --> [*]: audit only»).
    pub async fn reject(mut self) -> Result<(), PairError> {
        let outputs = self
            .fsm
            .handle(PairingInput::LocalDecision { approved: false });
        self.execute(outputs).await?;
        Ok(())
    }

    fn paired(&self) -> PairedPeer {
        PairedPeer {
            node_id: self.peer_node_id,
            display_name: self.peer_display_name.clone(),
            sas: self.sas,
            fingerprint: fingerprint(&self.peer_node_id),
        }
    }

    fn failure(&self) -> PairError {
        match self.fsm.rejection() {
            Some(reason) => PairError::Rejected(reason),
            None => PairError::Protocol("the ceremony did not reach Committed".into()),
        }
    }

    async fn execute(&mut self, outputs: Vec<PairingOutput>) -> Result<(), PairError> {
        for output in outputs {
            match output {
                PairingOutput::SendHello {
                    node_id,
                    display_name,
                    supports_sas,
                } => {
                    let envelope = self.envelope(pairing_envelope::Payload::Hello(PairHello {
                        node_id: node_id.as_bytes().to_vec(),
                        display_name,
                        supports_sas,
                        listen_port: u32::from(self.listen_port),
                    }));
                    self.stream.write_frame(&envelope).await?;
                }
                PairingOutput::SendDecision {
                    approved,
                    peer_node_id,
                } => {
                    let envelope = self.envelope(pairing_envelope::Payload::Decision(
                        protocol::wire::PairDecision {
                            approved,
                            peer_node_id: peer_node_id.as_bytes().to_vec(),
                        },
                    ));
                    self.stream.write_frame(&envelope).await?;
                }
                // Handled by the caller (GUI) once `approve`/`reject` is called.
                PairingOutput::ShowSas { .. } | PairingOutput::Commit { .. } => {}
                PairingOutput::Rejected { reason } => return Err(PairError::Rejected(reason)),
                PairingOutput::Expired => return Err(PairError::Timeout),
            }
        }
        Ok(())
    }

    fn envelope(&self, payload: pairing_envelope::Payload) -> PairingEnvelope {
        PairingEnvelope {
            seq: 1,
            attempt_id: self.fsm.attempt_id().to_vec(),
            major: protocol::limits::PROTOCOL_MAJOR,
            minor: protocol::limits::PROTOCOL_MINOR,
            payload: Some(payload),
        }
    }
}

/// Waits for one dialer and runs the opener's half of the ceremony.
///
/// The listener must only be bound while the pairing window is open
/// (`docs/05-protocol.md`: «Пользователь открывает окно сопряжения на узле-сервере на
/// 120 s»), so the caller owns its lifetime; `window` supplies the `attempt_id`, the
/// deadline and the admission limits («не более 3 ожидающих peers»).
pub async fn accept_one(
    listener: &TcpListener,
    identity: &NodeIdentity,
    window: &mut PairingWindow,
    now_ms: u64,
    listen_port: u16,
) -> Result<PairingDriver, PairError> {
    window.admit(now_ms).map_err(PairError::Rejected)?;
    let config = pairing_server_config(identity)?;
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let (tcp, address) = listener.accept().await?;
    let accepted = acceptor.accept(tcp).await.map_err(PairError::Io)?;
    let stream = Stream::Server(Box::new(accepted));
    let result = opener_ceremony(
        stream,
        identity,
        window,
        Some(address.to_string()),
        listen_port,
    )
    .await;
    window.finish();
    result
}

/// Dials a window opener and runs the dialer's half of the ceremony.
pub async fn dial(
    address: SocketAddr,
    identity: &NodeIdentity,
    window: Duration,
    listen_port: u16,
) -> Result<PairingDriver, PairError> {
    let config = pairing_client_config(identity)?;
    let connector = TlsConnector::from(Arc::new(config));
    let tcp = TcpStream::connect(address).await?;
    let connected = connector
        .connect(server_name(address.ip()), tcp)
        .await
        .map_err(PairError::Io)?;
    let stream = Stream::Client(Box::new(connected));
    let ceremony = dialer_ceremony(stream, identity, Some(address.to_string()), listen_port);
    tokio::time::timeout(window, ceremony)
        .await
        .map_err(|_| PairError::Timeout)?
}

async fn opener_ceremony(
    mut stream: Stream,
    identity: &NodeIdentity,
    window: &PairingWindow,
    address: Option<String>,
    listen_port: u16,
) -> Result<PairingDriver, PairError> {
    let attempt_id: AttemptId = *window.attempt_id();
    let mut fsm = PairingFsm::window_opener(identity.node_id(), "lan-workspace", attempt_id);
    if let Some(address) = address.clone() {
        fsm.set_address(address);
    }
    fsm.handle(PairingInput::WindowOpened { now_ms: 0 });
    fsm.handle(PairingInput::PeerAdmitted);

    let peer_node_id = presented_node_id(&stream)?;
    let sas = SasCode::from_exporter_bytes(stream.export_attempt_material(&attempt_id)?);

    let outputs = fsm.handle(PairingInput::HandshakeEstablished { peer_node_id });
    run_outputs(&mut stream, &mut fsm, outputs, listen_port).await?;
    // The opener generated the attempt id, so it can compute the code immediately; the
    // ceremony waits for the dialer's hello before Comparing.
    fsm.handle(PairingInput::AttemptMaterial { sas });

    // The dialer's PairHello. Its `listen_port` is the only honest address part the opener
    // can learn: what it sees on the socket is the dialer's ephemeral source port.
    let envelope = stream.read_frame().await?;
    let remote = hello_of(&envelope)?;
    let outputs = fsm.handle(PairingInput::RemoteHello {
        attempt_id: remote.attempt_id,
        node_id: remote.node_id,
        display_name: remote.display_name,
        supports_sas: remote.supports_sas,
    });
    run_outputs(&mut stream, &mut fsm, outputs, listen_port).await?;

    finish_driver(
        stream,
        fsm,
        peer_node_id,
        sas,
        address,
        listen_port,
        remote.listen_port,
    )
}

async fn dialer_ceremony(
    mut stream: Stream,
    identity: &NodeIdentity,
    address: Option<String>,
    listen_port: u16,
) -> Result<PairingDriver, PairError> {
    let mut fsm = PairingFsm::dialer(identity.node_id(), "lan-workspace");
    fsm.handle(PairingInput::DialStarted { now_ms: 0 });
    if let Some(address) = address.clone() {
        fsm.set_address(address);
    }

    let peer_node_id = presented_node_id(&stream)?;
    let outputs = fsm.handle(PairingInput::HandshakeEstablished { peer_node_id });
    run_outputs(&mut stream, &mut fsm, outputs, listen_port).await?;

    // The opener's PairHello carries the attempt id, so the code can only be computed now.
    let envelope = stream.read_frame().await?;
    let remote = hello_of(&envelope)?;
    let sas = SasCode::from_exporter_bytes(stream.export_attempt_material(&remote.attempt_id)?);
    fsm.handle(PairingInput::AttemptMaterial { sas });
    let outputs = fsm.handle(PairingInput::RemoteHello {
        attempt_id: remote.attempt_id,
        node_id: remote.node_id,
        display_name: remote.display_name,
        supports_sas: remote.supports_sas,
    });
    run_outputs(&mut stream, &mut fsm, outputs, listen_port).await?;

    finish_driver(
        stream,
        fsm,
        peer_node_id,
        sas,
        address,
        listen_port,
        remote.listen_port,
    )
}

#[allow(clippy::too_many_arguments)]
fn finish_driver(
    stream: Stream,
    fsm: PairingFsm,
    peer_node_id: NodeId,
    sas: SasCode,
    address: Option<String>,
    listen_port: u16,
    peer_listen_port: u16,
) -> Result<PairingDriver, PairError> {
    if fsm.state() != PairingState::Comparing {
        return Err(match fsm.rejection() {
            Some(reason) => PairError::Rejected(reason),
            None => PairError::Protocol(format!(
                "ceremony ended in {} instead of Comparing",
                fsm.state().as_str()
            )),
        });
    }
    let peer_display_name = fsm.peer_display_name().unwrap_or_default().to_string();
    Ok(PairingDriver {
        stream,
        fsm,
        peer_node_id,
        peer_display_name,
        sas,
        address,
        listen_port,
        peer_listen_port,
    })
}

/// The `node_id` of the certificate the verifier just accepted.
fn presented_node_id(stream: &Stream) -> Result<NodeId, PairError> {
    let certificate = stream.peer_certificate().ok_or_else(|| {
        PairError::Protocol("the peer presented no certificate on the pairing channel".into())
    })?;
    peer_node_id(std::slice::from_ref(&certificate)).map_err(PairError::Pin)
}

/// Executes the ceremony's outputs. The envelopes are built from the FSM's own
/// `attempt_id`, so a message can never carry an `attempt_id` the ceremony did not agree
/// on.
async fn run_outputs(
    stream: &mut Stream,
    fsm: &mut PairingFsm,
    outputs: Vec<PairingOutput>,
    listen_port: u16,
) -> Result<(), PairError> {
    for output in outputs {
        match output {
            PairingOutput::SendHello {
                node_id,
                display_name,
                supports_sas,
            } => {
                let envelope = envelope_of(
                    fsm,
                    pairing_envelope::Payload::Hello(PairHello {
                        node_id: node_id.as_bytes().to_vec(),
                        display_name,
                        supports_sas,
                        listen_port: u32::from(listen_port),
                    }),
                );
                stream.write_frame(&envelope).await?;
            }
            PairingOutput::SendDecision {
                approved,
                peer_node_id,
            } => {
                let envelope = envelope_of(
                    fsm,
                    pairing_envelope::Payload::Decision(protocol::wire::PairDecision {
                        approved,
                        peer_node_id: peer_node_id.as_bytes().to_vec(),
                    }),
                );
                stream.write_frame(&envelope).await?;
            }
            PairingOutput::ShowSas { .. } | PairingOutput::Commit { .. } => {}
            PairingOutput::Rejected { reason } => return Err(PairError::Rejected(reason)),
            PairingOutput::Expired => return Err(PairError::Timeout),
        }
    }
    Ok(())
}

fn envelope_of(fsm: &PairingFsm, payload: pairing_envelope::Payload) -> PairingEnvelope {
    PairingEnvelope {
        seq: 1,
        attempt_id: fsm.attempt_id().to_vec(),
        major: protocol::limits::PROTOCOL_MAJOR,
        minor: protocol::limits::PROTOCOL_MINOR,
        payload: Some(payload),
    }
}

/// The `PairHello` fields this implementation acts on.
struct RemoteHello {
    attempt_id: AttemptId,
    node_id: NodeId,
    display_name: String,
    supports_sas: bool,
    /// Raw `PairHello.listen_port`; `0` means the documented default.
    listen_port: u16,
}

fn hello_of(envelope: &PairingEnvelope) -> Result<RemoteHello, PairError> {
    match &envelope.payload {
        Some(pairing_envelope::Payload::Hello(hello)) => {
            let attempt_id: AttemptId = envelope
                .attempt_id
                .as_slice()
                .try_into()
                .map_err(|_| PairError::Protocol("attempt_id must be 16 bytes".into()))?;
            // A peer built by an older draft sends nothing, which protobuf reads as 0 —
            // exactly the documented default.
            let listen_port =
                u16::try_from(hello.listen_port).unwrap_or(protocol::PORT_INPUT_CONTROL);
            Ok(RemoteHello {
                attempt_id,
                node_id: node_id_from_bytes(&hello.node_id)?,
                display_name: hello.display_name.clone(),
                supports_sas: hello.supports_sas,
                listen_port,
            })
        }
        _ => Err(PairError::Protocol(
            "expected PairHello on the pairing channel".into(),
        )),
    }
}

fn decision_of(envelope: &PairingEnvelope) -> Result<bool, PairError> {
    match &envelope.payload {
        Some(pairing_envelope::Payload::Decision(decision)) => Ok(decision.approved),
        _ => Err(PairError::Protocol(
            "expected PairDecision on the pairing channel".into(),
        )),
    }
}

/// The `peer_node_id` of a `PairDecision`, which this implementation reads as the
/// **addressee** (see the ambiguity note in [`crate::pairing`]).
fn decision_addressee(envelope: &PairingEnvelope) -> Result<NodeId, PairError> {
    match &envelope.payload {
        Some(pairing_envelope::Payload::Decision(decision)) => {
            node_id_from_bytes(&decision.peer_node_id)
        }
        _ => Err(PairError::Protocol("expected PairDecision".into())),
    }
}

fn node_id_from_bytes(bytes: &[u8]) -> Result<NodeId, PairError> {
    let array: [u8; 32] = bytes
        .try_into()
        .map_err(|_| PairError::Protocol("node_id must be exactly 32 bytes".into()))?;
    Ok(NodeId::new(array))
}

/// Binds the pairing listener; bind it only while the window is open.
pub async fn bind(address: SocketAddr) -> Result<TcpListener, PairError> {
    Ok(TcpListener::bind(address).await?)
}

/// Opens a window with a fresh `attempt_id`.
pub fn open_window(now_ms: u64) -> Result<PairingWindow, PairError> {
    Ok(PairingWindow::open(now_ms, crate::key::random16()?))
}
