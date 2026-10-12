//! The agent-contract rules that need no endpoint: the launch token, the frame
//! version, the first-frame requirement and the `RECOVERY` allow-list.

use std::collections::BTreeSet;

use ipc::agent;
use ipc::IpcError;
use protocol::ipc::{
    agent_frame, AgentAttach, AgentMode, AgentShutdown, BeginCapture, CaptureKind, EndCapture,
    ForgetPortalGrants, Inject, LocalEvent, Ping, PrepareInject, QueryDisplays, ReleaseKeys,
    ReleaseSession,
};

const TOKEN: [u8; 32] = [0x42; 32];

#[test]
fn a_launch_token_is_compared_in_full_and_never_echoed() {
    assert!(agent::tokens_match(&TOKEN, &TOKEN));
    assert_eq!(agent::LAUNCH_TOKEN_LEN, 32);

    // A difference at either end of the 32 bytes must be found.
    let mut first = TOKEN;
    first[0] ^= 0xFF;
    let mut last = TOKEN;
    last[31] ^= 0xFF;
    assert!(!agent::tokens_match(&TOKEN, &first));
    assert!(!agent::tokens_match(&TOKEN, &last));

    // A prefix of the right token is not the token.
    assert!(!agent::tokens_match(&TOKEN, &TOKEN[..31]));
    assert!(!agent::tokens_match(&TOKEN, &[0u8; 33]));

    assert!(agent::verify_launch_token(&TOKEN, &TOKEN).is_ok());
    let error = agent::verify_launch_token(&TOKEN, &first).unwrap_err();
    assert!(matches!(error, IpcError::Protocol { .. }), "{error:?}");
    // The failure is reported without the secret: the documents keep the token
    // out of logs and diagnostics.
    let rendered = String::from_utf8_lossy(&TOKEN).into_owned();
    assert!(
        !error.to_string().contains(&rendered),
        "the error text must not leak the token: {error}"
    );

    // The expected token itself is validated: a wrong-length secret is a
    // configuration mistake, not a peer problem.
    let error = agent::verify_launch_token(&TOKEN[..16], &TOKEN).unwrap_err();
    assert!(matches!(error, IpcError::Config { .. }), "{error:?}");
}

#[test]
fn only_a_matching_frame_major_is_accepted() {
    assert!(agent::check_frame_version(0, 0).is_ok());
    assert!(agent::check_frame_version(3, 3).is_ok());

    let error = agent::check_frame_version(1, 0).unwrap_err();
    assert!(matches!(error, IpcError::Protocol { .. }), "{error:?}");
    assert!(error.to_string().contains("major"), "{error}");
}

#[test]
fn the_first_frame_must_be_agent_attach() {
    let attach = AgentAttach {
        launch_token: TOKEN.to_vec(),
        mode: AgentMode::Normal as i32,
        core_silence_ms: 300,
        ..Default::default()
    };
    let message = agent_frame::Msg::Attach(attach.clone());
    assert_eq!(agent::expect_attach(&message).unwrap().core_silence_ms, 300);

    let error = agent::expect_attach(&agent_frame::Msg::Ping(Ping { nonce: 1 })).unwrap_err();
    assert!(matches!(error, IpcError::Protocol { .. }), "{error:?}");
}

#[test]
fn recovery_accepts_only_release_keys_and_shutdown() {
    let messages = || -> Vec<agent_frame::Msg> {
        vec![
            agent_frame::Msg::Attach(AgentAttach::default()),
            agent_frame::Msg::BeginCapture(BeginCapture {
                kind: CaptureKind::Remote as i32,
                lease_id: vec![0u8; 16],
                self_timeout_ms: 0,
                dev_capture_injected: false,
            }),
            agent_frame::Msg::EndCapture(EndCapture::default()),
            agent_frame::Msg::PrepareInject(PrepareInject::default()),
            agent_frame::Msg::Inject(Inject {
                lease_id: vec![0u8; 16],
                input_batch_seq: 1,
                events: vec![LocalEvent::default()],
            }),
            agent_frame::Msg::ReleaseSession(ReleaseSession::default()),
            agent_frame::Msg::ReleaseKeys(ReleaseKeys::default()),
            agent_frame::Msg::QueryDisplays(QueryDisplays {}),
            agent_frame::Msg::Shutdown(AgentShutdown {}),
            agent_frame::Msg::Ping(Ping { nonce: 1 }),
            agent_frame::Msg::ForgetPortalGrants(ForgetPortalGrants {}),
        ]
    };

    let allowed: BTreeSet<&str> = messages()
        .iter()
        .filter(|message| agent::mode_allows(AgentMode::Recovery as i32, message))
        .map(variant_name)
        .collect();
    assert_eq!(
        allowed,
        BTreeSet::from(["ReleaseKeys", "Shutdown"]),
        "rule 7: RECOVERY accepts exactly ReleaseKeys and AgentShutdown"
    );

    // NORMAL accepts everything, including capture and inject.
    for message in messages() {
        assert!(
            agent::mode_allows(AgentMode::Normal as i32, &message),
            "NORMAL must accept {}",
            variant_name(&message)
        );
    }

    // An unknown or unspecified mode is not a reason to refuse work.
    for mode in [AgentMode::Unspecified as i32, 99] {
        assert!(agent::mode_allows(
            mode,
            &agent_frame::Msg::Inject(Inject::default())
        ));
    }

    // The predicates agree with each other.
    assert!(agent::recovery_allows(&agent_frame::Msg::ReleaseKeys(
        ReleaseKeys::default()
    )));
    assert!(agent::recovery_allows(&agent_frame::Msg::Shutdown(
        AgentShutdown {}
    )));
    assert!(!agent::recovery_allows(&agent_frame::Msg::Ping(Ping {
        nonce: 1
    })));
}

/// A stable name for a frame variant, so the assertion above names what it saw.
fn variant_name(message: &agent_frame::Msg) -> &'static str {
    match message {
        agent_frame::Msg::Attach(_) => "Attach",
        agent_frame::Msg::BeginCapture(_) => "BeginCapture",
        agent_frame::Msg::EndCapture(_) => "EndCapture",
        agent_frame::Msg::PrepareInject(_) => "PrepareInject",
        agent_frame::Msg::Inject(_) => "Inject",
        agent_frame::Msg::ArmDeadline(_) => "ArmDeadline",
        agent_frame::Msg::ReleaseSession(_) => "ReleaseSession",
        agent_frame::Msg::ReleaseKeys(_) => "ReleaseKeys",
        agent_frame::Msg::QueryDisplays(_) => "QueryDisplays",
        agent_frame::Msg::Shutdown(_) => "Shutdown",
        agent_frame::Msg::Ping(_) => "Ping",
        agent_frame::Msg::ForgetPortalGrants(_) => "ForgetPortalGrants",
        _ => "other",
    }
}
