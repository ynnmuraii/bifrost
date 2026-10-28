mod support;

use std::time::{Duration, Instant};

use lws_gui::view_model::Request;
use support::view;

#[test]
fn audit_pairing_wait_does_not_report_transport_silence() {
    for request in [Request::PairWith, Request::PairingAnswer] {
        let mut vm = view();
        vm.on_connected("test");
        let now = Instant::now();
        vm.begin_request(request, now);
        vm.tick(now + Duration::from_secs(4));
        assert!(!vm.silent, "{request:?} can wait for human confirmation");
        vm.tick(now + Duration::from_secs(119));
        assert!(!vm.silent);
    }
}

