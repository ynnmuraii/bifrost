use audio_model::packet::PACKET_NUMBER_LIMIT;
use audio_model::replay::{ReplayError, ReplayWindow, REPLAY_WINDOW_SIZE};

#[test]
fn zero_is_valid_and_duplicates_are_rejected() {
    let mut window = ReplayWindow::new();
    assert_eq!(REPLAY_WINDOW_SIZE, 1024);
    assert_eq!(window.check(0), Ok(()));
    assert_eq!(window.commit(0), Ok(()));
    assert_eq!(window.check(0), Err(ReplayError::Duplicate));
    assert_eq!(window.commit(0), Err(ReplayError::Duplicate));
    assert_eq!(window.commit(1), Ok(()));
    assert_eq!(window.check(0), Err(ReplayError::Duplicate));
}

#[test]
fn reordered_packets_and_both_window_edges() {
    let mut window = ReplayWindow::default();
    assert_eq!(window.commit(2000), Ok(()));
    for number in [1999, 1936, 1937, 1024, 977] {
        assert_eq!(window.check(number), Ok(()));
        assert_eq!(window.commit(number), Ok(()));
        assert_eq!(window.check(number), Err(ReplayError::Duplicate));
    }
    assert_eq!(window.check(976), Err(ReplayError::TooOld));
    assert_eq!(window.commit(976), Err(ReplayError::TooOld));
    assert_eq!(window.commit(2001), Ok(()));
    assert_eq!(window.check(977), Err(ReplayError::TooOld));
    assert_eq!(window.check(978), Ok(()));
    assert_eq!(window.check(1999), Err(ReplayError::Duplicate));
}

#[test]
fn advances_preserve_bits_across_word_boundaries_and_clear_large_jumps() {
    let distances = [0, 1, 63, 64, 65, 127, 128, 511, 959, 960, 1022, 1023];
    for advance in [1, 2, 63, 64, 65, 127, 128, 129, 511, 960, 1023, 1024, 1025] {
        let mut window = ReplayWindow::new();
        let newest = 4096;
        for distance in distances {
            window.commit(newest - distance).unwrap();
        }
        window.commit(newest + advance).unwrap();
        for distance in distances {
            let expected = if distance + advance >= REPLAY_WINDOW_SIZE as u64 {
                Err(ReplayError::TooOld)
            } else {
                Err(ReplayError::Duplicate)
            };
            assert_eq!(window.check(newest - distance), expected);
        }
        // No shifted-out bit may reappear as a duplicate at the lower edge.
        let lower_edge_was_seen = 1023_u64
            .checked_sub(advance)
            .is_some_and(|distance| distances.contains(&distance));
        let expected = if lower_edge_was_seen {
            Err(ReplayError::Duplicate)
        } else {
            Ok(())
        };
        assert_eq!(window.check(newest + advance - 1023), expected);
    }
}

#[test]
fn unauthenticated_checks_do_not_change_any_state() {
    let mut window = ReplayWindow::new();
    assert_eq!(window.check(PACKET_NUMBER_LIMIT - 1), Ok(()));
    assert_eq!(window.check(0), Ok(()));
    window.commit(0).unwrap();
    window.commit(7).unwrap();
    for number in [PACKET_NUMBER_LIMIT - 1, 1_000_000, 1024, 8, 3] {
        assert_eq!(window.check(number), Ok(()));
    }
    assert_eq!(window.check(0), Err(ReplayError::Duplicate));
    assert_eq!(window.check(7), Err(ReplayError::Duplicate));
    assert_eq!(window.commit(3), Ok(()));
    assert_eq!(window.check(8), Ok(()));
}

#[test]
fn commit_rechecks_a_previously_successful_check() {
    let mut window = ReplayWindow::new();
    assert_eq!(window.check(1), Ok(()));
    window.commit(1).unwrap();
    assert_eq!(window.commit(1), Err(ReplayError::Duplicate));
    assert_eq!(window.check(2), Ok(()));
    window.commit(1026).unwrap();
    assert_eq!(window.commit(2), Err(ReplayError::TooOld));
    assert_eq!(window.check(1026), Err(ReplayError::Duplicate));
    assert_eq!(window.check(1025), Ok(()));
}

#[test]
fn rotation_limit_never_wraps_and_fresh_sessions_have_fresh_windows() {
    let mut window = ReplayWindow::new();
    let last = PACKET_NUMBER_LIMIT - 1;
    window.commit(last - 1).unwrap();
    window.commit(last).unwrap();
    assert_eq!(window.check(last), Err(ReplayError::Duplicate));
    assert_eq!(window.check(last - 1), Err(ReplayError::Duplicate));
    assert_eq!(window.commit(last - 1023), Ok(()));
    assert_eq!(window.check(last - 1024), Err(ReplayError::TooOld));
    assert_eq!(window.commit(0), Err(ReplayError::TooOld));
    for rejected in [PACKET_NUMBER_LIMIT, PACKET_NUMBER_LIMIT + 1, u64::MAX] {
        assert_eq!(window.check(rejected), Err(ReplayError::RotationRequired));
        assert_eq!(window.commit(rejected), Err(ReplayError::RotationRequired));
    }
    assert_eq!(window.check(last), Err(ReplayError::Duplicate));
    assert_eq!(ReplayWindow::new().commit(0), Ok(()));

    let mut huge_jump = ReplayWindow::new();
    huge_jump.commit(0).unwrap();
    huge_jump.commit(last).unwrap();
    assert_eq!(huge_jump.check(0), Err(ReplayError::TooOld));
    assert_eq!(huge_jump.check(last - 1), Ok(()));
}

#[test]
fn every_bitmap_position_survives_successive_advances() {
    let mut window = ReplayWindow::new();
    for number in 0..3072 {
        window.commit(number).unwrap();
        for distance in 0..=number.min(1023) {
            assert_eq!(window.check(number - distance), Err(ReplayError::Duplicate));
        }
        if number >= 1024 {
            assert_eq!(window.check(number - 1024), Err(ReplayError::TooOld));
        }
    }
}
