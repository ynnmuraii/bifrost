mod common;
use common::*;
use focus_router::*;

fn reports(outputs: &[Output]) -> Vec<LeaseDiagnosticSnapshot> {
    outputs
        .iter()
        .filter_map(|o| match o {
            Output::LeaseDiagnostic(s) => Some(*s),
            _ => None,
        })
        .collect()
}

#[test]
fn source_counts_status_acceptance_and_reports_once_at_expiry() {
    let mut r = remote();
    let ttl = r.timing().lease_ttl_ms;
    let status = Message::LeaseStatus {
        lease_id: id(1),
        nonce: N,
        accepted: true,
        last_input_batch_seq: 0,
    };
    r.handle(20, net(status.clone()));
    r.handle(21, net(status));
    let tick = r.handle(1014, Input::TimerExpired(Timer::LeaseDiagnostic));
    let s = reports(&tick)[0];
    assert!(!s.final_snapshot);
    assert_eq!(s.role, LeaseRole::Source);
    assert_eq!(s.counters.challenge_received, 1);
    assert_eq!(s.counters.renew_sent, 1);
    assert_eq!(s.counters.status_received, 2);
    assert_eq!(s.counters.status_accepted, 1);
    assert_eq!(s.counters.status_rejected, 1);
    assert_eq!(s.ages.challenge_ms, Some(1001));
    assert_eq!(s.ages.status_ms, Some(993));
    let expiry_ms = (20 + ttl).max(1015);
    let expired = r.handle(expiry_ms, Input::TimerExpired(Timer::SourceLeaseWatchdog));
    let final_s = reports(&expired);
    assert_eq!(final_s.len(), 1);
    assert_eq!(final_s[0].remaining_ms, 0);
    assert!(final_s[0].final_snapshot);
    assert_eq!(final_s[0].counters, s.counters);
    assert_eq!(final_s[0].ages.status_ms, Some(expiry_ms - 21));
    assert!(matches!(expired.first(), Some(Output::LeaseDiagnostic(_))));
    assert!(reports(&r.handle(
        expiry_ms + 1,
        Input::TimerExpired(Timer::SourceLeaseWatchdog)
    ))
    .is_empty());
    assert!(
        reports(&r.handle(expiry_ms + 2, Input::TimerExpired(Timer::LeaseDiagnostic))).is_empty()
    );
}

#[test]
fn receiver_counts_accepted_and_replayed_renewals() {
    let mut r = active();
    let ttl = r.timing().lease_ttl_ms;
    let nonce = r.lease().receiver().unwrap().challenges[0].0;
    r.handle(20, net(Message::LeaseRenew { lease_id: L, nonce }));
    r.handle(21, net(Message::LeaseRenew { lease_id: L, nonce }));
    let deadline = r.lease().receiver().unwrap().deadline_ms;
    let expired = r.handle(deadline, Input::TimerExpired(Timer::ReceiverLeaseDeadline));
    let s = reports(&expired)[0];
    assert_eq!(s.lease_id, L);
    assert_eq!(s.peer, PEER);
    assert_eq!(s.role, LeaseRole::Receiver);
    assert_eq!(s.ttl_ms, ttl);
    assert_eq!(s.remaining_ms, 0);
    assert_eq!(s.counters.challenge_sent, 1);
    assert_eq!(s.counters.renew_received, 2);
    assert_eq!(s.counters.status_sent, 2);
    assert_eq!(s.counters.renew_accepted, 1);
    assert_eq!(s.counters.renew_rejected, 1);
    assert_eq!(s.ages.renew_ms, Some(deadline - 21));
    assert!(s.final_snapshot);
    assert!(matches!(expired.first(), Some(Output::LeaseDiagnostic(_))));
    assert!(reports(&r.handle(
        deadline + 1,
        Input::TimerExpired(Timer::ReceiverLeaseDeadline)
    ))
    .is_empty());
}
