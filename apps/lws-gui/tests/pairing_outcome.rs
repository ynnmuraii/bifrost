mod support;

use std::time::Instant;

use lws_gui::view_model::Request;
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
            vm.outcome.as_ref().is_some_and(|result| result.ok),
            outcome == PairingOutcome::Paired
        );
        assert!(vm.pairing.prompt.is_none());
        assert!(!vm.pairing.dialing);
    }
}
