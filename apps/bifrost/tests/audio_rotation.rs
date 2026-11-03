//! Real isolated paired cores and the real Dry worker; no installed app or native device.
#![cfg(all(feature = "audio", debug_assertions))]
mod common;
use common::{
    audio::{self, Window},
    Unpaired,
};
use protocol::ipc::{self as wire, gui_frame};
use std::{
    thread,
    time::{Duration, Instant},
};
#[test]
fn real_cores_rotate_keys_in_place_cross_old_deadline_and_deliver_real_thumbnail() {
    let _guard = common::serial();
    let two = Unpaired::start_audio_no_input("rk");
    two.wait_ready();
    let mut listener = Window::open(&two.a);
    let mut source = Window::open(&two.b);
    audio::pair(&two, &mut listener, &mut source);
    let listener_id = audio::node_id(&two.a_id);
    let source_id = audio::node_id(&two.b_id);
    let ports = audio::ports(2);
    listener.configure(audio::config(
        wire::AudioRole::Listener,
        ports[0],
        &[],
        std::slice::from_ref(&source_id),
        true,
    ));
    source.configure(audio::config(
        wire::AudioRole::Source,
        ports[1],
        &listener_id,
        std::slice::from_ref(&listener_id),
        true,
    ));
    source.wait("Source resolves Listener", |status| {
        status
            .config
            .as_ref()
            .is_some_and(|config| config.group_id.len() == 16)
    });
    source.start(&listener_id);
    let session = audio::healthy(&mut listener, &mut source, &listener_id, &source_id);
    let initial = listener.wait("complete real artwork cache is advertised", |status| {
        status.sessions.iter().any(|stream| {
            stream.session_id == session
                && stream
                    .media_info
                    .as_ref()
                    .is_some_and(|info| info.artwork_id.len() == 32)
        })
    });
    let media = initial
        .sessions
        .iter()
        .find(|stream| stream.session_id == session)
        .unwrap()
        .media_info
        .as_ref()
        .unwrap()
        .clone();
    let request = wire::GetAudioArtwork {
        session_id: session.clone(),
        session_token: media.session_token,
        artwork_id: media.artwork_id.clone(),
    };
    match listener.request(gui_frame::Msg::GetAudioArtwork(request.clone())) {
        gui_frame::Msg::AudioArtwork(art) => {
            assert_eq!(art.artwork_id, media.artwork_id);
            assert_eq!(art.mime, "image/jpeg");
            assert!(!art.data.is_empty() && art.data.len() <= 49152);
            assert_eq!(&art.data[..2], &[255, 216]);
            assert!((1..=192).contains(&art.width) && (1..=192).contains(&art.height));
        }
        other => panic!("artwork reply {other:?}"),
    }
    for offset in [1_680_000, 1_740_000] {
        for root in [&two.a.root, &two.b.root] {
            std::fs::write(
                root.join("run/test-audio-clock-offset-ms"),
                offset.to_string(),
            )
            .unwrap();
        }
        if offset == 1_680_000 {
            thread::sleep(Duration::from_millis(150));
        }
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let rotated = [&two.a, &two.b].iter().all(|node| {
            std::fs::read_to_string(&node.log)
                .unwrap_or_default()
                .contains("audio media keys rotated in place")
        });
        if rotated {
            break;
        }
        let a = listener.status();
        let b = source.status();
        for status in [&a, &b] {
            assert!(
                status
                    .sessions
                    .iter()
                    .any(|stream| stream.session_id == session
                        && stream.state == wire::AudioSessionState::Running as i32),
                "rotation changed live session: {status:?}"
            );
        }
        assert!(
            Instant::now() < deadline,
            "workers did not complete key handoff"
        );
        thread::sleep(Duration::from_millis(25));
    }
    for root in [&two.a.root, &two.b.root] {
        std::fs::write(root.join("run/test-audio-clock-offset-ms"), "1800000").unwrap();
    }
    thread::sleep(Duration::from_millis(300));
    assert_eq!(
        audio::healthy(&mut listener, &mut source, &listener_id, &source_id),
        session
    );
    let after = listener.status();
    let after_media = after
        .sessions
        .iter()
        .find(|stream| stream.session_id == session)
        .unwrap()
        .media_info
        .as_ref()
        .unwrap();
    assert_eq!(after_media.session_token, media.session_token);
    assert_eq!(after_media.artwork_id, media.artwork_id);
    source.stop(&listener_id, &session);
    source.stopped(&session);
    listener.stopped(&session);
    match listener.request(gui_frame::Msg::GetAudioArtwork(request)) {
        gui_frame::Msg::OpResult(result) => assert!(!result.ok),
        other => panic!("revoked artwork remained visible: {other:?}"),
    }
    source.closed();
    listener.closed();
}
