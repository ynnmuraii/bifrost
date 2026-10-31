//! Real Dry Listener and paired-root TLS Source with a deliberately silent UDP endpoint.
//! No firewall, default roots, audio devices, synthetic worker ACKs or model-only activation.

#![cfg(feature = "audio")]

mod common;

use common::audio::{self, Window, TIMEOUT};
use common::Unpaired;
use identity::{IdentityPaths, NodeIdentity, TrustStore};
use protocol::{ipc as gui, limits, wire};
use std::net::{SocketAddr, UdpSocket};
use std::path::Path;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc,
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use transport::{convert::Inbound, Direction, LinkConfig, LinkReport, Stream};

struct RawPeer {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    observations: mpsc::Receiver<Observation>,
    port: u16,
}

enum Observation {
    Up,
    Offer(wire::AudioOffer),
    Commit(wire::AudioCommit),
    Datagram(SocketAddr),
    Stopped(wire::AudioStop),
    Alive,
}

impl RawPeer {
    fn start(root: &Path, listener_port: u16, listener_id: Vec<u8>, group: Vec<u8>) -> Self {
        let paths = config::Paths::new(
            root.join("config"),
            root.join("data"),
            root.join("state"),
            Some(root.join("run")),
        );
        let identity_paths = IdentityPaths {
            data_dir: paths.data_dir().to_path_buf(),
            key_file: paths.identity_file(),
            trust_file: paths.trust_file(),
        };
        let (identity, loaded) = NodeIdentity::load_or_create(&identity_paths)
            .expect("reuse only the stopped fixture Source identity");
        assert_eq!(loaded, identity::IdentityLoad::Loaded);
        let identity::atomic::LoadResult::Loaded(trust) =
            TrustStore::load(&paths.trust_file()).expect("paired fixture trust")
        else {
            panic!("pairing must have persisted the Source trust store");
        };
        let blackhole = loop {
            let socket = UdpSocket::bind("127.0.0.1:0").expect("owned silent UDP endpoint");
            if ![47902, 47903].contains(&socket.local_addr().expect("UDP address").port()) {
                break socket;
            }
        };
        blackhole.set_nonblocking(true).expect("nonblocking UDP");
        let port = blackhole.local_addr().expect("UDP address").port();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let (tx, observations) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("raw peer runtime");
            runtime.block_on(async move {
                let address: SocketAddr = format!("127.0.0.1:{listener_port}").parse().unwrap();
                let mut tls = identity::trusted_client_config(&identity, &trust)
                    .expect("real pinned runtime TLS configuration");
                tls.alpn_protocols = vec![limits::ALPN_INPUT.as_bytes().to_vec()];
                let tcp = tokio::time::timeout(TIMEOUT, tokio::net::TcpStream::connect(address))
                    .await.expect("bounded Listener TCP connect").expect("Listener TCP");
                tcp.set_nodelay(true).expect("TCP nodelay");
                let connector = tokio_rustls::TlsConnector::from(Arc::new(tls));
                let tls = tokio::time::timeout(
                    TIMEOUT,
                    connector.connect(identity::server_name(address.ip()), tcp),
                ).await.expect("bounded runtime TLS handshake").expect("paired runtime TLS");
                let peer = identity::peer_node_id(
                    tls.get_ref().1.peer_certificates().expect("Listener certificate chain"),
                ).expect("authenticated Listener certificate");
                assert_eq!(peer.as_bytes().as_slice(), listener_id);
                let (reports, mut rx) = tokio::sync::mpsc::channel(64);
                let link = transport::link::spawn(LinkConfig {
                    local: identity.node_id(),
                    peer,
                    direction: Direction::Outgoing,
                    key: identity::random16().expect("link incarnation"),
                    local_hello: wire::Hello {
                        node_id: identity.node_id().as_bytes().to_vec(),
                        capabilities: vec![
                            wire::Capability::AudioControlV1 as i32,
                            wire::Capability::SystemAudioSource as i32,
                        ],
                        ..Default::default()
                    },
                    stream: Stream::Client(Box::new(tls)),
                    reports,
                    queue_capacity: 64,
                    liveness: transport::LinkLiveness::default(),
                }).expect("real transport Link");
                let mut parent = None;
                let mut session = None;
                let mut committed = false;
                let mut serial = 0;
                let mut packet_seen = false;
                let mut pulse = tokio::time::interval(Duration::from_millis(25));
                let mut last_keepalive = Instant::now();
                let mut buffer = [0; 2048];
                loop {
                    tokio::select! {
                        report = rx.recv() => match report.expect("live parent reports") {
                            LinkReport::Up { connection_id, .. } => {
                                parent = Some(connection_id);
                                link.handle.send_audio_control(wire::AudioControl {
                                    control_version: 1,
                                    body: Some(wire::audio_control::Body::Hello(wire::AudioHello {
                                        group_id: group.clone(),
                                        listener_node_id: listener_id.clone(),
                                        role: wire::AudioNodeRole::Source as i32,
                                        udp_port: u32::from(port),
                                        media_versions: vec![1],
                                        consent_revision: 1,
                                        directions: limits::AUDIO_DIRECTION_SEND,
                                        listener_available: true,
                                    })),
                                }).await.expect("authenticated Source Hello");
                                tx.send(Observation::Up).expect("test observer");
                            }
                            LinkReport::Message { message: Inbound::Audio { message, connection_id }, .. } => {
                                assert_eq!(Some(connection_id), parent);
                                match message.body.expect("audio control body") {
                                    wire::audio_control::Body::Offer(offer) => {
                                        assert!(session.is_none(), "exactly one session agreement");
                                        assert_eq!(offer.parent_connection_id.as_slice(), connection_id.as_slice());
                                        assert_eq!(offer.source_node_id.as_slice(), identity.node_id().as_bytes().as_slice());
                                        assert_eq!(offer.listener_node_id, listener_id);
                                        assert_eq!(offer.group_id, group);
                                        let mut nonce = identity::random16().expect("fresh Source nonce").to_vec();
                                        nonce.extend_from_slice(&identity::random16().expect("fresh Source nonce"));
                                        link.handle.send_audio_control(wire::AudioControl {
                                            control_version: 1,
                                            body: Some(wire::audio_control::Body::Answer(wire::AudioAnswer {
                                                session_id: offer.session_id.clone(),
                                                accepted: true,
                                                reason: 0,
                                                accepted_format: offer.format,
                                                udp_port: u32::from(port),
                                                consent_revision: 1,
                                                source_nonce: nonce,
                                            })),
                                        }).await.expect("validated authenticated AudioAnswer");
                                        session = Some(offer.session_id.clone());
                                        tx.send(Observation::Offer(offer)).expect("test observer");
                                    }
                                    wire::audio_control::Body::Commit(commit) => {
                                        assert_eq!(Some(&commit.session_id), session.as_ref());
                                        assert_eq!(commit.transcript_sha256.len(), 32);
                                        committed = true;
                                        tx.send(Observation::Commit(commit)).expect("test observer");
                                    }
                                    wire::audio_control::Body::Stop(stop) => {
                                        assert_eq!(Some(&stop.session_id), session.as_ref());
                                        committed = false;
                                        tx.send(Observation::Stopped(stop)).expect("test observer");
                                    }
                                    _ => {}
                                }
                            }
                            LinkReport::Down { reason, .. } => panic!("UDP fault must not end parent TLS: {reason:?}"),
                            LinkReport::PeerRejected { .. } => panic!("paired fixture identity rejected"),
                            _ => {}
                        },
                        _ = pulse.tick() => {
                            if stopping.load(Ordering::Acquire) {
                                // Reaching here proves the real Link still owns the parent after failure.
                                tx.send(Observation::Alive).expect("test observer");
                                break;
                            }
                            match blackhole.recv_from(&mut buffer) {
                                Ok((length, sender)) => {
                                    assert!(length > 0, "real native path probe");
                                    if !packet_seen {
                                        tx.send(Observation::Datagram(sender)).expect("test observer");
                                        packet_seen = true;
                                    }
                                    // Deliberately never reply, even though TLS control remains responsive.
                                }
                                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                                Err(error) => panic!("owned UDP receive: {error}"),
                            }
                            if committed && last_keepalive.elapsed() >= Duration::from_millis(250) {
                                serial += 1;
                                link.handle.send_audio_control(wire::AudioControl {
                                    control_version: 1,
                                    body: Some(wire::audio_control::Body::Keepalive(wire::AudioKeepalive {
                                        session_id: session.clone().expect("committed session"),
                                        control_serial: serial,
                                        authorized_for_ms: 3000,
                                    })),
                                }).await.expect("control TTL stays alive during UDP fault");
                                last_keepalive = Instant::now();
                            }
                        }
                    }
                }
                link.handle.close();
                tokio::time::timeout(Duration::from_secs(2), link.task)
                    .await.expect("owned Link shuts down").expect("Link task joins");
            });
        });
        Self {
            stop,
            thread: Some(thread),
            observations,
            port,
        }
    }

    fn observe(&self) -> Observation {
        self.observations
            .recv_timeout(TIMEOUT)
            .expect("bounded real peer observation")
    }
}

impl Drop for RawPeer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let result = thread.join();
            if !std::thread::panicking() {
                result.expect("raw peer thread joins");
            }
        }
    }
}

#[test]
fn silent_authenticated_source_udp_surfaces_native_path_failure_without_losing_parent() {
    let _guard = common::serial();
    let mut two = Unpaired::start_audio("audio-path-blackhole");
    two.wait_ready();
    let mut listener = Window::open(&two.a);
    let mut source = Window::open(&two.b);
    audio::pair(&two, &mut listener, &mut source);
    let listener_id = audio::node_id(&two.a_id);
    let source_id = audio::node_id(&two.b_id);
    let listener_port = audio::ports(1)[0];
    listener.configure(audio::config(
        gui::AudioRole::Listener,
        listener_port,
        &[],
        std::slice::from_ref(&source_id),
        true,
    ));
    let group = listener.status().config.expect("Listener config").group_id;
    assert_eq!(group.len(), 16);
    drop(source);
    two.b.stop();
    two.a
        .wait_for("only owned Source core stops", TIMEOUT, |status| {
            status.members == 0
        })
        .expect("old parent is gone before replacement");
    let raw = RawPeer::start(&two.b.root, two.a_port, listener_id, group);
    assert!(matches!(raw.observe(), Observation::Up));
    two.a
        .wait_for("paired replacement parent is up", TIMEOUT, |status| {
            status.members == 1
        })
        .expect("authenticated parent membership");
    listener.start(&source_id);
    let mut offer = None;
    let mut commit = None;
    let mut sender = None;
    while offer.is_none() || commit.is_none() || sender.is_none() {
        match raw.observe() {
            Observation::Offer(value) => offer = Some(value),
            Observation::Commit(value) => commit = Some(value),
            Observation::Datagram(value) => sender = Some(value),
            _ => panic!("native startup must agree, commit, and send a real probe before stopping"),
        }
    }
    let offer = offer.unwrap();
    assert_eq!(offer.udp_port, u32::from(listener_port));
    assert_eq!(commit.unwrap().session_id, offer.session_id);
    assert_eq!(sender.unwrap().port(), listener_port);
    let failed = listener.wait(
        "native no-reply UDP path fails with typed evidence",
        |status| {
            for session in &status.sessions {
                assert_ne!(session.state, gui::AudioSessionState::Running as i32);
                assert_ne!(session.capture_active, Some(true));
                assert_ne!(session.sending, Some(true));
            }
            status.sessions.iter().any(|session| {
                session.session_id == offer.session_id
                    && session.state == gui::AudioSessionState::Failed as i32
                    && session.path_diagnostic
                        == gui::AudioPathDiagnostic::NoAuthenticatedPeerPackets as i32
            })
        },
    );
    let session = failed
        .sessions
        .iter()
        .find(|s| s.session_id == offer.session_id)
        .expect("failed exact agreement remains observable");
    assert_eq!(session.peer, source_id);
    assert_eq!(session.local_role, gui::AudioRole::Listener as i32);
    assert_eq!(session.local_udp_port, u32::from(listener_port));
    assert_eq!(session.peer_udp_port, u32::from(raw.port));
    assert!(session
        .health
        .as_ref()
        .is_none_or(|health| health.received_packets == 0));
    assert!(session.remote_quality.is_none());
    let Observation::Stopped(stop) = raw.observe() else {
        panic!("native path failure must stop the exact peer session");
    };
    assert_eq!(stop.session_id, offer.session_id);
    assert_eq!(stop.reason, wire::AudioStopCode::Timeout as i32);
    two.a
        .wait_for("UDP failure preserves input parent", TIMEOUT, |status| {
            status.members == 1
        })
        .expect("parent remains up after native failure");
    raw.stop.store(true, Ordering::Release);
    assert!(matches!(raw.observe(), Observation::Alive));
    let source_port = raw.port;
    drop(raw);
    UdpSocket::bind(("127.0.0.1", source_port))
        .expect("owned RawPeer releases its silent UDP socket");
    listener.closed();
    two.a.stop();
    UdpSocket::bind(("127.0.0.1", listener_port))
        .expect("owned Listener worker releases its UDP socket on core shutdown");
}
