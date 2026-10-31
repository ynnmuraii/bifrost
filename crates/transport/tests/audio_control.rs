//! Isolated identities, loopback sockets and real pinned TLS; no audio devices.
use identity::{IdentityPaths, NodeIdentity, TrustStore, TrustedPeer};
use protocol::{limits, validate::AudioViolation, wire};
use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::mpsc;
use transport::{
    convert::{inbound_from_envelope, Inbound},
    Direction, Link, LinkConfig, LinkReport, Stream, TransportError,
};

const CAP: i32 = wire::Capability::AudioControlV1 as i32;
const FOCUS_CAP: i32 = wire::Capability::AudioFocusReportV1 as i32;
static COUNTER: AtomicU64 = AtomicU64::new(0);

struct Root(std::path::PathBuf);
impl Root {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "bifrost-audio-wire-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        )))
    }
    fn identity(&self, name: &str) -> NodeIdentity {
        NodeIdentity::load_or_create(&IdentityPaths::rooted_at(self.0.join(name)))
            .unwrap()
            .0
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn trust(peer: transport::NodeId) -> TrustStore {
    let mut store = TrustStore::new();
    store.trust(TrustedPeer::paired(
        peer,
        "peer",
        None,
        identity::now_unix_ms(),
    ));
    store
}
async fn streams(client: &NodeIdentity, server: &NodeIdentity) -> (Stream, Stream) {
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
    tcp.set_nodelay(true).unwrap();
    let client_tls = connector
        .connect(identity::server_name(address.ip()), tcp)
        .await
        .unwrap();
    (
        Stream::Client(Box::new(client_tls)),
        Stream::Server(Box::new(accepting.await.unwrap())),
    )
}
fn spawn(
    stream: Stream,
    local: transport::NodeId,
    peer: transport::NodeId,
    outgoing: bool,
    caps: Vec<i32>,
) -> (Link, mpsc::Receiver<LinkReport>) {
    let (reports, rx) = mpsc::channel(32);
    let link = transport::link::spawn(LinkConfig {
        local,
        peer,
        direction: if outgoing {
            Direction::Outgoing
        } else {
            Direction::Incoming
        },
        key: identity::random16().unwrap(),
        local_hello: wire::Hello {
            node_id: local.as_bytes().to_vec(),
            capabilities: caps,
            ..Default::default()
        },
        stream,
        reports,
        queue_capacity: 16,
        liveness: transport::LinkLiveness::default(),
    })
    .unwrap();
    (link, rx)
}
async fn report(rx: &mut mpsc::Receiver<LinkReport>) -> LinkReport {
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap()
}
fn up(report: LinkReport) -> [u8; 16] {
    let LinkReport::Up { connection_id, .. } = report else {
        panic!("expected Up, got {report:?}");
    };
    connection_id
}
fn message(report: LinkReport) -> Inbound {
    let LinkReport::Message { message, .. } = report else {
        panic!("expected Message, got {report:?}");
    };
    message
}
fn stop() -> wire::AudioControl {
    wire::AudioControl {
        control_version: 1,
        body: Some(wire::audio_control::Body::Stop(wire::AudioStop {
            session_id: vec![3; 16],
            reason: wire::AudioStopCode::UserStop as i32,
        })),
    }
}
fn all_bodies(connection: [u8; 16]) -> Vec<wire::AudioControl> {
    use wire::audio_control::Body;
    let format = wire::AudioFormat {
        media_version: 1,
        codec: 1,
        sample_rate: 48_000,
        channels: 2,
        frame_samples: 480,
        bitrate_bps: 96_000,
        fec: false,
        profile: 1,
        max_datagram_bytes: 1200,
        aead: 1,
    };
    vec![
        Body::Hello(wire::AudioHello {
            group_id: vec![1; 16],
            listener_node_id: vec![2; 32],
            role: 2,
            udp_port: 47903,
            media_versions: vec![1],
            listener_available: true,
            ..Default::default()
        }),
        Body::Offer(wire::AudioOffer {
            session_id: vec![3; 16],
            session_nonce: vec![4; 32],
            parent_connection_id: connection.to_vec(),
            group_id: vec![1; 16],
            listener_node_id: vec![2; 32],
            source_node_id: vec![5; 32],
            kind: 1,
            format: Some(format),
            udp_port: 47903,
            key_epoch: 1,
            control_ttl_ms: 3000,
            request_id: Vec::new(),
            ..Default::default()
        }),
        Body::Answer(wire::AudioAnswer {
            session_id: vec![3; 16],
            reason: 2,
            ..Default::default()
        }),
        Body::Commit(wire::AudioCommit {
            session_id: vec![3; 16],
            transcript_sha256: vec![6; 32],
            key_epoch: 1,
            start_muted: false,
        }),
        Body::Stop(wire::AudioStop {
            session_id: vec![3; 16],
            reason: 12,
        }),
        Body::Keepalive(wire::AudioKeepalive {
            session_id: vec![3; 16],
            control_serial: 1,
            authorized_for_ms: 3000,
        }),
        Body::Report(wire::AudioReport {
            session_id: vec![3; 16],
            device_state: 1,
            ..Default::default()
        }),
        Body::Consent(wire::AudioConsentChanged {
            group_id: vec![1; 16],
            allowed_directions: 0,
            consent_revision: 1,
        }),
        Body::StartRequest(wire::AudioStartRequest {
            request_id: vec![7; 16],
            group_id: vec![1; 16],
            source_node_id: vec![5; 32],
            consent_revision: 1,
            kind: wire::AudioStreamKind::System as i32,
            profile: wire::AudioLatencyProfile::Wifi as i32,
        }),
        Body::StartRefusal(wire::AudioStartRefusal {
            request_id: vec![7; 16],
            reason: wire::AudioStopCode::ConsentRequired as i32,
        }),
    ]
    .into_iter()
    .map(|body| wire::AudioControl {
        control_version: 1,
        body: Some(body),
    })
    .collect()
}
fn envelope(connection: [u8; 16], seq: u64, payload: wire::envelope::Payload) -> wire::Envelope {
    wire::Envelope {
        major: limits::PROTOCOL_MAJOR,
        minor: limits::PROTOCOL_MINOR,
        connection_id: connection.to_vec(),
        seq,
        payload: Some(payload),
        ..Default::default()
    }
}
fn focus() -> focus_router::Message {
    focus_router::Message::LeaseRenew {
        lease_id: [4; 16],
        nonce: [5; 16],
    }
}

#[test]
fn inbound_preserves_authenticated_parent_and_audio_errors_are_typed() {
    let connection = [7; 16];
    let message = stop();
    let mut frame = envelope(
        connection,
        2,
        wire::envelope::Payload::AudioControl(message.clone()),
    );
    assert_eq!(
        inbound_from_envelope(&frame),
        Ok(Inbound::Audio {
            message,
            connection_id: connection
        })
    );
    frame.lease_id = vec![1; 16];
    assert_eq!(
        inbound_from_envelope(&frame),
        Err(transport::ConvertError::Audio(AudioViolation::Lease))
    );
    frame.lease_id.clear();
    frame.payload = Some(wire::envelope::Payload::AudioControl(wire::AudioControl {
        control_version: 1,
        body: None,
    }));
    assert_eq!(
        inbound_from_envelope(&frame),
        Err(transport::ConvertError::Audio(AudioViolation::MissingBody))
    );
    let offer = all_bodies([8; 16])
        .into_iter()
        .find(|message| matches!(message.body, Some(wire::audio_control::Body::Offer(_))))
        .unwrap();
    frame.payload = Some(wire::envelope::Payload::AudioControl(offer));
    assert_eq!(
        inbound_from_envelope(&frame),
        Err(transport::ConvertError::Audio(
            AudioViolation::ParentConnection
        ))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn real_tls_delivers_both_directions_and_dynamic_withdrawal_stops_audio() {
    let root = Root::new();
    let client = root.identity("client");
    let server = root.identity("server");
    let (a, b) = streams(&client, &server).await;
    let (left, mut left_rx) = spawn(a, client.node_id(), server.node_id(), true, vec![CAP]);
    let (right, mut right_rx) = spawn(b, server.node_id(), client.node_id(), false, vec![CAP]);
    let connection = up(report(&mut left_rx).await);
    assert_eq!(up(report(&mut right_rx).await), connection);
    for audio in all_bodies(connection) {
        left.handle.send_audio_control(audio.clone()).await.unwrap();
        right
            .handle
            .send_audio_control(audio.clone())
            .await
            .unwrap();
        for received in [
            message(report(&mut left_rx).await),
            message(report(&mut right_rx).await),
        ] {
            assert_eq!(
                received,
                Inbound::Audio {
                    message: audio.clone(),
                    connection_id: connection
                }
            );
        }
    }
    right.handle.send_capabilities(vec![]).await.unwrap();
    assert!(
        matches!(message(report(&mut left_rx).await), Inbound::CapabilitiesChanged(caps) if caps.is_empty())
    );
    for handle in [&left.handle, &right.handle] {
        assert!(matches!(
            handle.send_audio_control(stop()).await,
            Err(TransportError::Audio(AudioViolation::Capability))
        ));
    }
    left.handle.send(focus()).await.unwrap();
    right.handle.send(focus()).await.unwrap();
    assert_eq!(message(report(&mut left_rx).await), Inbound::Focus(focus()));
    assert_eq!(
        message(report(&mut right_rx).await),
        Inbound::Focus(focus())
    );
    assert!(!left.task.is_finished() && !right.task.is_finished());
    left.handle.close();
    right.handle.close();
    left.task.abort();
    right.task.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn either_missing_capability_blocks_every_audio_body_without_transmitting() {
    for (left_caps, right_caps) in [(vec![CAP], vec![]), (vec![], vec![CAP]), (vec![], vec![])] {
        let root = Root::new();
        let client = root.identity("client");
        let server = root.identity("server");
        let (a, b) = streams(&client, &server).await;
        let (left, mut left_rx) = spawn(a, client.node_id(), server.node_id(), true, left_caps);
        let (right, mut right_rx) = spawn(b, server.node_id(), client.node_id(), false, right_caps);
        let connection = up(report(&mut left_rx).await);
        up(report(&mut right_rx).await);
        for handle in [&left.handle, &right.handle] {
            for message in all_bodies(connection) {
                assert!(matches!(
                    handle.send_audio_control(message).await,
                    Err(TransportError::Audio(AudioViolation::Capability))
                ));
            }
            handle.send(focus()).await.unwrap();
        }
        // If Audio were transmitted it would appear before these focus messages.
        assert_eq!(message(report(&mut left_rx).await), Inbound::Focus(focus()));
        assert_eq!(
            message(report(&mut right_rx).await),
            Inbound::Focus(focus())
        );
        left.handle.close();
        right.handle.close();
        left.task.abort();
        right.task.abort();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn malformed_audio_reports_audio_only_failure_and_preserves_focus_and_ping() {
    let root = Root::new();
    let client = root.identity("client");
    let server = root.identity("server");
    let (mut raw, b) = streams(&client, &server).await;
    let (right, mut rx) = spawn(b, server.node_id(), client.node_id(), false, vec![CAP]);
    let connection = [8; 16];
    transport::write_frame(
        &mut raw,
        &envelope(
            connection,
            1,
            wire::envelope::Payload::Hello(wire::Hello {
                node_id: client.node_id().as_bytes().to_vec(),
                capabilities: vec![CAP],
                ..Default::default()
            }),
        ),
    )
    .await
    .unwrap();
    let _hello = transport::read_frame(&mut raw).await.unwrap();
    assert_eq!(up(report(&mut rx).await), connection);
    let mut bad_lease = envelope(connection, 3, wire::envelope::Payload::AudioControl(stop()));
    bad_lease.lease_id = vec![1; 16];
    let malformed = [
        envelope(
            connection,
            2,
            wire::envelope::Payload::AudioControl(wire::AudioControl {
                control_version: 1,
                body: None,
            }),
        ),
        bad_lease,
    ];
    for (frame, violation) in malformed
        .into_iter()
        .zip([AudioViolation::MissingBody, AudioViolation::Lease])
    {
        transport::write_frame(&mut raw, &frame).await.unwrap();
        assert_eq!(
            message(report(&mut rx).await),
            Inbound::AudioRejected {
                violation,
                connection_id: connection
            }
        );
    }
    transport::write_frame(
        &mut raw,
        &envelope(
            connection,
            4,
            wire::envelope::Payload::Ping(wire::Ping { nonce: 99 }),
        ),
    )
    .await
    .unwrap();
    let pong = tokio::time::timeout(Duration::from_secs(5), transport::read_frame(&mut raw))
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        pong.payload,
        Some(wire::envelope::Payload::Pong(wire::Pong { nonce: 99 }))
    ));
    let focus_frame =
        transport::convert::to_envelope(&focus(), 5, &connection, Some(&[4; 16]), "").unwrap();
    transport::write_frame(&mut raw, &focus_frame)
        .await
        .unwrap();
    assert_eq!(message(report(&mut rx).await), Inbound::Focus(focus()));
    assert!(!right.task.is_finished());
    right.handle.close();
    right.task.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn generic_audio_envelope_and_prehello_violations_remain_fatal() {
    for prehello in [true, false] {
        let root = Root::new();
        let client = root.identity("client");
        let server = root.identity("server");
        let (mut raw, b) = streams(&client, &server).await;
        let (right, mut rx) = spawn(b, server.node_id(), client.node_id(), false, vec![CAP]);
        let connection = [8; 16];
        if !prehello {
            transport::write_frame(
                &mut raw,
                &envelope(
                    connection,
                    1,
                    wire::envelope::Payload::Hello(wire::Hello {
                        node_id: client.node_id().as_bytes().to_vec(),
                        capabilities: vec![CAP],
                        ..Default::default()
                    }),
                ),
            )
            .await
            .unwrap();
            transport::read_frame(&mut raw).await.unwrap();
            up(report(&mut rx).await);
        }
        // seq3 after Hello1 is a fatal gap, even though the payload is audio.
        transport::write_frame(
            &mut raw,
            &envelope(
                connection,
                if prehello { 1 } else { 3 },
                wire::envelope::Payload::AudioControl(stop()),
            ),
        )
        .await
        .unwrap();
        assert!(matches!(
            report(&mut rx).await,
            LinkReport::Down {
                reason: transport::LinkDownReason::Protocol(_),
                ..
            }
        ));
        right.task.abort();
    }
}

async fn raw_audio_link() -> (Root, Stream, Link, mpsc::Receiver<LinkReport>, [u8; 16]) {
    raw_link(vec![CAP], vec![CAP]).await
}

async fn raw_link(
    local_caps: Vec<i32>,
    remote_caps: Vec<i32>,
) -> (Root, Stream, Link, mpsc::Receiver<LinkReport>, [u8; 16]) {
    let root = Root::new();
    let client = root.identity("client");
    let server = root.identity("server");
    let (mut raw, b) = streams(&client, &server).await;
    let (link, mut rx) = spawn(b, server.node_id(), client.node_id(), false, local_caps);
    let connection = [8; 16];
    transport::write_frame(
        &mut raw,
        &envelope(
            connection,
            1,
            wire::envelope::Payload::Hello(wire::Hello {
                node_id: client.node_id().as_bytes().to_vec(),
                capabilities: remote_caps,
                ..Default::default()
            }),
        ),
    )
    .await
    .unwrap();
    transport::read_frame(&mut raw).await.unwrap();
    assert_eq!(up(report(&mut rx).await), connection);
    (root, raw, link, rx, connection)
}

#[tokio::test(flavor = "current_thread")]
async fn forged_offer_and_envelope_binding_and_wrong_major_are_fatal_not_audio_accepted() {
    for forge_connection in [true, false] {
        let (_root, mut raw, link, mut rx, connection) = raw_audio_link().await;
        let binding = if forge_connection {
            [9; 16]
        } else {
            connection
        };
        let offer = all_bodies(binding)
            .into_iter()
            .find(|audio| matches!(audio.body, Some(wire::audio_control::Body::Offer(_))))
            .unwrap();
        let mut frame = envelope(binding, 2, wire::envelope::Payload::AudioControl(offer));
        if !forge_connection {
            frame.major += 1;
        }
        transport::write_frame(&mut raw, &frame).await.unwrap();
        // Generic input header/binding violations close KVM; no accepted Audio
        // (or audio-only rejection) may be delivered first.
        assert!(matches!(
            report(&mut rx).await,
            LinkReport::Down {
                reason: transport::LinkDownReason::Protocol(_),
                ..
            }
        ));
        link.task.abort();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn raw_audio_padding_limit_is_audio_only_and_envelope_padding_is_not_audio() {
    use prost::{
        encoding::{encode_key, encode_varint, WireType},
        Message,
    };
    use tokio::io::AsyncWriteExt;
    let (_root, mut raw, link, mut rx, connection) = raw_audio_link().await;
    for (seq, size) in [(2, 2048usize), (3, 2049usize)] {
        let mut audio = stop().encode_to_vec();
        // Unknown length-delimited AudioControl field: two-byte key + two-byte length.
        let padding = size - audio.len() - 4;
        encode_key(100, WireType::LengthDelimited, &mut audio);
        encode_varint(padding as u64, &mut audio);
        audio.resize(size, 0);
        let mut body = envelope(
            connection,
            seq,
            wire::envelope::Payload::AudioControl(stop()),
        );
        body.payload = None;
        let mut bytes = body.encode_to_vec();
        encode_key(59, WireType::LengthDelimited, &mut bytes);
        encode_varint(audio.len() as u64, &mut bytes);
        bytes.extend_from_slice(&audio);
        // This padding belongs only to Envelope, never the AudioControl budget.
        encode_key(100, WireType::LengthDelimited, &mut bytes);
        encode_varint(4096, &mut bytes);
        bytes.resize(bytes.len() + 4096, 0);
        raw.write_all(&(bytes.len() as u32).to_be_bytes())
            .await
            .unwrap();
        raw.write_all(&bytes).await.unwrap();
        raw.flush().await.unwrap();
        let expected = if size == 2048 {
            Inbound::Audio {
                message: stop(),
                connection_id: connection,
            }
        } else {
            Inbound::AudioRejected {
                violation: AudioViolation::TooLarge,
                connection_id: connection,
            }
        };
        assert_eq!(message(report(&mut rx).await), expected);
    }
    transport::write_frame(
        &mut raw,
        &envelope(
            connection,
            4,
            wire::envelope::Payload::Ping(wire::Ping { nonce: 991 }),
        ),
    )
    .await
    .unwrap();
    let pong = tokio::time::timeout(Duration::from_secs(5), transport::read_frame(&mut raw))
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        pong.payload,
        Some(wire::envelope::Payload::Pong(wire::Pong { nonce: 991 }))
    ));
    let frame =
        transport::convert::to_envelope(&focus(), 5, &connection, Some(&[4; 16]), "").unwrap();
    transport::write_frame(&mut raw, &frame).await.unwrap();
    assert_eq!(message(report(&mut rx).await), Inbound::Focus(focus()));
    link.handle.close();
    link.task.abort();
}

fn focus_bodies() -> [wire::AudioControl; 2] {
    [
        wire::AudioControl {
            control_version: 1,
            body: Some(wire::audio_control::Body::Focus(wire::AudioFocusSnapshot {
                owner_node_id: vec![1; 32],
                owner_serial: 1,
                focus_revision: 1,
                active_node_id: vec![2; 32],
                lease_id: vec![3; 16],
                committed: true,
            })),
        },
        wire::AudioControl {
            control_version: 1,
            body: Some(wire::audio_control::Body::FocusRequest(
                wire::AudioFocusRequest {
                    group_id: vec![4; 16],
                },
            )),
        },
    ]
}

#[tokio::test(flavor = "current_thread")]
async fn focus_only_capability_delivers_both_directions_and_survives_audio_withdrawal() {
    let root = Root::new();
    let client = root.identity("client");
    let server = root.identity("server");
    let (a, b) = streams(&client, &server).await;
    let (left, mut left_rx) = spawn(a, client.node_id(), server.node_id(), true, vec![FOCUS_CAP]);
    let (right, mut right_rx) = spawn(
        b,
        server.node_id(),
        client.node_id(),
        false,
        vec![CAP, FOCUS_CAP],
    );
    let connection = up(report(&mut left_rx).await);
    assert_eq!(up(report(&mut right_rx).await), connection);
    right
        .handle
        .send_capabilities(vec![FOCUS_CAP])
        .await
        .unwrap();
    assert!(matches!(
        message(report(&mut left_rx).await),
        Inbound::CapabilitiesChanged(caps) if caps == vec![FOCUS_CAP]
    ));
    for audio in focus_bodies() {
        for handle in [&left.handle, &right.handle] {
            handle.send_audio_control(audio.clone()).await.unwrap();
            assert!(matches!(
                handle.send_audio_control(stop()).await,
                Err(TransportError::Audio(AudioViolation::Capability))
            ));
        }
        for received in [
            message(report(&mut left_rx).await),
            message(report(&mut right_rx).await),
        ] {
            assert_eq!(
                received,
                Inbound::Audio {
                    message: audio.clone(),
                    connection_id: connection
                }
            );
        }
    }
    right.handle.send_capabilities(vec![CAP]).await.unwrap();
    assert!(matches!(
        message(report(&mut left_rx).await),
        Inbound::CapabilitiesChanged(_)
    ));
    for handle in [&left.handle, &right.handle] {
        for audio in focus_bodies() {
            assert!(matches!(
                handle.send_audio_control(audio).await,
                Err(TransportError::Audio(AudioViolation::Capability))
            ));
        }
    }
    left.handle.close();
    right.handle.close();
    left.task.abort();
    right.task.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn missing_focus_capability_refuses_send_and_raw_receive_without_harming_input() {
    for (local_caps, remote_caps) in [
        (vec![CAP], vec![CAP, FOCUS_CAP]),
        (vec![CAP, FOCUS_CAP], vec![CAP]),
        (vec![CAP], vec![CAP]),
    ] {
        let (_root, mut raw, link, mut rx, connection) = raw_link(local_caps, remote_caps).await;
        for (index, audio) in focus_bodies().into_iter().enumerate() {
            assert!(matches!(
                link.handle.send_audio_control(audio.clone()).await,
                Err(TransportError::Audio(AudioViolation::Capability))
            ));
            transport::write_frame(
                &mut raw,
                &envelope(
                    connection,
                    index as u64 + 2,
                    wire::envelope::Payload::AudioControl(audio),
                ),
            )
            .await
            .unwrap();
            assert_eq!(
                message(report(&mut rx).await),
                Inbound::AudioRejected {
                    violation: AudioViolation::Capability,
                    connection_id: connection,
                }
            );
        }
        transport::write_frame(
            &mut raw,
            &envelope(connection, 4, wire::envelope::Payload::AudioControl(stop())),
        )
        .await
        .unwrap();
        assert_eq!(
            message(report(&mut rx).await),
            Inbound::Audio {
                message: stop(),
                connection_id: connection
            }
        );
        let frame =
            transport::convert::to_envelope(&focus(), 5, &connection, Some(&[4; 16]), "").unwrap();
        transport::write_frame(&mut raw, &frame).await.unwrap();
        assert_eq!(message(report(&mut rx).await), Inbound::Focus(focus()));
        link.handle.close();
        link.task.abort();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn malformed_and_unknown_focus_metadata_are_bounded_audio_only_failures() {
    use prost::{
        encoding::{encode_key, encode_varint, WireType},
        Message,
    };
    use tokio::io::AsyncWriteExt;
    let (_root, mut raw, link, mut rx, connection) =
        raw_link(vec![FOCUS_CAP], vec![FOCUS_CAP]).await;
    let mut malformed = focus_bodies()[0].clone();
    if let Some(wire::audio_control::Body::Focus(snapshot)) = &mut malformed.body {
        snapshot.focus_revision = 0;
    }
    let cases = [
        (malformed.encode_to_vec(), Some(AudioViolation::Epoch)),
        (vec![8, 1, 0xb2, 1, 0], Some(AudioViolation::MissingBody)),
    ];
    let mut seq = 2;
    for (audio, violation) in cases
        .into_iter()
        .chain([2048usize, 2049].into_iter().map(|size| {
            let mut audio = focus_bodies()[0].encode_to_vec();
            let padding = size - audio.len() - 4;
            encode_key(100, WireType::LengthDelimited, &mut audio);
            encode_varint(padding as u64, &mut audio);
            audio.resize(size, 0);
            (audio, (size > 2048).then_some(AudioViolation::TooLarge))
        }))
    {
        let mut bytes = envelope(
            connection,
            seq,
            wire::envelope::Payload::Ping(wire::Ping { nonce: 0 }),
        );
        bytes.payload = None;
        let mut bytes = bytes.encode_to_vec();
        encode_key(59, WireType::LengthDelimited, &mut bytes);
        encode_varint(audio.len() as u64, &mut bytes);
        bytes.extend_from_slice(&audio);
        raw.write_all(&(bytes.len() as u32).to_be_bytes())
            .await
            .unwrap();
        raw.write_all(&bytes).await.unwrap();
        raw.flush().await.unwrap();
        let expected = match violation {
            Some(violation) => Inbound::AudioRejected {
                violation,
                connection_id: connection,
            },
            None => Inbound::Audio {
                message: focus_bodies()[0].clone(),
                connection_id: connection,
            },
        };
        assert_eq!(message(report(&mut rx).await), expected);
        seq += 1;
    }
    transport::write_frame(
        &mut raw,
        &envelope(
            connection,
            seq,
            wire::envelope::Payload::Ping(wire::Ping { nonce: 991 }),
        ),
    )
    .await
    .unwrap();
    let pong = tokio::time::timeout(Duration::from_secs(5), transport::read_frame(&mut raw))
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        pong.payload,
        Some(wire::envelope::Payload::Pong(wire::Pong { nonce: 991 }))
    ));
    let frame = transport::convert::to_envelope(&focus(), seq + 1, &connection, Some(&[4; 16]), "")
        .unwrap();
    transport::write_frame(&mut raw, &frame).await.unwrap();
    assert_eq!(message(report(&mut rx).await), Inbound::Focus(focus()));
    link.handle.close();
    link.task.abort();
}
