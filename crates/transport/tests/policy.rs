//! The pure mesh and liveness decisions: dedup, membership and the `Ping`/offline clock.

use geometry::NodeId;
use protocol::limits::N_MAX;
use transport::{
    admit_member, liveness_action, next_liveness_wakeup, prefer_incoming, reconnect_delay,
    winning_dialer, BackoffBounds, LinkLiveness, LivenessAction, MembershipFull,
};

fn id(byte: u8) -> NodeId {
    NodeId::new([byte; 32])
}

/// The cadence a caller without a configuration gets; every call below is explicit about it
/// now that the transport takes it as a parameter instead of owning constants.
const LIVENESS: LinkLiveness = LinkLiveness {
    ping_ms: 1_000,
    offline_ms: 3_000,
};

/// The bounds `crates/config` validates and hands to the transport.
const BACKOFF: BackoffBounds = BackoffBounds {
    min_ms: 500,
    max_ms: 5_000,
};

#[test]
fn the_liveness_defaults_are_the_documented_ones() {
    // docs/06-development-plan.md, KVM-070: «Ping каждую 1 s; офлайн через 3 s без Pong».
    // `crates/config` (`workspace.link_ping_ms` / `workspace.link_timeout_ms`) is the owner
    // of the real numbers; this default is what a caller with no configuration gets.
    assert_eq!(LIVENESS, LinkLiveness::default());
}

#[test]
fn the_winning_dialer_is_the_larger_node_id_and_the_rule_is_symmetric() {
    for (a, b) in [
        (id(1), id(2)),
        (id(2), id(1)),
        (id(0), id(0xff)),
        (id(0xff), id(0x80)),
        (id(7), id(7)),
    ] {
        assert_eq!(
            winning_dialer(&a, &b),
            winning_dialer(&b, &a),
            "both sides must compute the same winner"
        );
        assert_eq!(winning_dialer(&a, &b), a.max(b));
    }
}

#[test]
fn both_sides_agree_on_which_simultaneous_dial_survives() {
    // «при одновременном дозвоне побеждает линк от большего node_id» — the two nodes see
    // the two links from opposite sides, so the decision has to be made without
    // negotiation: each side asks whether the *incoming* link's initiator is the winner.
    for (local, peer) in [(id(1), id(2)), (id(2), id(1)), (id(0x10), id(0x20))] {
        let winner = winning_dialer(&local, &peer);

        // Our own view: the existing link was dialled by us, the incoming one by the peer.
        let survivor_here = if prefer_incoming(&local, &peer) {
            peer
        } else {
            local
        };
        // The peer's view: the existing link was dialled by the peer, the incoming one by us.
        let survivor_there = if prefer_incoming(&peer, &local) {
            local
        } else {
            peer
        };

        assert_eq!(
            survivor_here, survivor_there,
            "one link must survive, not both"
        );
        assert_eq!(survivor_here, winner, "and it must be the larger initiator");
        assert!(
            !(prefer_incoming(&local, &peer) && prefer_incoming(&peer, &local)),
            "both sides cannot prefer the incoming link"
        );
    }
}

#[test]
fn membership_is_capped_at_min_max_members_and_n_max() {
    assert_eq!(N_MAX, 4);

    // The first members fit, the one that reaches the limit does not.
    assert_eq!(admit_member(0, 3), Ok(()));
    assert_eq!(admit_member(2, 3), Ok(()));
    assert_eq!(
        admit_member(3, 3),
        Err(MembershipFull {
            current: 3,
            max_members: 3
        })
    );

    // A configured limit above N_max is silently capped, so a fifth member can never be
    // admitted: «N_max = 4».
    assert_eq!(admit_member(3, 4), Ok(()));
    assert_eq!(
        admit_member(4, 10),
        Err(MembershipFull {
            current: 4,
            max_members: usize::from(N_MAX)
        })
    );
    assert_eq!(
        admit_member(0, 10),
        Ok(()),
        "below the cap the configuration wins"
    );

    // `max_members = 0` admits nobody rather than being read as "unlimited".
    assert_eq!(
        admit_member(0, 0),
        Err(MembershipFull {
            current: 0,
            max_members: 0
        })
    );
}

#[test]
fn the_reconnect_schedule_stays_inside_half_a_second_to_five_seconds() {
    // «backoff 0.5..5 s, все настроенные пиры». `crates/config`
    // (`network.reconnect_backoff_ms`) owns the range; this is the documented default.
    assert_eq!(BACKOFF, BackoffBounds::default());
    assert_eq!(reconnect_delay(0, BACKOFF).as_millis(), 500);
    assert_eq!(reconnect_delay(1, BACKOFF).as_millis(), 1_000);
    assert_eq!(reconnect_delay(2, BACKOFF).as_millis(), 2_000);
    assert_eq!(reconnect_delay(3, BACKOFF).as_millis(), 4_000);
    assert_eq!(
        reconnect_delay(4, BACKOFF).as_millis(),
        5_000,
        "8 s is clamped to the maximum"
    );

    let mut previous = reconnect_delay(0, BACKOFF);
    for attempt in 0..64u32 {
        let delay = reconnect_delay(attempt, BACKOFF);
        assert!(
            delay >= std::time::Duration::from_millis(500)
                && delay <= std::time::Duration::from_millis(5_000),
            "attempt {attempt} produced {delay:?}"
        );
        assert!(
            delay >= previous,
            "the schedule must not go backwards at {attempt}"
        );
        previous = delay;
    }
    assert_eq!(
        reconnect_delay(u32::MAX, BACKOFF).as_millis(),
        5_000,
        "no shift overflow"
    );
}

#[test]
fn liveness_pings_after_the_period_and_goes_offline_after_the_timeout() {
    // Nothing sent yet: the first check wants a Ping immediately.
    assert_eq!(liveness_action(1_000, 1_000, None, LIVENESS), LivenessAction::Ping);

    // A Ping that is younger than the period suppresses the next one.
    assert_eq!(
        liveness_action(1_000, 1_000, Some(1_000), LIVENESS),
        LivenessAction::Idle
    );
    assert_eq!(
        liveness_action(1_999, 1_000, Some(1_000), LIVENESS),
        LivenessAction::Idle
    );
    assert_eq!(
        liveness_action(2_000, 1_000, Some(1_000), LIVENESS),
        LivenessAction::Ping
    );

    // The offline deadline is the silence since the last Pong, not since the last Ping.
    assert_eq!(
        liveness_action(3_999, 1_000, Some(2_000), LIVENESS),
        LivenessAction::Ping
    );
    assert_eq!(
        liveness_action(4_000, 1_000, Some(3_900), LIVENESS),
        LivenessAction::Offline
    );
    assert_eq!(
        liveness_action(4_000, 1_000, None, LIVENESS),
        LivenessAction::Offline,
        "an unanswered link goes offline even when no Ping was ever sent"
    );

    // A fresh Pong resets the offline clock.
    assert_eq!(liveness_action(8_000, 7_999, None, LIVENESS), LivenessAction::Ping);
}

#[test]
fn the_next_wakeup_is_the_first_moment_something_can_change() {
    // The caller sleeps until the wakeup and re-checks; waking early would busy-loop and
    // waking late would delay either the Ping or the offline verdict.
    for now in [0u64, 500, 1_000, 2_500, 3_000, 5_000, 60_000] {
        for last_pong in [0u64, 500, 2_500, 2_999, 5_000] {
            for last_ping in [None, Some(0u64), Some(500), Some(2_500), Some(5_000)] {
                let wakeup = next_liveness_wakeup(now, last_pong, last_ping, LIVENESS);

                assert!(wakeup >= now, "never wake in the past: {now} -> {wakeup}");
                assert!(
                    wakeup <= std::cmp::max(now, last_pong + 3_000),
                    "never sleep past the offline verdict: {now} -> {wakeup}"
                );

                // Nothing changes strictly before the wakeup.
                for probe in [now, now + 1, wakeup.saturating_sub(1)] {
                    if probe < now || probe >= wakeup {
                        continue;
                    }
                    assert_eq!(
                        liveness_action(probe, last_pong, last_ping, LIVENESS),
                        LivenessAction::Idle,
                        "state changed before the wakeup ({probe} < {wakeup})"
                    );
                }

                // At the wakeup either a Ping is due or the link is offline.
                let action = liveness_action(wakeup, last_pong, last_ping, LIVENESS);
                assert_ne!(
                    action,
                    LivenessAction::Idle,
                    "waking at {wakeup} must find something to do"
                );
            }
        }
    }
}
