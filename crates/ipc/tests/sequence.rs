//! `seq` and frame-version handling of the two link types.
//!
//! The documents make a sequence break lethal for the channel and, for the agent,
//! a reason to release: «Свой `seq` в каждом направлении; при разрыве
//! последовательности соединение закрывается». The three ways to break it are
//! distinguished here — a first frame that is not 1, a repeat, and a gap.

use ipc::{AgentLink, Framed, GuiLink, IpcError};
use protocol::ipc::{agent_frame, gui_frame, AgentFrame, GetStatus, GuiFrame, Ping, Pong};
use protocol::SeqError;
use tokio::io::{AsyncWriteExt, DuplexStream};

fn duplex() -> (DuplexStream, DuplexStream) {
    tokio::io::duplex(64 * 1024)
}

fn ping(nonce: u64) -> AgentFrame {
    AgentFrame {
        major: 0,
        minor: 3,
        seq: nonce,
        msg: Some(agent_frame::Msg::Ping(Ping { nonce })),
    }
}

#[tokio::test]
async fn each_direction_starts_at_one_and_is_stamped() {
    let (core_io, agent_io) = duplex();
    let mut core = AgentLink::new(core_io, 0, 3);
    let mut agent = AgentLink::new(agent_io, 0, 3);

    assert_eq!(core.outbound_next(), 1);
    assert_eq!(core.inbound_expected(), 1);
    assert_eq!(core.version(), (0, 3));

    core.send(agent_frame::Msg::Ping(Ping { nonce: 11 }))
        .await
        .unwrap();
    assert_eq!(
        core.outbound_next(),
        2,
        "sending advances only the outbound side"
    );

    let frame = agent.recv().await.unwrap();
    assert_eq!(frame.seq, 1);
    assert_eq!(frame.major, 0);
    assert_eq!(frame.minor, 3);
    assert_eq!(agent.inbound_expected(), 2);
    assert_eq!(
        agent.outbound_next(),
        1,
        "receiving does not advance the outbound side"
    );

    // The reply direction has its own counter, also starting at 1.
    agent
        .send(agent_frame::Msg::Pong(Pong { nonce: 11 }))
        .await
        .unwrap();
    let reply = core.recv().await.unwrap();
    assert_eq!(reply.seq, 1);
    assert!(matches!(reply.msg, Some(agent_frame::Msg::Pong(pong)) if pong.nonce == 11));

    // ... and the next frames are 2 in both directions.
    core.send(agent_frame::Msg::Ping(Ping { nonce: 12 }))
        .await
        .unwrap();
    assert_eq!(agent.recv().await.unwrap().seq, 2);
}

#[tokio::test]
async fn a_first_frame_that_is_not_seq_one_is_not_started() {
    let (writer, reader) = duplex();
    let mut raw = Framed::ipc(writer);
    raw.send(&ping(5)).await.unwrap();

    let mut link = AgentLink::new(reader, 0, 3);
    let error = link.recv().await.unwrap_err();
    assert!(
        matches!(error, IpcError::Sequence(SeqError::NotStarted { got: 5 })),
        "{error:?}"
    );
    assert_eq!(
        link.inbound_expected(),
        1,
        "a rejected frame advances nothing"
    );
}

#[tokio::test]
async fn replays_and_gaps_are_distinguished_and_recoverable() {
    let (writer, reader) = duplex();
    let mut raw = Framed::ipc(writer);
    let mut link = AgentLink::new(reader, 0, 3);

    raw.send(&ping(1)).await.unwrap();
    assert_eq!(link.recv().await.unwrap().seq, 1);
    assert_eq!(link.inbound_expected(), 2);

    raw.send(&ping(1)).await.unwrap();
    let error = link.recv().await.unwrap_err();
    assert!(
        matches!(
            error,
            IpcError::Sequence(SeqError::Replay {
                expected: 2,
                got: 1
            })
        ),
        "{error:?}"
    );
    assert_eq!(link.inbound_expected(), 2);

    raw.send(&ping(4)).await.unwrap();
    let error = link.recv().await.unwrap_err();
    assert!(
        matches!(
            error,
            IpcError::Sequence(SeqError::Gap {
                expected: 2,
                got: 4
            })
        ),
        "{error:?}"
    );
    assert_eq!(link.inbound_expected(), 2);

    // The expected number is a strict state, not a watermark: the frame that was
    // due is still accepted.
    raw.send(&ping(2)).await.unwrap();
    assert_eq!(link.recv().await.unwrap().seq, 2);
    assert_eq!(link.inbound_expected(), 3);
}

#[tokio::test]
async fn a_frame_major_mismatch_is_a_protocol_error() {
    let (writer, reader) = duplex();
    let mut raw = Framed::ipc(writer);
    let mut foreign = ping(1);
    foreign.major = 1;
    raw.send(&foreign).await.unwrap();

    let mut link = AgentLink::new(reader, 0, 3);
    let error = link.recv().await.unwrap_err();
    assert!(matches!(error, IpcError::Protocol { .. }), "{error:?}");
    assert_eq!(
        link.inbound_expected(),
        1,
        "the version is checked before seq, so seq did not advance"
    );
}

#[tokio::test]
async fn a_rejected_prefix_is_never_consumed_and_resynchronisation_is_impossible() {
    // An invalid length prefix cannot be skipped: the stream has no frame
    // boundaries to resynchronise on, so the channel must be closed — which is
    // what the documents prescribe for a broken channel.
    let (mut writer, reader) = duplex();
    let mut link = AgentLink::new(reader, 0, 3);

    writer.write_all(&65_537u32.to_be_bytes()).await.unwrap();
    writer.flush().await.unwrap();

    for attempt in 0..2 {
        let error = link.recv().await.unwrap_err();
        assert!(matches!(error, IpcError::Framing(_)), "{error:?}");
        assert_eq!(link.inbound_expected(), 1, "nothing was ever accepted");
        assert_eq!(
            link.framed().buffered_len(),
            4,
            "attempt {attempt}: the rejected prefix stays exactly where it is"
        );
    }

    // Even a well-formed frame behind it does not help: the prefix is still first.
    let frame = protocol::frame::encode_message(protocol::Channel::Ipc, &ping(1)).unwrap();
    writer.write_all(&frame).await.unwrap();
    writer.flush().await.unwrap();
    let error = link.recv().await.unwrap_err();
    assert!(matches!(error, IpcError::Framing(_)), "{error:?}");
    assert_eq!(link.inbound_expected(), 1);
}

#[tokio::test]
async fn gui_frames_stamp_seq_and_pass_the_request_id_through() {
    let (core_io, gui_io) = duplex();
    let mut core = GuiLink::new(core_io);
    let mut gui = GuiLink::new(gui_io);

    assert_eq!(core.outbound_next(), 1);
    core.send(7, gui_frame::Msg::GetStatus(GetStatus {}))
        .await
        .unwrap();
    assert_eq!(core.outbound_next(), 2);

    let frame = gui.recv().await.unwrap();
    assert_eq!(frame.seq, 1);
    assert_eq!(frame.request_id, 7);
    assert_eq!(gui.inbound_expected(), 2);

    // Events carry request_id 0 and are not confused with responses.
    gui.send(0, gui_frame::Msg::GetStatus(GetStatus {}))
        .await
        .unwrap();
    assert_eq!(core.recv().await.unwrap().request_id, 0);
}

#[tokio::test]
async fn gui_seq_violations_are_reported_as_sequence_errors() {
    let (writer, reader) = duplex();
    let mut raw = Framed::ipc(writer);
    raw.send(&GuiFrame {
        seq: 5,
        request_id: 0,
        msg: Some(gui_frame::Msg::GetStatus(GetStatus {})),
    })
    .await
    .unwrap();

    let mut gui = GuiLink::new(reader);
    let error = gui.recv().await.unwrap_err();
    assert!(
        matches!(error, IpcError::Sequence(SeqError::NotStarted { got: 5 })),
        "{error:?}"
    );
}
