mod support;

use std::time::{Duration, Instant};

use lws_gui::view_model::Request;
use support::view;

#[test]
fn audit_pairing_disconnect_and_unsent_allow_retry() {
    let mut vm = view();
    vm.on_connected("test");
    vm.pairing.dialing = true;
    vm.pairing.window_deadline = Some(Instant::now() + Duration::from_secs(120));
    vm.on_disconnected("core stopped".into());
    assert!(!vm.pairing.dialing);
    assert!(vm.pairing.window_deadline.is_none());
    vm.on_connected("test");
    vm.pairing.dialing = true;
    let id = vm.begin_request(Request::PairWith, Instant::now());
    vm.on_not_sent(id);
    assert!(!vm.pairing.dialing);
}
