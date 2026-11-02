use audio_model::{
    media::{Refusal, Relay, Scope},
    session::{Parent, Phase, SessionView},
    status::{MediaCommand, MediaCommands, MediaInfo},
};

fn scope() -> Scope {
    Scope {
        view: SessionView {
            parent: Parent {
                peer: [1; 32],
                connection_id: [2; 16],
                generation: [3; 16],
            },
            session: [4; 16],
            worker_generation: [5; 16],
            phase: Phase::Active,
            reason: None,
        },
        current_parent: true,
        consented: true,
        capable: true,
        worker_generation: [5; 16],
    }
}
fn info() -> MediaInfo {
    MediaInfo {
        session_token: 7,
        revision: 8,
        commands: MediaCommands {
            play: true,
            ..Default::default()
        },
        ..Default::default()
    }
}
fn populated() -> Relay {
    let mut relay = Relay::default();
    let s = scope();
    relay.scope(Some(s));
    assert!(relay.update(s.view.parent, s.view.session, 1, Some(info())));
    relay
}
#[test]
fn authorization_requires_every_live_session_gate() {
    for gate in 0..5 {
        let mut s = scope();
        match gate {
            0 => s.current_parent = false,
            1 => s.consented = false,
            2 => s.capable = false,
            3 => s.view.phase = Phase::Rebuffering,
            _ => s.worker_generation = [9; 16],
        }
        let mut relay = populated();
        relay.scope(Some(s));
        assert!(relay.info().is_none());
        assert!(!relay.update(s.view.parent, s.view.session, 2, Some(info())));
        assert_eq!(
            relay.authorize(true, 7, 8, MediaCommand::Play),
            Err(Refusal::Inactive)
        );
    }
}
#[test]
fn command_requires_permission_exact_token_revision_and_native_command() {
    let relay = populated();
    assert_eq!(relay.authorize(true, 7, 8, MediaCommand::Play), Ok(()));
    assert_eq!(
        relay.authorize(false, 7, 8, MediaCommand::Play),
        Err(Refusal::ConsentRequired)
    );
    assert_eq!(
        relay.authorize(true, 9, 8, MediaCommand::Play),
        Err(Refusal::StaleRevision)
    );
    assert_eq!(
        relay.authorize(true, 7, 9, MediaCommand::Play),
        Err(Refusal::StaleRevision)
    );
    assert_eq!(
        relay.authorize(true, 7, 8, MediaCommand::Next),
        Err(Refusal::Unsupported)
    );
}
#[test]
fn stop_replacement_empty_and_old_snapshots_do_not_resurrect_facts() {
    let mut relay = populated();
    let s = scope();
    assert!(!relay.update(s.view.parent, s.view.session, 1, Some(info())));
    assert!(relay.update(s.view.parent, s.view.session, 2, None));
    assert!(relay.info().is_none());
    assert!(!relay.update(s.view.parent, s.view.session, 1, Some(info())));
    relay.scope(None);
    assert!(!relay.update(s.view.parent, s.view.session, 3, Some(info())));
    let mut replacement = s;
    replacement.view.parent.generation = [8; 16];
    relay.scope(Some(replacement));
    assert!(!relay.update(s.view.parent, s.view.session, 4, Some(info())));
    assert!(!relay.update(replacement.view.parent, [9; 16], 4, Some(info())));
}
#[test]
fn malformed_metadata_is_not_retained() {
    let mut relay = populated();
    let s = scope();
    let mut bad = info();
    bad.title = "x".repeat(257);
    assert!(!relay.update(s.view.parent, s.view.session, 2, Some(bad)));
    let mut bad = info();
    bad.session_token = 0;
    assert!(!relay.update(s.view.parent, s.view.session, 2, Some(bad)));
}

#[test]
fn seek_preserves_permission_identity_and_native_range_guards() {
    let mut relay = Relay::default();
    let scope = scope();
    relay.scope(Some(scope));
    let mut media = info();
    media.commands.seek = true;
    media.position_ms = Some(20);
    media.duration_ms = Some(100);
    media.seek_min_ms = Some(0);
    media.seek_max_ms = Some(100);
    assert!(relay.update(
        scope.view.parent,
        scope.view.session,
        1,
        Some(media.clone())
    ));
    for position_ms in [0, 100] {
        let command = MediaCommand::SeekTo { position_ms };
        assert_eq!(relay.authorize(true, 7, 8, command), Ok(()));
        assert_eq!(
            relay.authorize(false, 7, 8, command),
            Err(Refusal::ConsentRequired)
        );
        assert_eq!(
            relay.authorize(true, 0, 8, command),
            Err(Refusal::StaleRevision)
        );
        assert_eq!(
            relay.authorize(true, 7, 9, command),
            Err(Refusal::StaleRevision)
        );
    }
    assert_eq!(
        relay.authorize(true, 7, 8, MediaCommand::SeekTo { position_ms: 101 }),
        Err(Refusal::Unsupported)
    );
    media.seek_min_ms = Some(10);
    assert!(relay.update(scope.view.parent, scope.view.session, 2, Some(media)));
    assert_eq!(
        relay.authorize(true, 7, 8, MediaCommand::SeekTo { position_ms: 0 }),
        Err(Refusal::Unsupported)
    );
    let mut replacement = scope;
    replacement.worker_generation = [9; 16];
    relay.scope(Some(replacement));
    assert_eq!(
        relay.authorize(true, 7, 8, MediaCommand::SeekTo { position_ms: 20 }),
        Err(Refusal::Inactive)
    );
}
