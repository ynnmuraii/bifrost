use super::*;
use protocol::frame::{decode_message, encode_message, Channel};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

type RawPeer = tokio_rustls::server::TlsStream<tokio::net::TcpStream>;
const MARKER: u64 = 0x5345_4331;

async fn receive(stream: &mut RawPeer) -> std::io::Result<wire::Envelope> {
    let length = stream.read_u32().await?;
    if !(1..=Channel::InputControl.max_frame_len() as u32).contains(&length) {
        return Err(std::io::Error::other("invalid peer frame length"));
    }
    let mut frame = vec![0; length as usize + 4];
    frame[..4].copy_from_slice(&length.to_be_bytes());
    stream.read_exact(&mut frame[4..]).await?;
    decode_message(Channel::InputControl, &frame)
        .map_err(|_| std::io::Error::other("invalid peer frame"))?
        .ok_or_else(|| std::io::Error::other("incomplete peer frame"))
}

async fn send(stream: &mut RawPeer, envelope: &wire::Envelope) -> std::io::Result<()> {
    let frame = encode_message(Channel::InputControl, envelope)
        .map_err(|_| std::io::Error::other("cannot encode frame"))?;
    stream.write_all(&frame).await?;
    stream.flush().await
}

fn envelope(connection_id: &[u8], seq: u64, payload: wire::envelope::Payload) -> wire::Envelope {
    wire::Envelope {
        major: protocol::limits::PROTOCOL_MAJOR,
        minor: protocol::limits::PROTOCOL_MINOR,
        connection_id: connection_id.to_vec(),
        seq,
        payload: Some(payload),
        ..Default::default()
    }
}

// Identical authenticated clients exercise admission and refusal, not a mocked report.
async fn scenario(full: bool) -> bool {
    let root = temp_root(if full {
        "admission-full"
    } else {
        "admission-control"
    });
    let victim_identity = Arc::new(identity_at(&root, "WORKSTATION"));
    let member_identity = Arc::new(identity_at(&root, "LAPTOP"));
    let raw_identity = identity_at(&root, "HTPC");
    let victim_id = victim_identity.node_id();
    let member_id = member_identity.node_id();
    let raw_id = raw_identity.node_id();
    let victim_address = free_address().await;
    let member_address = free_address().await;
    let raw_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let raw_address = raw_listener.local_addr().unwrap();
    let config = identity::server_config(
        &raw_identity,
        identity::TrustPolicy::Pinned(BTreeSet::from([victim_id])),
        &identity::LINK_ALPN,
    )
    .unwrap();
    let accepting = tokio::spawn(async move {
        let (tcp, _) = raw_listener.accept().await.unwrap();
        tokio_rustls::TlsAcceptor::from(Arc::new(config))
            .accept(tcp)
            .await
            .unwrap()
    });
    let mut victim = spawn_node(
        "WORKSTATION",
        victim_identity,
        victim_address,
        &[(member_id, member_address), (raw_id, raw_address)],
        &[],
        if full { 1 } else { 2 },
    )
    .await;
    let member = spawn_node(
        "LAPTOP",
        member_identity,
        member_address,
        &[(victim_id, victim_address)],
        &[victim_address],
        2,
    )
    .await;
    assert!(wait_for_members(&victim, 1, FORM_TIMEOUT).await);
    assert!(wait_for_members(&member, 1, FORM_TIMEOUT).await);
    tokio::time::timeout(
        EVENT_TIMEOUT,
        victim.manager.refresh_discovered_route(raw_id, raw_address),
    )
    .await
    .unwrap()
    .unwrap();
    let mut raw = tokio::time::timeout(EVENT_TIMEOUT, accepting)
        .await
        .unwrap()
        .unwrap();
    let request = tokio::time::timeout(EVENT_TIMEOUT, receive(&mut raw))
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        request.payload,
        Some(wire::envelope::Payload::Hello(_))
    ));
    let connection_id = request.connection_id;
    send(
        &mut raw,
        &envelope(
            &connection_id,
            1,
            wire::envelope::Payload::Hello(hello_for(raw_id, "HTPC")),
        ),
    )
    .await
    .unwrap();
    if full {
        let refusal = tokio::time::timeout(EVENT_TIMEOUT, async {
            loop {
                if let Some(wire::envelope::Payload::Error(error)) =
                    receive(&mut raw).await.unwrap().payload
                {
                    break error;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(refusal.code, wire::ErrorCode::MembershipFull as i32);
        assert!(wait_for_event(&mut victim.events, |event| matches!(event,
            LinkEvent::Down { peer, reason: LinkDownReason::MembershipFull, .. } if *peer == raw_id
        ), EVENT_TIMEOUT).await.is_some());
        assert_eq!(victim.manager.member_count().await, 1);
        assert!(victim.manager.handle(&raw_id).await.is_none());
    } else {
        assert!(wait_for_event(
            &mut victim.events,
            |event| matches!(event,
                LinkEvent::Up { peer, .. } if *peer == raw_id
            ),
            EVENT_TIMEOUT
        )
        .await
        .is_some());
        assert_eq!(victim.manager.member_count().await, 2);
    }
    let wrote = send(
        &mut raw,
        &envelope(
            &connection_id,
            2,
            wire::envelope::Payload::OwnerState(wire::OwnerState {
                serial: MARKER,
                owner_node_id: raw_id.as_bytes().to_vec(),
                granted: true,
            }),
        ),
    )
    .await
    .is_ok();
    assert!(full || wrote);
    let responder = tokio::spawn(async move {
        let mut seq = 3;
        while let Ok(received) = receive(&mut raw).await {
            if let Some(wire::envelope::Payload::Ping(ping)) = received.payload {
                if send(
                    &mut raw,
                    &envelope(
                        &connection_id,
                        seq,
                        wire::envelope::Payload::Pong(wire::Pong { nonce: ping.nonce }),
                    ),
                )
                .await
                .is_err()
                {
                    break;
                }
                seq += 1;
            }
        }
    });
    let visible = wait_for_event(
        &mut victim.events,
        |event| {
            matches!(event,
                LinkEvent::Message {
                    peer,
                    message: Inbound::Focus(Message::OwnerState { serial, owner, granted: true }),
                } if *peer == raw_id && *serial == MARKER && *owner == Some(raw_id)
            )
        },
        Duration::from_millis(LIVENESS.offline_ms + 1_000),
    )
    .await
    .is_some();
    if full {
        tokio::time::timeout(EVENT_TIMEOUT, responder)
            .await
            .expect("refused peer must observe terminal socket closure")
            .unwrap();
    } else {
        responder.abort();
        let _ = responder.await;
    }
    victim.kill().await;
    member.kill().await;
    std::fs::remove_dir_all(root).unwrap();
    visible
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rejected_member_cannot_publish_runtime_owner_state() {
    assert!(
        scenario(false).await,
        "admitted positive control must deliver OwnerState"
    );
    assert!(
        !scenario(true).await,
        "refused member must never publish runtime control"
    );
}
