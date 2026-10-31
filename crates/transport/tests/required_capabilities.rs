//! Hello requirements and capability replacement are enforced over pinned input TLS.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use identity::{IdentityPaths, NodeIdentity, TrustStore, TrustedPeer};
use protocol::{limits, wire};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use transport::{Direction, Link, LinkConfig, LinkDownReason, LinkReport, Stream};

static COUNTER: AtomicU64 = AtomicU64::new(0);
const TIMEOUT: Duration = Duration::from_secs(10);
const UNKNOWN_CAPABILITY: i32 = 10_001;

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

struct Session {
    link: Link,
    peer: Stream,
    reports: mpsc::Receiver<LinkReport>,
    connection_id: Vec<u8>,
    root: std::path::PathBuf,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.link.handle.close();
        self.link.task.abort();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

impl Session {
    async fn new(direction: Direction, mut hello: wire::Hello) -> (Self, wire::Hello) {
        let root = std::env::temp_dir().join(format!(
            "bifrost-required-capabilities-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let identity = |name: &str| {
            NodeIdentity::load_or_create(&IdentityPaths::rooted_at(root.join(name)))
                .unwrap()
                .0
        };
        let local = identity("local");
        let remote = identity("remote");
        let (client, server) = match direction {
            Direction::Outgoing => (&local, &remote),
            Direction::Incoming => (&remote, &local),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let mut server_config =
            identity::trusted_server_config(server, &trust(client.node_id())).unwrap();
        server_config.alpn_protocols = vec![limits::ALPN_INPUT.as_bytes().to_vec()];
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config));
        let accepting = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            acceptor.accept(tcp).await.unwrap()
        });
        let mut client_config =
            identity::trusted_client_config(client, &trust(server.node_id())).unwrap();
        client_config.alpn_protocols = vec![limits::ALPN_INPUT.as_bytes().to_vec()];
        let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));
        let tcp = tokio::net::TcpStream::connect(address).await.unwrap();
        let client_tls = connector
            .connect(identity::server_name(address.ip()), tcp)
            .await
            .unwrap();
        let server_tls = accepting.await.unwrap();
        let (stream, peer) = match direction {
            Direction::Outgoing => (
                Stream::Client(Box::new(client_tls)),
                Stream::Server(Box::new(server_tls)),
            ),
            Direction::Incoming => (
                Stream::Server(Box::new(server_tls)),
                Stream::Client(Box::new(client_tls)),
            ),
        };
        hello.node_id = local.node_id().as_bytes().to_vec();
        let remote_hello = wire::Hello {
            node_id: remote.node_id().as_bytes().to_vec(),
            can_own: true,
            ..Default::default()
        };
        let (reports, receiver) = mpsc::channel(8);
        let link = transport::link::spawn(LinkConfig {
            peer: remote.node_id(),
            local: local.node_id(),
            direction,
            key: identity::random16().unwrap(),
            local_hello: hello,
            stream,
            reports,
            queue_capacity: 8,
            liveness: transport::LinkLiveness::default(),
        })
        .unwrap();
        assert!(link.handle.auxiliary_secret().is_none());
        let mut session = Self {
            link,
            peer,
            reports: receiver,
            connection_id: vec![7; 16],
            root,
        };
        if matches!(direction, Direction::Outgoing) {
            let envelope = session.frame().await;
            assert_eq!(envelope.seq, 1);
            assert!(matches!(
                envelope.payload,
                Some(wire::envelope::Payload::Hello(_))
            ));
            session.connection_id = envelope.connection_id;
        }
        (session, remote_hello)
    }

    async fn send(&mut self, seq: u64, payload: wire::envelope::Payload) {
        let envelope = wire::Envelope {
            major: limits::PROTOCOL_MAJOR,
            minor: 4, // The compatible pre-requirements 0.4 wire shape.
            connection_id: self.connection_id.clone(),
            seq,
            payload: Some(payload),
            ..Default::default()
        };
        tokio::time::timeout(TIMEOUT, transport::write_frame(&mut self.peer, &envelope))
            .await
            .unwrap()
            .unwrap();
    }

    async fn frame(&mut self) -> wire::Envelope {
        tokio::time::timeout(TIMEOUT, transport::read_frame(&mut self.peer))
            .await
            .unwrap()
            .unwrap()
    }

    async fn report(&mut self) -> LinkReport {
        tokio::time::timeout(TIMEOUT, self.reports.recv())
            .await
            .unwrap()
            .expect("link report")
    }

    async fn establish(&mut self, direction: Direction, hello: wire::Hello) {
        self.send(1, wire::envelope::Payload::Hello(hello)).await;
        if matches!(direction, Direction::Incoming) {
            let envelope = self.frame().await;
            assert_eq!(envelope.seq, 1);
            assert_eq!(envelope.connection_id, self.connection_id);
            assert!(matches!(
                envelope.payload,
                Some(wire::envelope::Payload::Hello(_))
            ));
        }
        assert!(matches!(self.report().await, LinkReport::Up { .. }));
        assert!(self.link.handle.auxiliary_secret().is_some());
    }

    async fn refused(&mut self, code: wire::ErrorCode, related_seq: u64) {
        let envelope = tokio::time::timeout(TIMEOUT, async {
            loop {
                let envelope = transport::read_frame(&mut self.peer).await.unwrap();
                // Whole frames already in flight cannot be preempted by the urgent refusal.
                if !matches!(envelope.payload, Some(wire::envelope::Payload::Ping(_))) {
                    break envelope;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(envelope.connection_id, self.connection_id);
        let Some(wire::envelope::Payload::Error(error)) = envelope.payload else {
            panic!("expected wire refusal, got {envelope:?}");
        };
        assert_eq!(error.code, code as i32);
        assert_eq!(error.related_seq, related_seq);
        assert!(matches!(
            self.frame().await.payload,
            Some(wire::envelope::Payload::Shutdown(_))
        ));
        // A raw TLS read distinguishes close_notify from an unclean socket EOF/reset.
        let mut byte = [0];
        assert_eq!(
            tokio::time::timeout(TIMEOUT, self.peer.read(&mut byte))
                .await
                .unwrap()
                .expect("TLS close_notify, not reset"),
            0
        );
        self.peer.shutdown().await.unwrap();
        let expected = if code == wire::ErrorCode::UnsupportedVersion {
            LinkDownReason::Incompatible
        } else {
            LinkDownReason::ProtocolRefused(code)
        };
        match self.report().await {
            LinkReport::Down { reason, .. } => assert_eq!(reason, expected),
            report => panic!("refused link must not publish Up or a message: {report:?}"),
        }
        assert!(self.link.handle.auxiliary_secret().is_none());
        tokio::time::timeout(TIMEOUT, &mut self.link.task)
            .await
            .unwrap()
            .unwrap();
        assert!(
            self.reports.recv().await.is_none(),
            "no extra Up after refusal"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn local_requirement_refuses_legacy_hello_on_acceptor_and_dialer() {
    for direction in [Direction::Incoming, Direction::Outgoing] {
        let capability = wire::Capability::Capture as i32;
        let (mut session, remote) = Session::new(
            direction,
            wire::Hello {
                capabilities: vec![capability],
                required_capabilities: vec![capability],
                ..Default::default()
            },
        )
        .await;
        // A normal 0.4 Hello has neither offers nor requirements for the new contract.
        session
            .send(1, wire::envelope::Payload::Hello(remote))
            .await;
        session
            .refused(wire::ErrorCode::UnsupportedVersion, 1)
            .await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn unknown_remote_requirement_is_refused_even_when_remote_offers_it() {
    for direction in [Direction::Incoming, Direction::Outgoing] {
        let (mut session, mut remote) = Session::new(direction, wire::Hello::default()).await;
        remote.capabilities = vec![UNKNOWN_CAPABILITY];
        remote.required_capabilities = vec![UNKNOWN_CAPABILITY];
        session
            .send(1, wire::envelope::Payload::Hello(remote))
            .await;
        session
            .refused(wire::ErrorCode::UnsupportedVersion, 1)
            .await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn normal_legacy_peers_still_establish_in_both_tls_roles() {
    for direction in [Direction::Incoming, Direction::Outgoing] {
        let (mut session, remote) = Session::new(direction, wire::Hello::default()).await;
        session.establish(direction, remote).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn implemented_requirements_use_static_support_not_runtime_offers() {
    for direction in [Direction::Incoming, Direction::Outgoing] {
        for capability in [
            wire::Capability::Capture as i32,
            wire::Capability::Inject as i32,
            wire::Capability::AudioControlV1 as i32,
            wire::Capability::SystemAudioSource as i32,
            wire::Capability::AudioFocusReportV1 as i32,
            wire::Capability::AudioListener as i32,
            wire::Capability::MicControlV1 as i32,
            wire::Capability::MediaControlV1 as i32,
        ] {
            for available in [false, true] {
                let (mut session, mut remote) = Session::new(
                    direction,
                    wire::Hello {
                        capabilities: if available { vec![capability] } else { vec![] },
                        ..Default::default()
                    },
                )
                .await;
                remote.capabilities = vec![capability];
                remote.required_capabilities = vec![capability];
                session.establish(direction, remote).await;
            }
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn oversized_capability_replacement_is_refused_over_tls() {
    let direction = Direction::Incoming;
    let (mut session, remote) = Session::new(direction, wire::Hello::default()).await;
    session.establish(direction, remote).await;
    session
        .send(
            2,
            wire::envelope::Payload::CapabilitiesChanged(wire::CapabilitiesChanged {
                capabilities: vec![UNKNOWN_CAPABILITY; 65],
            }),
        )
        .await;
    session.refused(wire::ErrorCode::InvalidFrame, 2).await;
}

#[tokio::test(flavor = "current_thread")]
async fn replacement_cannot_withdraw_either_hello_requirement() {
    let capability = wire::Capability::Capture as i32;
    for direction in [Direction::Incoming, Direction::Outgoing] {
        for local_requires in [false, true] {
            let (mut session, mut remote) = Session::new(
                direction,
                wire::Hello {
                    capabilities: vec![capability],
                    required_capabilities: if local_requires {
                        vec![capability]
                    } else {
                        vec![]
                    },
                    ..Default::default()
                },
            )
            .await;
            remote.capabilities = vec![capability];
            if !local_requires {
                remote.required_capabilities = vec![capability];
            }
            session.establish(direction, remote).await;
            session
                .send(
                    2,
                    wire::envelope::Payload::CapabilitiesChanged(wire::CapabilitiesChanged {
                        capabilities: vec![UNKNOWN_CAPABILITY],
                    }),
                )
                .await;
            session.refused(wire::ErrorCode::InvalidFrame, 2).await;
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn bounded_replacement_preserving_both_requirements_accepts_unknown_optional_values() {
    let capture = wire::Capability::Capture as i32;
    let inject = wire::Capability::Inject as i32;
    for direction in [Direction::Incoming, Direction::Outgoing] {
        let (mut session, mut remote) = Session::new(
            direction,
            wire::Hello {
                capabilities: vec![capture, inject],
                required_capabilities: vec![capture],
                ..Default::default()
            },
        )
        .await;
        remote.capabilities = vec![capture, inject, UNKNOWN_CAPABILITY];
        remote.required_capabilities = vec![inject];
        session.establish(direction, remote).await;
        let mut bounded = vec![UNKNOWN_CAPABILITY; 64];
        bounded[0] = capture;
        bounded[1] = inject;
        for (seq, capabilities) in [
            (2, bounded),
            (3, vec![capture, inject, UNKNOWN_CAPABILITY + 1]),
        ] {
            session
                .send(
                    seq,
                    wire::envelope::Payload::CapabilitiesChanged(wire::CapabilitiesChanged {
                        capabilities: capabilities.clone(),
                    }),
                )
                .await;
            match session.report().await {
                LinkReport::Message {
                    message: transport::convert::Inbound::CapabilitiesChanged(actual),
                    ..
                } => assert_eq!(actual, capabilities),
                report => {
                    panic!("valid replacement must remain on the established link: {report:?}")
                }
            }
            assert!(session.link.handle.auxiliary_secret().is_some());
        }
    }
}
