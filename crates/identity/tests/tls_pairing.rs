//! Loopback pairing with the real TLS stack: one ceremony end to end, the pin on the
//! input channel, revoke, and the policy/ALPN invariant.
//!
//! Everything runs against `127.0.0.1` with identities and trust stores in temp
//! directories, so no user file is ever touched.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use geometry::NodeId;
use identity::{
    accept_one, bind, client_config, dial, open_window, pairing_client_config, server_config,
    trusted_client_config, trusted_server_config, IdentityPaths, NodeIdentity, TrustPolicy,
    TrustStore, LINK_ALPN,
};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::{TlsAcceptor, TlsConnector};

static COUNTER: AtomicU32 = AtomicU32::new(0);

const DIAL_TIMEOUT: Duration = Duration::from_secs(20);

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "lws-identity-tls-{}-{}-{label}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temporary directory");
    dir
}

fn identity_at(root: &Path, name: &str) -> NodeIdentity {
    NodeIdentity::load_or_create(&IdentityPaths::rooted_at(root.join(name)))
        .expect("identity")
        .0
}

/// Attempts the input handshake and reports whether the peer refused us.
///
/// TLS 1.3 verifies the client certificate on the *server* side, so a refused client may
/// see either an immediate handshake error (the alert arrived in time) or a connection
/// that completes and is then torn down. Both are a refusal; what must never happen is
/// application data arriving.
async fn refused(
    address: std::net::SocketAddr,
    identity: &NodeIdentity,
    trust: &TrustStore,
) -> bool {
    use tokio::io::AsyncReadExt;

    let config = trusted_client_config(identity, trust).expect("client config");
    let connector = TlsConnector::from(Arc::new(config));
    let Ok(tcp) = TcpStream::connect(address).await else {
        return true;
    };
    let Ok(mut stream) = connector
        .connect(identity::server_name(address.ip()), tcp)
        .await
    else {
        return true;
    };

    // The stream exists, so the proof is that the peer hangs up without data.
    let mut byte = [0u8; 1];
    match tokio::time::timeout(Duration::from_secs(10), stream.read(&mut byte)).await {
        Ok(Ok(0)) | Ok(Err(_)) => true,
        Ok(Ok(_)) => false,
        Err(_) => false,
    }
}

async fn dial_input(
    address: std::net::SocketAddr,
    identity: &NodeIdentity,
    trust: &TrustStore,
) -> Result<(), std::io::Error> {
    let config = trusted_client_config(identity, trust).expect("client config");
    let connector = TlsConnector::from(Arc::new(config));
    let tcp = TcpStream::connect(address).await?;
    connector
        .connect(identity::server_name(address.ip()), tcp)
        .await
        .map(|_| ())
}

async fn accept_input_once(
    listener: &TcpListener,
    identity: &NodeIdentity,
    trust: &TrustStore,
) -> Result<(), std::io::Error> {
    let acceptor = TlsAcceptor::from(Arc::new(
        trusted_server_config(identity, trust).expect("server config"),
    ));
    let (tcp, _) = listener.accept().await?;
    acceptor.accept(tcp).await.map(|_| ())
}

#[tokio::test(flavor = "current_thread")]
async fn a_loopback_pairing_commits_on_both_sides_with_one_code() {
    let root = temp_dir("pair");
    let a = Arc::new(identity_at(&root, "node-a"));
    let b = identity_at(&root, "node-b");

    let listener = bind("127.0.0.1:0".parse().expect("address"))
        .await
        .expect("bind");
    let address = listener.local_addr().expect("local address");

    let opener_identity = Arc::clone(&a);
    let opener = tokio::spawn(async move {
        let mut window = open_window(0).expect("window");
        accept_one(&listener, &opener_identity, &mut window, 0, 47901)
            .await
            .expect("the opener's half of the ceremony")
    });

    let dialer = dial(address, &b, DIAL_TIMEOUT, 47901)
        .await
        .expect("the dialer's half of the ceremony");
    let opener = opener.await.expect("the opener task");

    // Both sides derive the code from their own exporter, and they must agree.
    assert_eq!(opener.sas(), dialer.sas());
    assert_eq!(opener.peer_node_id(), &b.node_id());
    assert_eq!(dialer.peer_node_id(), &a.node_id());
    assert_eq!(
        opener.peer_fingerprint(),
        identity::fingerprint_grouped(&b.node_id())
    );
    assert_eq!(opener.state(), identity::PairingState::Comparing);

    // Both users approve; the decision exchange needs both halves at once.
    let (opener_paired, dialer_paired) = tokio::join!(opener.approve(), dialer.approve());
    let opener_paired = opener_paired.expect("the opener commits");
    let dialer_paired = dialer_paired.expect("the dialer commits");

    assert_eq!(opener_paired.node_id, b.node_id());
    assert_eq!(dialer_paired.node_id, a.node_id());
    assert_eq!(opener_paired.sas, dialer_paired.sas);
    assert_eq!(
        opener_paired.fingerprint,
        identity::fingerprint(&b.node_id())
    );

    // The record the caller persists makes the peer pinnable.
    let mut trust_a = TrustStore::new();
    trust_a.trust(opener_paired.trusted_peer(identity::now_unix_ms(), Some(address.to_string())));
    let mut trust_b = TrustStore::new();
    trust_b.trust(dialer_paired.trusted_peer(identity::now_unix_ms(), None));

    assert!(trust_a.is_trusted(&b.node_id()));
    assert!(trust_b.is_trusted(&a.node_id()));
    assert_eq!(trust_a.pinned(), [b.node_id()].into_iter().collect());

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "current_thread")]
async fn an_unpinned_peer_cannot_finish_the_input_handshake() {
    let root = temp_dir("unknown");
    let a = Arc::new(identity_at(&root, "node-a"));
    let b = identity_at(&root, "node-b");
    let c = identity_at(&root, "node-c");

    // `a` knows only `b`.
    let mut trust_a = TrustStore::new();
    trust_a.trust(identity::TrustedPeer::paired(
        b.node_id(),
        "node-b",
        None,
        1,
    ));
    let mut trust_b = TrustStore::new();
    trust_b.trust(identity::TrustedPeer::paired(
        a.node_id(),
        "node-a",
        None,
        1,
    ));

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let server_identity = Arc::clone(&a);

    // The unknown peer is refused by the pin, before any application data.
    let server = {
        let listener = listener;
        let trust = trust_a.clone();
        tokio::spawn(async move { accept_input_once(&listener, &server_identity, &trust).await })
    };
    assert!(
        refused(address, &c, &TrustStore::new()).await,
        "an unknown peer must not get a usable connection"
    );
    assert!(
        server.await.expect("server task").is_err(),
        "the pinned server must refuse the handshake"
    );

    // The pinned peer does complete it, which proves the refusal was about the pin.
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let server = {
        let listener = listener;
        let identity = Arc::clone(&a);
        let trust = trust_a.clone();
        tokio::spawn(async move { accept_input_once(&listener, &identity, &trust).await })
    };
    dial_input(address, &b, &trust_b)
        .await
        .expect("a pinned peer must be accepted");
    server
        .await
        .expect("server task")
        .expect("the server side must also succeed");

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "current_thread")]
async fn revoking_a_peer_makes_the_next_handshake_fail() {
    let root = temp_dir("revoke");
    let a = Arc::new(identity_at(&root, "node-a"));
    let b = identity_at(&root, "node-b");

    let mut trust_a = TrustStore::new();
    trust_a.trust(identity::TrustedPeer::paired(
        b.node_id(),
        "node-b",
        None,
        1,
    ));
    let mut trust_b = TrustStore::new();
    trust_b.trust(identity::TrustedPeer::paired(
        a.node_id(),
        "node-a",
        None,
        1,
    ));

    // First handshake: accepted.
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let server = {
        let listener = listener;
        let identity = Arc::clone(&a);
        let trust = trust_a.clone();
        tokio::spawn(async move { accept_input_once(&listener, &identity, &trust).await })
    };
    dial_input(address, &b, &trust_b).await.expect("accepted");
    server.await.expect("server task").expect("server side");

    // «Отзыв доверия ... запрещает reconnect»: the pin loses the peer, so the next
    // handshake cannot get past the verifier.
    assert!(trust_a.revoke(&b.node_id(), 2));
    assert!(trust_a.pinned().is_empty());

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let server = {
        let listener = listener;
        let identity = Arc::clone(&a);
        let trust = trust_a.clone();
        tokio::spawn(async move { accept_input_once(&listener, &identity, &trust).await })
    };
    assert!(
        refused(address, &b, &trust_b).await,
        "a revoked peer must not get a usable connection"
    );
    assert!(
        server.await.expect("server task").is_err(),
        "the server must refuse a revoked peer"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "current_thread")]
async fn two_independent_sessions_do_not_agree_on_a_code() {
    let root = temp_dir("sessions");
    let a = Arc::new(identity_at(&root, "node-a"));
    let b = identity_at(&root, "node-b");

    let mut codes = Vec::new();
    for round in 0..2u64 {
        let listener = bind("127.0.0.1:0".parse().expect("address"))
            .await
            .expect("bind");
        let address = listener.local_addr().expect("address");
        let opener_identity = Arc::clone(&a);
        let opener = tokio::spawn(async move {
            let mut window = open_window(round * 1_000).expect("window");
            accept_one(
                &listener,
                &opener_identity,
                &mut window,
                round * 1_000,
                47901,
            )
            .await
            .expect("opener")
        });
        let dialer = dial(address, &b, DIAL_TIMEOUT, 47901)
            .await
            .expect("dialer");
        let opener = opener.await.expect("task");
        assert_eq!(
            opener.sas(),
            dialer.sas(),
            "within one session the codes agree"
        );
        codes.push(dialer.sas());
    }

    // Different sessions (different exporter contexts) must not produce the same code;
    // a MITM would have to make two independent sessions collide.
    assert_ne!(codes[0], codes[1]);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_pairing_policy_and_the_pairing_channel_require_each_other() {
    let root = temp_dir("policy");
    let a = identity_at(&root, "node-a");
    let pair_alpn = [protocol::limits::ALPN_PAIR];

    // An unknown peer may only ever appear on the pairing channel.
    assert!(
        client_config(&a, TrustPolicy::Pairing, &LINK_ALPN).is_err(),
        "the pairing policy must not be usable on the input/clipboard channels"
    );
    assert!(
        server_config(&a, TrustPolicy::Pairing, &LINK_ALPN).is_err(),
        "and not on a server that speaks the runtime channels either"
    );

    // Conversely the pairing channel exists to meet an unknown peer, so a pinned policy
    // there would be a contradiction.
    let empty: BTreeSet<NodeId> = BTreeSet::new();
    assert!(client_config(&a, TrustPolicy::Pinned(empty.clone()), &pair_alpn).is_err());
    assert!(server_config(&a, TrustPolicy::Pinned(empty), &pair_alpn).is_err());

    // The two correct combinations are accepted.
    assert!(client_config(&a, TrustPolicy::Pairing, &pair_alpn).is_ok());
    assert!(pairing_client_config(&a).is_ok());
    assert!(identity::pairing_server_config(&a).is_ok());

    // A node that has paired with nobody can still build a runtime configuration; it will
    // simply refuse every peer at the handshake.
    assert!(trusted_client_config(&a, &TrustStore::new()).is_ok());
    assert!(trusted_server_config(&a, &TrustStore::new()).is_ok());

    let _ = std::fs::remove_dir_all(&root);
}
