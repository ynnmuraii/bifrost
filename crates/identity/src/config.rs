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

/// Client config for the **clipboard channel only**: ALPN
/// `protocol::limits::ALPN_CLIPBOARD`, pinned to the active trust records.
///
/// The clipboard channel is its own TLS session on port `47902`
/// (`docs/05-protocol.md`), so its handshake negotiates only
/// `lan-workspace/clipboard/1`. A peer with no active trust record is refused during the
/// handshake, before any `ClipboardBind` is read; a peer that answered on another channel
/// cannot reach this one.
pub fn clipboard_client_config(
    identity: &NodeIdentity,
    trust: &TrustStore,
) -> Result<ClientConfig, IdentityError> {
    client_config(
        identity,
        TrustPolicy::Pinned(trust.pinned()),
        &[protocol::limits::ALPN_CLIPBOARD],
    )
}

/// Server config for the **clipboard channel only**. See [`clipboard_client_config`].
pub fn clipboard_server_config(
    identity: &NodeIdentity,
    trust: &TrustStore,
) -> Result<ServerConfig, IdentityError> {
    server_config(
        identity,
        TrustPolicy::Pinned(trust.pinned()),
        &[protocol::limits::ALPN_CLIPBOARD],
    )
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::key::IdentityPaths;
    use crate::trust::TrustedPeer;

    use tokio::net::{TcpListener, TcpStream};
    use tokio_rustls::{TlsAcceptor, TlsConnector};

    fn temp_dir(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lws-identity-config-{}-{label}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temporary directory");
        dir
    }

    fn identity_at(root: &std::path::Path, name: &str) -> NodeIdentity {
        NodeIdentity::load_or_create(&IdentityPaths::rooted_at(root.join(name)))
            .expect("identity")
            .0
    }

    /// A pinned trust store for the two identities that have just met.
    fn paired(a: &NodeIdentity, b: &NodeIdentity) -> (TrustStore, TrustStore) {
        let mut a_trust = TrustStore::new();
        a_trust.trust(TrustedPeer::paired(b.node_id(), "peer", None, 0));
        let mut b_trust = TrustStore::new();
        b_trust.trust(TrustedPeer::paired(a.node_id(), "peer", None, 0));
        (a_trust, b_trust)
    }

    /// The clipboard channel's handshake negotiates `lan-workspace/clipboard/1` on both
    /// sides: the two builders offer that ALPN and nothing else, and rustls selects it.
    #[tokio::test(flavor = "current_thread")]
    async fn a_clipboard_handshake_negotiates_the_clipboard_alpn() {
        let root = temp_dir("clipboard-alpn");
        let server = identity_at(&root, "server");
        let client = identity_at(&root, "client");
        let (server_trust, client_trust) = paired(&server, &client);

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let acceptor = TlsAcceptor::from(Arc::new(
            clipboard_server_config(&server, &server_trust).expect("server config"),
        ));

        let accept = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.expect("accept");
            let stream = acceptor.accept(tcp).await.expect("handshake");
            stream.get_ref().1.alpn_protocol().map(<[u8]>::to_vec)
        });

        let connector = TlsConnector::from(Arc::new(
            clipboard_client_config(&client, &client_trust).expect("client config"),
        ));
        let tcp = TcpStream::connect(address).await.expect("connect");
        let stream = connector
            .connect(server_name(address.ip()), tcp)
            .await
            .expect("handshake");
        let client_alpn = stream.get_ref().1.alpn_protocol().map(<[u8]>::to_vec);
        let server_alpn = accept.await.expect("accept task");

        assert_eq!(
            client_alpn.as_deref(),
            Some(protocol::limits::ALPN_CLIPBOARD.as_bytes()),
            "the dialer must negotiate the clipboard ALPN"
        );
        assert_eq!(
            server_alpn.as_deref(),
            Some(protocol::limits::ALPN_CLIPBOARD.as_bytes()),
            "the acceptor must negotiate the clipboard ALPN"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// An input-only server refuses a client that offers only the clipboard ALPN: rustls
    /// sends `no_application_protocol` when the server has ALPN configured and nothing
    /// overlaps, so a clipboard session cannot masquerade as an input link.
    #[tokio::test(flavor = "current_thread")]
    async fn an_input_only_server_refuses_a_clipboard_only_client() {
        let root = temp_dir("clipboard-vs-input");
        let server = identity_at(&root, "server");
        let client = identity_at(&root, "client");
        let (server_trust, client_trust) = paired(&server, &client);

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let acceptor = TlsAcceptor::from(Arc::new(
            server_config(
                &server,
                TrustPolicy::Pinned(server_trust.pinned()),
                &[protocol::limits::ALPN_INPUT],
            )
            .expect("input-only server config"),
        ));

        let accept = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.expect("accept");
            acceptor.accept(tcp).await.map(|_| ())
        });

        let connector = TlsConnector::from(Arc::new(
            clipboard_client_config(&client, &client_trust).expect("client config"),
        ));
        let tcp = TcpStream::connect(address).await.expect("connect");
        let client_result = connector.connect(server_name(address.ip()), tcp).await;

        let server_result = accept.await.expect("accept task");
        assert!(
            server_result.is_err(),
            "an input-only server must refuse a clipboard-only client"
        );
        // The client may observe the refusal as a handshake error, or the handshake may
        // finish just before the server's `no_application_protocol` alert arrives; in that
        // case the proof of refusal is a stream that never delivers data.
        if let Ok(mut stream) = client_result {
            use tokio::io::AsyncReadExt;
            let mut byte = [0u8; 1];
            let read =
                tokio::time::timeout(std::time::Duration::from_secs(5), stream.read(&mut byte))
                    .await;
            assert!(
                matches!(read, Ok(Ok(0)) | Ok(Err(_))),
                "a refused clipboard client must not be able to exchange data"
            );
        }

        let _ = std::fs::remove_dir_all(&root);
    }
}
