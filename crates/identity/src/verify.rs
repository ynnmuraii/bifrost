//! Pinned certificate verification: the identity check *is* the SPKI pin.
//!
//! ADR-007: «Принято: mTLS с pin ключей... По сети не пересылается постоянный пароль»,
//! and «mDNS/IP — маршрутизация, не идентичность». ADR-011: trust is pairwise and
//! local. So there is no CA, no webpki path validation and no DNS-name check: a peer is
//! accepted exactly when `SHA-256(SPKI DER)` of its presented certificate is a `node_id`
//! we already trust.
//!
//! # The one place unknown peers are allowed
//!
//! [`TrustPolicy::Pairing`] accepts *any* well-formed certificate, because the pairing
//! ceremony is how a peer becomes known — and that ceremony authenticates through the SAS
//! code, not through the pin. `docs/05-protocol.md`: «Пока ключ неизвестен, allowlist
//! разрешает только `PairHello` и статус», and `docs/05-protocol.md` again: the pairing
//! channel is `lan-workspace/pair/1` and «Нельзя принимать runtime payload на
//! pairing-канале». [`crate::config`] therefore refuses to build a pairing policy for a
//! non-pairing ALPN list, which is what keeps a Pairing-policy handshake off the input
//! and clipboard channels.
//!
//! Signature verification is delegated to the `ring` crypto provider, so the pin does not
//! weaken the handshake: the peer must still prove possession of the key it presented.

use std::collections::BTreeSet;
use std::fmt;
use std::sync::{Arc, RwLock};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{CertificateError, DigitallySignedStruct, DistinguishedName, Error, SignatureScheme};

use geometry::NodeId;

use crate::spki::{node_id_from_certificate, SpkiError};

/// Who may complete the TLS handshake.
#[derive(Debug, Clone)]
pub enum TrustPolicy {
    /// Only peers whose `node_id` is in this set (the active trust records). Used for
    /// the input/control and clipboard channels.
    Pinned(BTreeSet<NodeId>),
    /// A pinned set updated while a link manager is running.
    PinnedShared(Arc<RwLock<BTreeSet<NodeId>>>),
    /// Any well-formed peer certificate; only for `lan-workspace/pair/1`, where the SAS
    /// code is the authentication step.
    Pairing,
}

impl PartialEq for TrustPolicy {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (TrustPolicy::Pinned(left), TrustPolicy::Pinned(right)) => left == right,
            (TrustPolicy::PinnedShared(left), TrustPolicy::PinnedShared(right)) => {
                Arc::ptr_eq(left, right)
            }
            (TrustPolicy::Pairing, TrustPolicy::Pairing) => true,
            _ => false,
        }
    }
}

impl Eq for TrustPolicy {}

impl TrustPolicy {
    /// The pinned `node_id`s, empty for [`TrustPolicy::Pairing`].
    pub fn pinned(&self) -> BTreeSet<NodeId> {
        match self {
            TrustPolicy::Pinned(node_ids) => node_ids.clone(),
            TrustPolicy::PinnedShared(node_ids) => node_ids
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
            TrustPolicy::Pairing => BTreeSet::new(),
        }
    }

    /// Whether this policy lets an unknown peer through.
    pub const fn allows_unknown(&self) -> bool {
        matches!(self, TrustPolicy::Pairing)
    }

    fn allows(&self, node_id: &NodeId) -> bool {
        match self {
            TrustPolicy::Pinned(node_ids) => node_ids.contains(node_id),
            TrustPolicy::PinnedShared(node_ids) => node_ids
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains(node_id),
            TrustPolicy::Pairing => true,
        }
    }
}

/// Why a peer certificate was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinError {
    /// The certificate did not parse or its SPKI could not be extracted.
    MalformedCertificate(SpkiError),
    /// The peer's `node_id` is not in the pinned set, and this channel does not allow
    /// unknown peers.
    NotPinned(String),
}

impl fmt::Display for PinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PinError::MalformedCertificate(error) => {
                write!(f, "peer certificate is unusable: {error}")
            }
            PinError::NotPinned(node_id) => write!(
                f,
                "peer {node_id} is not pinned (unknown, or its trust record was revoked); \
                 unknown peers are only allowed on the pairing channel"
            ),
        }
    }
}

impl std::error::Error for PinError {}

/// The shared pin logic of both verifiers.
#[derive(Debug)]
struct PinCheck {
    policy: TrustPolicy,
    algorithms: WebPkiSupportedAlgorithms,
}

impl PinCheck {
    fn new(policy: TrustPolicy, algorithms: WebPkiSupportedAlgorithms) -> Self {
        Self { policy, algorithms }
    }

    /// Derives the peer's `node_id` from the presented certificate and applies the
    /// policy.
    fn check(&self, end_entity: &CertificateDer<'_>) -> Result<NodeId, Error> {
        let node_id = node_id_from_certificate(end_entity.as_ref())
            .map_err(|error| refuse(PinError::MalformedCertificate(error)))?;
        if self.policy.allows(&node_id) {
            Ok(node_id)
        } else {
            Err(refuse(PinError::NotPinned(
                crate::spki::fingerprint_grouped(&node_id),
            )))
        }
    }
}

/// Wraps a [`PinError`] in the rustls error type, so a caller can still recognise it.
fn refuse(error: PinError) -> Error {
    Error::InvalidCertificate(CertificateError::Other(rustls::OtherError(Arc::new(error))))
}

/// Recognises a pin refusal inside a rustls error, for the pairing driver and the GUI.
pub fn pin_error(error: &Error) -> Option<&PinError> {
    match error {
        Error::InvalidCertificate(CertificateError::Other(other)) => {
            other.0.downcast_ref::<PinError>()
        }
        _ => None,
    }
}

/// `node_id` of the peer that completed a handshake, taken from its certificate.
///
/// The verifier has already accepted it (pin or pairing policy), so this is the identity
/// to compare against `PairHello.node_id` and to store in the trust file.
pub fn peer_node_id(certificates: &[CertificateDer<'_>]) -> Result<NodeId, PinError> {
    let end_entity = certificates.first().ok_or(PinError::MalformedCertificate(
        SpkiError::MalformedCertificate,
    ))?;
    node_id_from_certificate(end_entity.as_ref()).map_err(PinError::MalformedCertificate)
}

/// Client-side verifier: checks the **server** certificate against the pin.
#[derive(Debug)]
pub struct PinnedServerVerifier(PinCheck);

impl PinnedServerVerifier {
    /// A verifier for `policy`, using the `ring` provider's signature algorithms.
    pub fn new(policy: TrustPolicy) -> Self {
        Self(PinCheck::new(
            policy,
            crate::config::ring_provider().signature_verification_algorithms,
        ))
    }
}

impl ServerCertVerifier for PinnedServerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        // No intermediates, no name check, no validity window: the certificate is a
        // carrier for the pinned public key (see the module docs), and `ServerName` is
        // only what rustls needs to start a connection.
        self.0.check(end_entity)?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls12_signature(message, cert, dss, &self.0.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls13_signature(message, cert, dss, &self.0.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.algorithms.supported_schemes()
    }
}

/// Server-side verifier: checks the **client** certificate against the pin, and always
/// asks for it (mutual TLS is the point).
#[derive(Debug)]
pub struct PinnedClientVerifier(PinCheck);

impl PinnedClientVerifier {
    /// A verifier for `policy`, using the `ring` provider's signature algorithms.
    pub fn new(policy: TrustPolicy) -> Self {
        Self(PinCheck::new(
            policy,
            crate::config::ring_provider().signature_verification_algorithms,
        ))
    }
}

impl ClientCertVerifier for PinnedClientVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        // There is no CA to hint at; an empty list tells the client to present whatever
        // certificate it has, which is exactly the pinned model.
        &[]
    }

    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        // «Устройства проходят TLS с доказательством владения ключами» — both sides
        // always prove possession.
        true
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, Error> {
        self.0.check(end_entity)?;
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls12_signature(message, cert, dss, &self.0.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls13_signature(message, cert, dss, &self.0.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.algorithms.supported_schemes()
    }
}
