//! Framing behaviour over an in-memory duplex: no OS endpoint is involved, so
//! these assertions are about the codec alone.
//!
//! The invariant under test throughout is the one the documents state for the
//! wire too: a length prefix is judged from its four bytes, before any body byte
//! is buffered, and an outbound frame above the channel limit is never written.

use std::time::Duration;

use ipc::{FrameChannel, Framed, IpcError};
use protocol::ipc::{agent_frame, AgentFrame, AgentReady, GetStatus, GuiFrame, Ping, Pong};
use protocol::FrameError;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The in-memory pipe must hold a complete maximum-size frame (65540 bytes with
/// the prefix), otherwise `write_all` of a full-size frame would wait for a
/// reader that the test has not started yet.
fn duplex() -> (
    Framed<tokio::io::DuplexStream>,
    Framed<tokio::io::DuplexStream>,
) {
    let (core, agent) = tokio::io::duplex(128 * 1024);
    (Framed::ipc(core), Framed::ipc(agent))
}

#[tokio::test]
async fn round_trips_every_frame_shape_used_by_the_channels() {
    let (mut core, mut agent) = duplex();

    core.send(&AgentFrame {
        major: 0,
        minor: 3,
        seq: 7,
        msg: Some(agent_frame::Msg::Ping(Ping { nonce: 42 })),
    })
    .await
    .unwrap();
    let frame: AgentFrame = agent.recv().await.unwrap();
    assert_eq!(frame.major, 0);
    assert_eq!(frame.minor, 3);
    assert_eq!(frame.seq, 7);
    assert!(matches!(frame.msg, Some(agent_frame::Msg::Ping(ping)) if ping.nonce == 42));

    agent
        .send(&AgentFrame {
            major: 0,
            minor: 3,
            seq: 1,
            msg: Some(agent_frame::Msg::Ready(AgentReady {
                capabilities: vec!["capture".to_owned()],
                platform: "platform-x11".to_owned(),
                session_kind: "x11".to_owned(),
                backend: None,
            })),
        })
        .await
        .unwrap();
    let frame: AgentFrame = core.recv().await.unwrap();
    match frame.msg {
        Some(agent_frame::Msg::Ready(ready)) => {
            assert_eq!(ready.platform, "platform-x11");
            assert_eq!(ready.capabilities, ["capture"]);
            assert!(ready.backend.is_none());
        }
        other => panic!("expected AgentReady, got {other:?}"),
    }

    core.send(&GuiFrame {
        seq: 1,
        request_id: 9,
        msg: Some(protocol::ipc::gui_frame::Msg::GetStatus(GetStatus {})),
    })
    .await
    .unwrap();
    let frame: GuiFrame = agent.recv().await.unwrap();
    assert_eq!(frame.request_id, 9);
    assert!(matches!(
        frame.msg,
        Some(protocol::ipc::gui_frame::Msg::GetStatus(_))
    ));

    core.send(&Pong { nonce: 1 }).await.unwrap();
    assert_eq!(agent.recv::<Pong>().await.unwrap().nonce, 1);
}

#[tokio::test]
async fn the_channel_limit_is_the_documented_maximum_frame() {
    assert_eq!(FrameChannel::Ipc.max_frame_len(), 65_536);

    let (mut sender, mut receiver) = duplex();
    let at_limit = vec![0xABu8; FrameChannel::Ipc.max_frame_len()];
    sender.send_bytes(&at_limit).await.unwrap();
    assert_eq!(receiver.recv_bytes().await.unwrap(), at_limit);
    assert_eq!(receiver.buffered_len(), 0, "a complete frame is consumed");

    let above_limit = vec![0xABu8; FrameChannel::Ipc.max_frame_len() + 1];
    let error = sender.send_bytes(&above_limit).await.unwrap_err();
    assert!(
        matches!(
            error,
            IpcError::Framing(FrameError::TooLarge { len, max })
                if len == 65_537 && max == 65_536
        ),
        "{error:?}"
    );
}

#[tokio::test]
async fn an_oversized_frame_leaves_the_stream_untouched() {
    let (mut sender, mut receiver) = duplex();

    let error = sender
        .send_bytes(&vec![0u8; FrameChannel::Ipc.max_frame_len() + 1])
        .await
        .unwrap_err();
    assert!(matches!(error, IpcError::Framing(_)));

    // The rejected frame must not have written anything: the first bytes the peer
    // sees are the next, valid frame.
    sender.send_bytes(&[1, 2, 3]).await.unwrap();
    let mut bytes = [0u8; 7];
    receiver.get_mut().read_exact(&mut bytes).await.unwrap();
    assert_eq!(bytes, [0, 0, 0, 3, 1, 2, 3]);
}

#[tokio::test]
async fn an_invalid_prefix_is_rejected_from_its_four_bytes() {
    for (prefix, expected) in [
        (0u32, FrameError::Empty),
        (
            u32::MAX,
            FrameError::TooLarge {
                len: u32::MAX as usize,
                max: 65_536,
            },
        ),
        (
            65_537,
            FrameError::TooLarge {
                len: 65_537,
                max: 65_536,
            },
        ),
    ] {
        let (mut writer, reader) = tokio::io::duplex(64);
        let mut framed = Framed::ipc(reader);
        // Only the prefix: the body is never written, so a rejection can only
        // come from the four bytes already present.
        writer.write_all(&prefix.to_be_bytes()).await.unwrap();
        writer.flush().await.unwrap();

        let error = framed.recv_bytes().await.unwrap_err();
        assert_eq!(
            error_display(&error),
            format!("{expected:?}"),
            "prefix {prefix}"
        );
        assert!(matches!(error, IpcError::Framing(_)));
    }
}

#[tokio::test]
async fn decoding_is_incremental_and_the_buffer_stays_bounded() {
    let (mut writer, reader) = tokio::io::duplex(64 * 1024);
    let mut framed = Framed::ipc(reader);

    assert_eq!(framed.buffered_len(), 0);
    assert_eq!(
        framed.max_buffered_len(),
        FrameChannel::Ipc.max_frame_len() + 4 + ipc::framed::READ_CHUNK
    );

    writer.write_all(&10u32.to_be_bytes()).await.unwrap();
    writer.write_all(&[7, 7, 7, 7]).await.unwrap();
    writer.flush().await.unwrap();

    // A frame that is present only in part must not decode.
    let incomplete = tokio::time::timeout(Duration::from_millis(50), framed.recv_bytes()).await;
    assert!(incomplete.is_err(), "an incomplete frame must not decode");
    assert_eq!(
        framed.buffered_len(),
        8,
        "prefix plus the bytes that arrived"
    );
    assert!(framed.buffered_len() < framed.max_buffered_len());

    writer.write_all(&[8; 6]).await.unwrap();
    writer.flush().await.unwrap();
    let body = framed.recv_bytes().await.unwrap();
    assert_eq!(body, [7, 7, 7, 7, 8, 8, 8, 8, 8, 8]);
    assert_eq!(framed.buffered_len(), 0);
}

#[tokio::test]
async fn eof_is_closed_with_or_without_a_partial_frame() {
    let (writer, reader) = tokio::io::duplex(64);
    drop(writer);
    let error = Framed::ipc(reader).recv_bytes().await.unwrap_err();
    assert!(matches!(error, IpcError::Closed), "{error:?}");
    assert!(error.is_disconnected());

    // A truncated frame is still "the peer is gone", never a framing error: the
    // agent must release capture on either.
    let (mut writer, reader) = tokio::io::duplex(64);
    writer.write_all(&100u32.to_be_bytes()).await.unwrap();
    writer.write_all(b"only a few").await.unwrap();
    drop(writer);
    let error = Framed::ipc(reader).recv_bytes().await.unwrap_err();
    assert!(matches!(error, IpcError::Closed), "{error:?}");
}

#[tokio::test]
async fn a_valid_frame_with_a_malformed_body_is_not_a_framing_error() {
    let (mut writer, reader) = tokio::io::duplex(64);
    let mut framed = Framed::ipc(reader);

    // Length-delimited unknown field declaring five bytes while one remains.
    writer.write_all(&3u32.to_be_bytes()).await.unwrap();
    writer.write_all(&[0x0A, 0x05, 0x00]).await.unwrap();
    writer.flush().await.unwrap();

    let error = framed.recv::<AgentFrame>().await.unwrap_err();
    assert!(matches!(error, IpcError::Malformed { .. }), "{error:?}");
}

/// `FrameError` has no `Display`; compare through its `Debug` shape so the
/// failure message names the exact length that was rejected.
fn error_display(error: &IpcError) -> String {
    match error {
        IpcError::Framing(framing) => format!("{framing:?}"),
        other => format!("{other:?}"),
    }
}
