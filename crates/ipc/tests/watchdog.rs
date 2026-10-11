//! Watchdog arithmetic, driven entirely by explicit `now_ms` values.
//!
//! Every boundary the documents name is checked here: the `core_silence_ms`
//! range 200..500 with default 300, the derived heartbeat `core_silence_ms / 4`
//! (≤ 125 ms), "exactly the limit is still alive", and the two independent
//! schedules — silence resets on any received frame, the heartbeat period does
//! not.

use ipc::{IpcError, Watchdog};

#[test]
fn core_silence_range_is_enforced_and_the_heartbeat_is_derived() {
    for outside in [0, 1, 100, 199, 501, 1_000] {
        let error = Watchdog::for_core_silence_ms(outside, 0).unwrap_err();
        assert!(
            matches!(error, IpcError::Config { .. }),
            "core_silence_ms {outside} must be refused, got {error:?}"
        );
    }

    for inside in [200, 300, 400, 500] {
        let watchdog = Watchdog::for_core_silence_ms(inside, 0).unwrap();
        assert_eq!(watchdog.silence_limit_ms(), u64::from(inside));
        assert_eq!(watchdog.heartbeat_ms(), u64::from(inside) / 4);
        assert!(
            watchdog.heartbeat_ms() <= 125,
            "the derived heartbeat must stay within the documented 125 ms"
        );
    }

    let defaults = Watchdog::for_core_silence_ms(ipc::DEFAULT_CORE_SILENCE_MS, 0).unwrap();
    assert_eq!(defaults.heartbeat_ms(), 75);
    assert_eq!(defaults.silence_limit_ms(), 300);
}

#[test]
fn exactly_the_limit_is_still_alive() {
    let watchdog = Watchdog::for_core_silence_ms(300, 1_000).unwrap();

    assert_eq!(watchdog.silence_ms(1_000), 0);
    assert_eq!(watchdog.silence_ms(1_300), 300);
    assert!(!watchdog.silence_exceeded(1_300));
    assert_eq!(watchdog.remaining_ms(1_300), 0);
    assert!(watchdog.check(1_300).is_ok());

    assert!(watchdog.silence_exceeded(1_301));
    let error = watchdog.check(1_301).unwrap_err();
    assert!(
        matches!(
            error,
            IpcError::Watchdog {
                silence_ms: 301,
                limit_ms: 300
            }
        ),
        "{error:?}"
    );
}

#[test]
fn any_frame_resets_silence_but_not_the_heartbeat_period() {
    let mut watchdog = Watchdog::for_core_silence_ms(300, 0).unwrap();

    // Silence grows with the clock and can never exceed it.
    assert_eq!(watchdog.silence_ms(100), 100);
    assert_eq!(watchdog.remaining_ms(100), 200);

    // «нет ни одного кадра, включая IPC Ping»: any frame counts, not just Pong.
    watchdog.on_frame(250);
    assert_eq!(watchdog.silence_ms(300), 50);
    assert!(!watchdog.silence_exceeded(300));
    assert_eq!(watchdog.remaining_ms(300), 250);
    assert!(watchdog.check(550).is_ok());
    assert!(watchdog.silence_exceeded(551));

    // The heartbeat schedule is periodic and independent of received frames.
    assert!(watchdog.heartbeat_due(75));
    watchdog.on_frame(75);
    assert!(
        watchdog.heartbeat_due(75),
        "receiving a frame must not postpone the heartbeat"
    );
    watchdog.on_heartbeat(80);
    assert!(!watchdog.heartbeat_due(80));
    assert!(!watchdog.heartbeat_due(154));
    assert!(watchdog.heartbeat_due(155));
    assert_eq!(watchdog.next_heartbeat_in_ms(80), 75);
    assert_eq!(watchdog.next_heartbeat_in_ms(100), 55);
    assert_eq!(watchdog.next_heartbeat_in_ms(200), 0);
}

#[test]
fn explicit_periods_are_validated_and_saturate() {
    assert!(Watchdog::with_periods(0, 300, 0).is_err());
    assert!(Watchdog::with_periods(300, 299, 0).is_err());
    assert!(Watchdog::with_periods(300, 300, 0).is_ok());

    // The network heartbeat (1 s / 3 s) is a different pair of periods.
    let network = Watchdog::with_periods(1_000, 3_000, 0).unwrap();
    assert_eq!(network.heartbeat_ms(), 1_000);
    assert_eq!(network.silence_limit_ms(), 3_000);
    assert!(!network.silence_exceeded(3_000));
    assert!(network.silence_exceeded(3_001));

    // A monotonic clock that jumps backwards must not panic or wrap.
    let watchdog = Watchdog::for_core_silence_ms(200, 5_000).unwrap();
    assert_eq!(watchdog.silence_ms(0), 0);
    assert_eq!(watchdog.remaining_ms(0), 200);
    assert_eq!(watchdog.silence_ms(u64::MAX), u64::MAX - 5_000);
}
