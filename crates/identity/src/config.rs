//! rustls configurations: TLS 1.3 only, `ring` backend, ALPN per channel.
//!
//! Constants come from `crates/protocol` (`protocol::limits`), which holds the ALPN
//! strings and the frame limits in one place, so this crate cannot drift from the wire
//! contract:
//!
//! * `ALPN_INPUT = "lan-workspace/input/1"`
//! * `ALPN_CLIPBOARD = "lan-workspace/clipboard/1"`
//! * `ALPN_PAIR = "lan-workspace/pair/1"`
//!
//! # Why `ring`
//!
//! The `aws-lc-rs` backend needs a C toolchain (cmake/nasm) on Windows; `ring` ships
//! pre-built assembly for the targets we build on, so the workspace stays a pure
//! `cargo build`. TLS 1.3 only, because `docs/05-protocol.md` specifies TLS 1.3 and
//! there is no reason to keep the older version reachable.
//!
//! # The policy/ALPN invariant
//!
//! [`TrustPolicy::Pairing`] accepts unknown peers, so it may only ever be used on the
//! pairing channel; conversely the pairing channel exists to meet unknown peers. The
//! builders enforce both directions, which is what makes «Пока ключ неизвестен, allowlist
//! разрешает только `PairHello` и статус» true by construction: an unknown peer cannot
//! reach the input ALPN, and the input channel's verifier refuses it outright.

use std::sync::Arc;

use rustls::crypto::CryptoProvider;
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ServerConfig};

use crate::key::{IdentityError, NodeIdentity};
use crate::trust::TrustStore;
use crate::verify::{PinnedClientVerifier, PinnedServerVerifier, TrustPolicy};

/// The `ring` crypto provider (signature verification schemes included).
pub fn ring_provider() -> CryptoProvider {
    rustls::crypto::ring::default_provider()
}

/// The channels a peer-to-peer link may use.
pub const LINK_ALPN: [&str; 2] = [
    protocol::limits::ALPN_INPUT,
    protocol::limits::ALPN_CLIPBOARD,
];

/// A client configuration that pins the server by `node_id`.
pub fn client_config(
    identity: &NodeIdentity,
    policy: TrustPolicy,
    alpn: &[&str],
) -> Result<ClientConfig, IdentityError> {
    check_policy_alpn(&policy, alpn)?;
    let config = ClientConfig::builder_with_provider(Arc::new(ring_provider()))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|error| IdentityError::Tls(error.to_string()))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedServerVerifier::new(policy)))
        .with_client_auth_cert(vec![identity.certificate()], identity.private_key())
        .map_err(|error| IdentityError::Tls(error.to_string()))?;
    let mut config = config;
    config.alpn_protocols = alpn.iter().map(|name| name.as_bytes().to_vec()).collect();
    Ok(config)
}

/// A server configuration that pins the client by `node_id` and always asks for its
/// certificate.
pub fn server_config(
    identity: &NodeIdentity,
    policy: TrustPolicy,
    alpn: &[&str],
) -> Result<ServerConfig, IdentityError> {
    check_policy_alpn(&policy, alpn)?;
    let config = ServerConfig::builder_with_provider(Arc::new(ring_provider()))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|error| IdentityError::Tls(error.to_string()))?
        .with_client_cert_verifier(Arc::new(PinnedClientVerifier::new(policy)))
        .with_single_cert(vec![identity.certificate()], identity.private_key())
        .map_err(|error| IdentityError::Tls(error.to_string()))?;
    let mut config = config;
    config.alpn_protocols = alpn.iter().map(|name| name.as_bytes().to_vec()).collect();
    Ok(config)
}

/// Client config for the input/control and clipboard channels, pinned to the active
/// trust records.
pub fn trusted_client_config(
    identity: &NodeIdentity,
    trust: &TrustStore,
) -> Result<ClientConfig, IdentityError> {
    client_config(identity, TrustPolicy::Pinned(trust.pinned()), &LINK_ALPN)
}

/// Server config for the input/control and clipboard channels, pinned to the active
/// trust records: a peer that was revoked (or never paired) is refused during the
/// handshake, before any `Hello` can be exchanged.
pub fn trusted_server_config(
    identity: &NodeIdentity,
    trust: &TrustStore,
) -> Result<ServerConfig, IdentityError> {
    server_config(identity, TrustPolicy::Pinned(trust.pinned()), &LINK_ALPN)
}

/// Client config for the pairing channel: the server is unknown by definition, and the
/// SAS code is what authenticates it.
pub fn pairing_client_config(identity: &NodeIdentity) -> Result<ClientConfig, IdentityError> {
    client_config(
        identity,
        TrustPolicy::Pairing,
        &[protocol::limits::ALPN_PAIR],
    )
}

/// Server config for the pairing channel. Bind the listener only while the pairing window
/// is open (`docs/05-protocol.md`: «Пользователь открывает окно сопряжения на
/// узле-сервере на 120 s»).
pub fn pairing_server_config(identity: &NodeIdentity) -> Result<ServerConfig, IdentityError> {
    server_config(
        identity,
        TrustPolicy::Pairing,
        &[protocol::limits::ALPN_PAIR],
    )
}

/// The `ServerName` to dial a peer with.
///
/// Identity is the pin, not the name, so an IP address is used as-is and never resolved:
/// «mDNS/IP — маршрутизация, не идентичность» (ADR-007).
pub fn server_name(address: std::net::IpAddr) -> ServerName<'static> {
    ServerName::IpAddress(address.into())
}

fn check_policy_alpn(policy: &TrustPolicy, alpn: &[&str]) -> Result<(), IdentityError> {
    let uses_pairing_alpn = alpn.contains(&protocol::limits::ALPN_PAIR);
    match policy {
        TrustPolicy::Pairing if !uses_pairing_alpn => Err(IdentityError::Tls(
            "the pairing trust policy may only be used on lan-workspace/pair/1".into(),
        )),
        TrustPolicy::Pinned(_) if uses_pairing_alpn => Err(IdentityError::Tls(
            "the pairing channel needs the pairing trust policy: its peer is unknown by definition"
                .into(),
        )),
        _ => Ok(()),
    }
}
