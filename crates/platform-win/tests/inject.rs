#![cfg(windows)]

use platform_win::inject::Injector;

#[test]
fn releasing_empty_injector_is_complete_and_idempotent() {
    let mut injector = Injector::new();
    for _ in 0..2 {
        // The empty-ledger branch returns before querying the desktop or
        // calling SendInput, so this public operation is hermetic.
        let status = injector.release_all();
        assert!(status.is_complete());
        assert!(!status.is_total_failure());
        assert_eq!(status.requested, 0);
        assert_eq!(status.applied, 0);
        assert!(status.not_injected.is_empty());
        assert_eq!(status.accepted_records, 0);
        assert_eq!(status.total_records, 0);
        assert_eq!(status.error, None);
        assert!(injector.ledger().is_empty());
    }
}
