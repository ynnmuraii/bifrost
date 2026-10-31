use audio_model::focus::{FocusPolicy, Owner, Snapshot};

const OWNER: [u8; 32] = [1; 32];
const SOURCE: [u8; 32] = [2; 32];
const LISTENER: [u8; 32] = [3; 32];
const OTHER: [u8; 32] = [4; 32];
const MEMBERS: [[u8; 32]; 4] = [OWNER, SOURCE, LISTENER, OTHER];

fn owner() -> Owner {
    Owner {
        node: OWNER,
        serial: 1,
    }
}

fn snapshot(revision: u64, active: [u8; 32]) -> Snapshot {
    Snapshot {
        owner: owner(),
        revision,
        active: Some(active),
        lease: (active != OWNER).then_some([7; 16]),
        committed: true,
    }
}

fn policy() -> FocusPolicy {
    let mut policy = FocusPolicy::new(250, 150, 20);
    policy.set_owner(0, Some(owner()));
    policy
}

fn accept(policy: &mut FocusPolicy, now: u64, revision: u64, active: [u8; 32]) {
    assert!(policy.accept(now, OWNER, snapshot(revision, active), &MEMBERS));
}

#[test]
fn exact_debounce_and_linear_ramp_boundaries() {
    let mut policy = policy();
    accept(&mut policy, 0, 1, SOURCE);
    policy.tick(249);
    assert_eq!(policy.primary(), None);
    assert_eq!(policy.gain_percent(OTHER), 100);
    policy.tick(250);
    assert_eq!(policy.primary(), Some(SOURCE));
    assert_eq!(policy.gain_percent(OTHER), 100);
    policy.tick(325);
    assert_eq!(policy.gain_percent(OTHER), 60);
    assert_eq!(policy.gain_percent(SOURCE), 100);
    policy.tick(399);
    assert_eq!(policy.gain_percent(OTHER), 21);
    policy.tick(400);
    assert_eq!(policy.gain_percent(OTHER), 20);
}

#[test]
fn flapping_restarts_debounce_but_unchanged_fresh_reports_do_not() {
    let mut policy = policy();
    accept(&mut policy, 0, 1, SOURCE);
    accept(&mut policy, 200, 2, OTHER);
    accept(&mut policy, 300, 3, SOURCE);
    accept(&mut policy, 500, 4, SOURCE);
    policy.tick(549);
    assert_eq!(policy.primary(), None);
    policy.tick(550);
    assert_eq!(policy.primary(), Some(SOURCE));
    accept(&mut policy, 600, 5, OTHER);
    accept(&mut policy, 700, 6, SOURCE);
    policy.tick(1000);
    assert_eq!(policy.primary(), Some(SOURCE));
}

#[test]
fn unknown_holds_two_seconds_then_returns_to_full_equal_gains() {
    let mut policy = policy();
    accept(&mut policy, 0, 1, SOURCE);
    policy.tick(400);
    policy.unknown(500);
    policy.unknown(1000);
    policy.tick(2499);
    assert_eq!(policy.primary(), Some(SOURCE));
    assert_eq!(policy.gain_percent(OTHER), 20);
    policy.tick(2500);
    assert_eq!(policy.primary(), None);
    assert_eq!(policy.gain_percent(OTHER), 20);
    policy.tick(2575);
    assert_eq!(policy.gain_percent(OTHER), 60);
    policy.tick(2650);
    for node in MEMBERS {
        assert_eq!(policy.gain_percent(node), 100);
    }
}

#[test]
fn recovery_cancels_unknown_without_restarting_unchanged_focus() {
    let mut policy = policy();
    accept(&mut policy, 0, 1, SOURCE);
    policy.tick(400);
    policy.unknown(500);
    accept(&mut policy, 2499, 2, SOURCE);
    policy.tick(5000);
    assert_eq!(policy.primary(), Some(SOURCE));
    assert_eq!(policy.gain_percent(OTHER), 20);
    policy.unknown(5001);
    policy.tick(7151);
    accept(&mut policy, 7200, 3, SOURCE);
    policy.tick(7449);
    assert_eq!(policy.primary(), None);
    policy.tick(7450);
    assert_eq!(policy.primary(), Some(SOURCE));
    policy.tick(7600);
    assert_eq!(policy.gain_percent(OTHER), 20);
}

#[test]
fn repeated_unknown_snapshots_and_replays_never_extend_hold() {
    let mut policy = policy();
    accept(&mut policy, 0, 1, SOURCE);
    policy.tick(400);
    let unknown = Snapshot {
        active: None,
        lease: None,
        committed: false,
        ..snapshot(2, SOURCE)
    };
    assert!(policy.accept(500, OWNER, unknown, &MEMBERS));
    assert!(policy.accept(
        2000,
        OWNER,
        Snapshot {
            revision: 3,
            ..unknown
        },
        &MEMBERS
    ));
    assert!(!policy.accept(2400, OWNER, snapshot(1, SOURCE), &MEMBERS));
    policy.tick(2500);
    assert_eq!(policy.primary(), None);
}

#[test]
fn owner_handoff_resets_revision_namespace_and_rejects_old_authority() {
    let mut policy = policy();
    accept(&mut policy, 0, 100, SOURCE);
    policy.tick(400);
    let next = Owner {
        node: OTHER,
        serial: 2,
    };
    policy.set_owner(500, Some(next));
    assert!(!policy.accept(600, OWNER, snapshot(101, SOURCE), &MEMBERS));
    let new = Snapshot {
        owner: next,
        revision: 1,
        active: Some(OTHER),
        lease: None,
        committed: true,
    };
    assert!(!policy.accept(600, OWNER, new, &MEMBERS));
    assert!(!policy.accept(
        600,
        OTHER,
        Snapshot {
            owner: Owner { serial: 1, ..next },
            ..new
        },
        &MEMBERS
    ));
    assert!(policy.accept(600, OTHER, new, &MEMBERS));
    assert!(!policy.accept(700, OTHER, new, &MEMBERS));
    policy.tick(850);
    assert_eq!(policy.primary(), Some(OTHER));
}

#[test]
fn invalid_commit_shapes_and_membership_do_not_consume_revision() {
    let mut policy = policy();
    let valid = snapshot(1, SOURCE);
    let invalid = [
        Snapshot {
            revision: 0,
            ..valid
        },
        Snapshot {
            active: None,
            ..valid
        },
        Snapshot {
            active: Some([9; 32]),
            ..valid
        },
        Snapshot {
            lease: None,
            ..valid
        },
        Snapshot {
            active: Some(OWNER),
            ..valid
        },
        Snapshot {
            committed: false,
            ..valid
        },
        Snapshot {
            committed: false,
            active: None,
            ..valid
        },
    ];
    for value in invalid {
        assert!(!policy.accept(0, OWNER, value, &MEMBERS));
    }
    assert!(!policy.accept(0, OWNER, valid, &[SOURCE, LISTENER]));
    assert!(!policy.accept(0, OWNER, valid, &[OWNER, LISTENER]));
    assert!(!policy.accept(0, OWNER, valid, &[OWNER, SOURCE, SOURCE]));
    assert!(!policy.accept(0, OWNER, valid, &[OWNER, SOURCE, LISTENER, OTHER, [5; 32]]));
    assert!(policy.accept(0, OWNER, valid, &MEMBERS));
}

#[test]
fn interrupted_ramps_preserve_every_members_instantaneous_gain() {
    let mut policy = FocusPolicy::new(0, 150, 20);
    policy.set_owner(0, Some(owner()));
    accept(&mut policy, 0, 1, SOURCE);
    policy.tick(75);
    assert_eq!(policy.gain_percent(OTHER), 60);
    accept(&mut policy, 75, 2, OTHER);
    assert_eq!(policy.gain_percent(SOURCE), 100);
    assert_eq!(policy.gain_percent(OTHER), 60);
    policy.tick(150);
    assert_eq!(policy.gain_percent(SOURCE), 60);
    assert_eq!(policy.gain_percent(OTHER), 80);
    assert_eq!(policy.gain_percent(LISTENER), 40);
    accept(&mut policy, 150, 3, LISTENER);
    assert_eq!(policy.gain_percent(SOURCE), 60);
    assert_eq!(policy.gain_percent(OTHER), 80);
    assert_eq!(policy.gain_percent(LISTENER), 40);
    policy.tick(225);
    assert_eq!(policy.gain_percent(SOURCE), 40);
    assert_eq!(policy.gain_percent(OTHER), 50);
    assert_eq!(policy.gain_percent(LISTENER), 70);
    policy.tick(300);
    assert_eq!(policy.gain_percent(SOURCE), 20);
    assert_eq!(policy.gain_percent(OTHER), 20);
    assert_eq!(policy.gain_percent(LISTENER), 100);
}

#[test]
fn local_listener_focus_ducks_all_remote_sources_not_its_os_audio() {
    let mut policy = policy();
    accept(&mut policy, 0, 1, LISTENER);
    policy.tick(400);
    assert_eq!(policy.primary(), Some(LISTENER));
    for node in [OWNER, SOURCE, OTHER] {
        assert_eq!(policy.gain_percent(node), 20);
    }
    assert_eq!(policy.gain_percent(LISTENER), 100);
}

#[test]
fn configuration_changes_are_continuous_and_can_mute_background() {
    let mut policy = policy();
    accept(&mut policy, 0, 1, SOURCE);
    policy.tick(400);
    policy.configure(400, 0, 100, 0);
    assert_eq!(policy.gain_percent(OTHER), 20);
    policy.tick(450);
    assert_eq!(policy.gain_percent(OTHER), 10);
    policy.tick(500);
    assert_eq!(policy.gain_percent(OTHER), 0);
    assert_eq!(policy.gain_percent(SOURCE), 100);
    accept(&mut policy, 500, 2, OTHER);
    assert_eq!(policy.primary(), Some(OTHER));
    policy.tick(600);
    assert_eq!(policy.gain_percent(SOURCE), 0);
    assert_eq!(policy.gain_percent(OTHER), 100);
    policy.configure(600, 0, 0, 200);
    assert_eq!(policy.gain_percent(SOURCE), 100);
}

#[test]
fn sparse_ticks_process_deadlines_at_their_actual_time() {
    let mut policy = policy();
    accept(&mut policy, 0, 1, OWNER);
    policy.tick(1000);
    assert_eq!(policy.primary(), Some(OWNER));
    assert_eq!(policy.gain_percent(SOURCE), 20);
    policy.unknown(1000);
    policy.tick(4000);
    assert_eq!(policy.primary(), None);
    assert_eq!(policy.gain_percent(SOURCE), 100);
}

#[test]
fn unknown_during_debounce_never_commits_the_pending_receiver() {
    let mut policy = policy();
    accept(&mut policy, 0, 1, SOURCE);
    policy.tick(400);
    accept(&mut policy, 500, 2, OTHER);
    policy.unknown(749);
    policy.tick(1000);
    assert_eq!(policy.primary(), Some(SOURCE));
    assert_eq!(policy.gain_percent(OTHER), 20);
    policy.tick(2748);
    assert_eq!(policy.primary(), Some(SOURCE));
    policy.tick(2749);
    assert_eq!(policy.primary(), None);
}

#[test]
fn fresh_reports_preserve_an_in_progress_ramp() {
    let mut policy = policy();
    accept(&mut policy, 0, 1, SOURCE);
    policy.tick(300);
    let before = policy.gain_percent(OTHER);
    accept(&mut policy, 300, 2, SOURCE);
    assert_eq!(policy.gain_percent(OTHER), before);
    accept(&mut policy, 350, 3, SOURCE);
    assert_eq!(policy.gain_percent(OTHER), 47);
    policy.tick(400);
    assert_eq!(policy.gain_percent(OTHER), 20);
}

#[test]
fn same_owner_serial_change_starts_a_fresh_namespace_and_holds_old_primary() {
    let mut policy = policy();
    accept(&mut policy, 0, 50, SOURCE);
    policy.tick(400);
    let next = Owner {
        serial: 2,
        ..owner()
    };
    policy.set_owner(500, Some(next));
    policy.set_owner(1000, Some(next));
    assert!(!policy.accept(1000, OWNER, snapshot(51, OTHER), &MEMBERS));
    policy.tick(2499);
    assert_eq!(policy.primary(), Some(SOURCE));
    policy.tick(2500);
    assert_eq!(policy.primary(), None);
    assert!(policy.accept(
        2700,
        OWNER,
        Snapshot {
            owner: next,
            ..snapshot(1, SOURCE)
        },
        &MEMBERS,
    ));
    policy.tick(3100);
    assert_eq!(policy.primary(), Some(SOURCE));
    assert_eq!(policy.gain_percent(OTHER), 20);
}

#[test]
fn replacement_parent_can_restart_revision_with_the_same_ownership_pair() {
    let mut policy = policy();
    accept(&mut policy, 0, 100, SOURCE);
    policy.tick(400);
    // The coordinator fences the old authenticated parent before resetting this namespace.
    policy.set_owner(500, None);
    policy.set_owner(500, Some(owner()));
    assert_eq!(policy.primary(), Some(SOURCE));
    assert_eq!(policy.gain_percent(OTHER), 20);
    accept(&mut policy, 600, 1, OTHER);
    policy.tick(850);
    assert_eq!(policy.primary(), Some(OTHER));
    policy.tick(1000);
    assert_eq!(policy.gain_percent(OTHER), 100);
    assert_eq!(policy.gain_percent(SOURCE), 20);
}
