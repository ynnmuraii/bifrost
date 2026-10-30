//! Auxiliary key bindings use isolated identities and real, pinned TLS input connections.

use std::sync::Arc;
use std::time::Duration;

use identity::{IdentityPaths, NodeIdentity, TrustStore, TrustedPeer};
use protocol::{limits, wire};
use tokio::sync::mpsc;
use transport::{
    AuxPurpose, AuxiliaryDeriveError, Direction, Link, LinkConfig, LinkEvent, LinkReport, Stream,
};

fn identity_at(root: &std::path::Path, name: &str) -> NodeIdentity {
    NodeIdentity::load_or_create(&IdentityPaths::rooted_at(root.join(name)))
        .expect("identity")
        .0
}

fn trust(peer: transport::NodeId) -> TrustStore {
    let mut trust = TrustStore::new();
    trust.trust(TrustedPeer::paired(
        peer,
        "peer",
        None,
        identity::now_unix_ms(),
    ));
    trust
}

struct Pair {
    client: Link,
    server: Link,
    client_reports: mpsc::Receiver<LinkReport>,
    server_reports: mpsc::Receiver<LinkReport>,
    root: [u8; 32],
}

impl Drop for Pair {
    fn drop(&mut self) {
        self.client.handle.close();
        self.server.handle.close();
        self.client.task.abort();
        self.server.task.abort();
    }
}

async fn pair(client: &NodeIdentity, server: &NodeIdentity, alpn: &[u8]) -> Pair {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut server_config =
        identity::trusted_server_config(server, &trust(client.node_id())).unwrap();
    if !alpn.is_empty() {
        server_config.alpn_protocols = vec![alpn.to_vec()];
    }
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config));
    let accepting = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        acceptor.accept(tcp).await.unwrap()
    });
    let mut client_config =
        identity::trusted_client_config(client, &trust(server.node_id())).unwrap();
    client_config.alpn_protocols = if alpn.is_empty() {
        Vec::new()
    } else {
        vec![alpn.to_vec()]
    };
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));
    let tcp = tokio::net::TcpStream::connect(address).await.unwrap();
    let client_tls = connector
        .connect(identity::server_name(address.ip()), tcp)
        .await
        .unwrap();
    let server_tls = accepting.await.unwrap();
    let root = client_tls
        .get_ref()
        .1
        .export_keying_material([0; 32], b"EXPORTER-bifrost-aux-v1", Some(&[]))
        .unwrap();
    let remote_root = server_tls
        .get_ref()
        .1
        .export_keying_material([0; 32], b"EXPORTER-bifrost-aux-v1", Some(&[]))
        .unwrap();
    assert!(
        root == remote_root,
        "TLS roles must agree on the exact exporter context"
    );
    let hello = |identity: &NodeIdentity| wire::Hello {
        node_id: identity.node_id().as_bytes().to_vec(),
        ..Default::default()
    };
    let (client_tx, client_reports) = mpsc::channel(8);
    let (server_tx, server_reports) = mpsc::channel(8);
    let client_link = transport::link::spawn(LinkConfig {
        peer: server.node_id(),
        local: client.node_id(),
        direction: Direction::Outgoing,
        key: identity::random16().unwrap(),
        local_hello: hello(client),
        stream: Stream::Client(Box::new(client_tls)),
        reports: client_tx,
        queue_capacity: 8,
        liveness: transport::LinkLiveness::default(),
    })
    .unwrap();
    let server_link = transport::link::spawn(LinkConfig {
        peer: client.node_id(),
        local: server.node_id(),
        direction: Direction::Incoming,
        key: identity::random16().unwrap(),
        local_hello: hello(server),
        stream: Stream::Server(Box::new(server_tls)),
        reports: server_tx,
        queue_capacity: 8,
        liveness: transport::LinkLiveness::default(),
    })
    .unwrap();
    // current_thread tests cannot poll the spawned links before these assertions.
    assert!(client_link.handle.auxiliary_secret().is_none());
    assert!(server_link.handle.auxiliary_secret().is_none());
    Pair {
        client: client_link,
        server: server_link,
        client_reports,
        server_reports,
        root,
    }
}

async fn report(receiver: &mut mpsc::Receiver<LinkReport>) -> LinkReport {
    tokio::time::timeout(Duration::from_secs(10), receiver.recv())
        .await
        .unwrap()
        .unwrap()
}

fn up(report: LinkReport) -> LinkEvent {
    let LinkReport::Up {
        peer,
        key,
        hello,
        connection_id,
        version,
        remote,
        ..
    } = report
    else {
        panic!("expected established input link");
    };
    LinkEvent::Up {
        peer,
        generation: key,
        hello,
        connection_id,
        version,
        remote,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn real_tls_keys_are_bound_revocable_and_redacted() {
    let root = std::env::temp_dir().join(format!("bifrost-auxiliary-{}", std::process::id()));
    let client = identity_at(&root, "client");
    let server = identity_at(&root, "server");
    let mut first = pair(&client, &server, limits::ALPN_INPUT.as_bytes()).await;
    let client_up = up(report(&mut first.client_reports).await);
    let server_up = up(report(&mut first.server_reports).await);
    let client_secret = first.client.handle.auxiliary_secret().unwrap();
    let server_secret = first.server.handle.auxiliary_secret().unwrap();
    let first_binding = client_secret.binding();
    assert_eq!(first_binding.peer, server.node_id());
    assert_eq!(
        first_binding.connection_id,
        server_secret.binding().connection_id
    );
    let LinkEvent::Up {
        connection_id,
        generation,
        ..
    } = client_up
    else {
        unreachable!()
    };
    assert_eq!(connection_id, first_binding.connection_id);
    assert_eq!(generation, first_binding.generation);
    let mut nodes = [*client.node_id().as_bytes(), *server.node_id().as_bytes()];
    nodes.sort();
    let mut context = nodes.concat();
    context.extend_from_slice(&[7; 32]); // Per-session nonce.
    let key = client_secret
        .derive(AuxPurpose::AudioForwardKey, &context)
        .unwrap();
    let purposes = [
        AuxPurpose::AudioForwardKey,
        AuxPurpose::AudioReverseKey,
        AuxPurpose::AudioForwardNonce,
        AuxPurpose::AudioReverseNonce,
    ];
    let keys: Vec<_> = purposes
        .into_iter()
        .map(|purpose| {
            let local = client_secret.derive(purpose, &context).unwrap();
            let remote = server_secret.derive(purpose, &context).unwrap();
            assert_eq!(local.expose_secret(), remote.expose_secret());
            local
        })
        .collect();
    for (index, key) in keys.iter().enumerate() {
        for other in &keys[..index] {
            assert_ne!(key.expose_secret(), other.expose_secret());
        }
    }
    context.push(1);
    assert_ne!(
        key.expose_secret(),
        client_secret
            .derive(AuxPurpose::AudioForwardKey, &context)
            .unwrap()
            .expose_secret()
    );
    for len in [64, 256] {
        let boundary = vec![9; len];
        assert_eq!(
            client_secret
                .derive(AuxPurpose::AudioForwardKey, &boundary)
                .unwrap()
                .expose_secret(),
            server_secret
                .derive(AuxPurpose::AudioForwardKey, &boundary)
                .unwrap()
                .expose_secret()
        );
    }
    for len in [0, 63] {
        assert!(matches!(
            client_secret.derive(AuxPurpose::AudioForwardKey, &vec![9; len]),
            Err(AuxiliaryDeriveError::ContextTooShort)
        ));
    }
    assert!(matches!(
        client_secret.derive(AuxPurpose::AudioForwardKey, &[9; 257]),
        Err(AuxiliaryDeriveError::ContextTooLong)
    ));
    let debug = format!(
        "{client_up:?} {server_up:?} {:?} {client_secret:?}",
        first.client.handle
    );
    for secret in std::iter::once(&first.root).chain(keys.iter().map(|key| key.expose_secret())) {
        let hex = secret
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert!(!debug.contains(&hex));
        assert!(!debug.contains(&format!("{secret:?}")));
    }
    assert!(debug.contains("REDACTED"));
    first.client.handle.close();
    assert!(first.client.handle.auxiliary_secret().is_none());
    assert!(matches!(
        client_secret.derive(AuxPurpose::AudioForwardKey, &context),
        Err(AuxiliaryDeriveError::LinkClosed)
    ));
    assert!(matches!(
        report(&mut first.server_reports).await,
        LinkReport::Down { .. }
    ));
    assert!(matches!(
        server_secret.derive(AuxPurpose::AudioForwardKey, &context),
        Err(AuxiliaryDeriveError::LinkClosed)
    ));

    let mut second = pair(&client, &server, limits::ALPN_INPUT.as_bytes()).await;
    up(report(&mut second.client_reports).await);
    up(report(&mut second.server_reports).await);
    let second_secret = second.client.handle.auxiliary_secret().unwrap();
    assert!(first.root != second.root);
    assert_ne!(
        first_binding.connection_id,
        second_secret.binding().connection_id
    );
    assert_ne!(first_binding.generation, second_secret.binding().generation);
    assert_ne!(
        key.expose_secret(),
        second_secret
            .derive(AuxPurpose::AudioForwardKey, &context[..context.len() - 1],)
            .unwrap()
            .expose_secret()
    );
    second.client.task.abort();
    let _ = (&mut second.client.task).await;
    assert!(matches!(
        second_secret.derive(AuxPurpose::AudioForwardKey, &context),
        Err(AuxiliaryDeriveError::LinkClosed)
    ));
    drop(first);
    drop(second);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn auxiliary_export_is_restricted_to_runtime_input_alpn() {
    let root = std::env::temp_dir().join(format!("bifrost-auxiliary-alpn-{}", std::process::id()));
    let client = identity_at(&root, "client");
    let server = identity_at(&root, "server");
    for alpn in [limits::ALPN_CLIPBOARD, limits::ALPN_PAIR, ""] {
        let mut pair = pair(&client, &server, alpn.as_bytes()).await;
        assert!(matches!(
            report(&mut pair.client_reports).await,
            LinkReport::Up { .. }
        ));
        assert!(matches!(
            report(&mut pair.server_reports).await,
            LinkReport::Up { .. }
        ));
        assert!(pair.client.handle.auxiliary_secret().is_none());
        assert!(pair.server.handle.auxiliary_secret().is_none());
        pair.client.handle.close();
        pair.server.handle.close();
        pair.client.task.await.unwrap();
        pair.server.task.await.unwrap();
    }
    std::fs::remove_dir_all(root).unwrap();
}
