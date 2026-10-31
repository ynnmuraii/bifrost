mod support;

use std::time::Instant;

use bifrost_gui::view_model::Request;
use protocol::ipc::gui_frame::Msg;
use support::{answer, event, pairing_prompt, view};

#[test]
fn audit_unstructured_pairing_ack_is_not_trust_confirmation() {
    let mut vm = view();
    vm.apply(event(Msg::PairingPrompt(pairing_prompt())));
    let id = vm.begin_request(Request::PairingAnswer, Instant::now());
    vm.apply(answer(
        id,
        Msg::OpResult(protocol::ipc::OpResult {
            ok: true,
            error_code: String::new(),
        }),
    ));
    assert!(
        vm.pairing.paired.is_none(),
        "a processed rejection is not pairing"
    );
    assert!(
        vm.outcome.is_none(),
        "no generic success for pairing answers"
    );
    assert!(!vm.log.iter().any(|line| line.contains("сопряжён")));
}

#[test]
fn audit_only_a_committed_pairing_outcome_shows_success() {
    use protocol::ipc::{PairingOutcome, PairingResult};
    for outcome in [
        PairingOutcome::Paired,
        PairingOutcome::Rejected,
        PairingOutcome::Failed,
        PairingOutcome::Unspecified,
    ] {
        let mut vm = view();
        vm.apply(event(Msg::PairingPrompt(pairing_prompt())));
        let id = vm.begin_request(Request::PairingAnswer, Instant::now());
        let result = PairingResult {
            outcome: outcome as i32,
            peer_name: "LAPTOP".into(),
            ..Default::default()
        };
        let bytes = protocol::encode_message(protocol::Channel::Ipc, &result).unwrap();
        let decoded = protocol::decode_message::<PairingResult>(protocol::Channel::Ipc, &bytes)
            .unwrap()
            .unwrap();
        vm.apply(event(Msg::PairingResult(decoded)));
        vm.apply(answer(
            id,
            Msg::OpResult(protocol::ipc::OpResult {
                ok: outcome != PairingOutcome::Failed,
                error_code: String::new(),
            }),
        ));
        assert_eq!(
            vm.pairing.paired.as_deref(),
            (outcome == PairingOutcome::Paired).then_some("LAPTOP")
        );
        assert_eq!(
            vm.outcome
                .as_ref()
                .is_some_and(|result| result.is_success()),
            outcome == PairingOutcome::Paired
        );
        assert!(vm.pairing.prompt.is_none());
        assert!(!vm.pairing.dialing);
    }
}

#[test]
fn matching_and_legacy_results_survive_the_answer_ack_removing_the_prompt() {
    for ceremony_id in [Vec::new(), vec![0x42; 16]] {
        let mut vm = view();
        vm.apply(event(Msg::PairingPrompt(pairing_prompt())));
        let id = vm.begin_request(Request::PairingAnswer, Instant::now());
        vm.apply(answer(
            id,
            Msg::OpResult(protocol::ipc::OpResult {
                ok: true,
                ..Default::default()
            }),
        ));
        assert!(vm.pairing.prompt.is_none());
        assert_eq!(vm.pairing.ceremony_id, Some([0x42; 16]));
        vm.apply(event(Msg::PairingResult(protocol::ipc::PairingResult {
            outcome: protocol::ipc::PairingOutcome::Paired as i32,
            peer_name: "LAPTOP".into(),
            ceremony_id,
            ..Default::default()
        })));
        assert_eq!(vm.pairing.paired.as_deref(), Some("LAPTOP"));
        assert!(vm.outcome.as_ref().unwrap().is_success());
    }
}

#[test]
fn stale_results_and_answer_acks_never_clear_or_retire_a_newer_prompt() {
    for outcome in [
        protocol::ipc::PairingOutcome::Paired,
        protocol::ipc::PairingOutcome::Rejected,
        protocol::ipc::PairingOutcome::Failed,
    ] {
        let mut vm = view();
        vm.apply(event(Msg::PairingPrompt(pairing_prompt())));
        let old_answer = vm.begin_request(Request::PairingAnswer, Instant::now());
        let mut newer = pairing_prompt();
        newer.attempt_id = vec![0x43; 16];
        vm.apply(event(Msg::PairingPrompt(newer)));
        let current_answer = vm.begin_request(Request::PairingAnswer, Instant::now());
        let before = vm.pairing.clone();
        vm.apply(answer(
            current_answer,
            Msg::PairingResult(protocol::ipc::PairingResult {
                outcome: outcome as i32,
                peer_name: "OLD".into(),
                ceremony_id: vec![0x42; 16],
                error_code: "PAIRING_CANCELLED".into(),
            }),
        ));
        assert_eq!(vm.pairing, before);
        assert!(vm
            .inflight
            .iter()
            .any(|request| request.id == current_answer));
        assert!(vm.outcome.is_none());
        for ok in [true, false] {
            vm.apply(answer(
                old_answer,
                Msg::OpResult(protocol::ipc::OpResult {
                    ok,
                    error_code: "PAIRING_CANCELLED".into(),
                }),
            ));
            assert_eq!(vm.pairing, before);
            assert!(vm.outcome.is_none());
        }
    }
}

#[test]
fn new_dials_and_accepted_pair_requests_fence_the_previous_visible_flow() {
    use bifrost_gui::view_model::Action;
    for action in [
        Action::PairWith("127.0.0.1:47901".into()),
        Action::RequestPairing(protocol::ipc::RequestPairing {
            address: "127.0.0.1:47901".into(),
            ..Default::default()
        }),
        Action::AnswerPairingRequest {
            requester_node_id: vec![0x44; 32],
            accept: true,
        },
    ] {
        let mut vm = view();
        vm.apply(event(Msg::PairingPrompt(pairing_prompt())));
        let old_answer = vm.begin_request(Request::PairingAnswer, Instant::now());
        vm.local(&action);
        assert!(vm.pairing.ceremony_id.is_none());
        assert!(vm.pairing.prompt.is_none());
        let before = vm.pairing.clone();
        vm.apply(event(Msg::PairingResult(protocol::ipc::PairingResult {
            outcome: protocol::ipc::PairingOutcome::Paired as i32,
            ceremony_id: vec![0x42; 16],
            ..Default::default()
        })));
        vm.apply(answer(
            old_answer,
            Msg::OpResult(protocol::ipc::OpResult {
                ok: false,
                error_code: "PAIRING_CANCELLED".into(),
            }),
        ));
        assert_eq!(vm.pairing, before);
        assert!(vm.outcome.is_none());
    }
}

#[test]
fn cancellation_results_and_pair_request_failures_use_the_localized_status() {
    for lang in [ui_text::Lang::En, ui_text::Lang::Ru] {
        for request in [None, Some(Request::PairingRequest)] {
            let mut vm = view();
            vm.look.language = lang;
            if let Some(request) = request {
                let id = vm.begin_request(request, Instant::now());
                vm.apply(answer(
                    id,
                    Msg::OpResult(protocol::ipc::OpResult {
                        ok: false,
                        error_code: "PAIRING_CANCELLED".into(),
                    }),
                ));
            } else {
                vm.apply(event(Msg::PairingPrompt(pairing_prompt())));
                vm.apply(event(Msg::PairingResult(protocol::ipc::PairingResult {
                    outcome: protocol::ipc::PairingOutcome::Rejected as i32,
                    ceremony_id: vec![0x42; 16],
                    error_code: "PAIRING_CANCELLED".into(),
                    ..Default::default()
                })));
            }
            assert!(vm.pairing.prompt.is_none());
            assert!(vm.pairing.paired.is_none());
            let status = vm.outcome.as_ref().unwrap();
            assert_eq!(status.kind, bifrost_gui::view_model::OutcomeKind::Info);
            assert_eq!(
                status.line(lang),
                ui_text::text(lang, ui_text::Key::PairingCancelled)
            );
            assert_eq!(
                status.text,
                ui_text::text(lang, ui_text::Key::PairingCancelled)
            );
        }
    }
}
