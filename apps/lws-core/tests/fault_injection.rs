//! Fault injection for the E2 gate: 1000 transitions, a broken link, a dead agent.
//!
//! `docs/06-development-plan.md` (gate after E2: «стабилен ли FSM — если есть залипания или
//! дубли, не добавлять функции до исправления») and `docs/07-quality-security.md`
//! («Fault injection») ask for exactly these runs, and `AC-01`/`AC-02`/`AC-14` are the
//! criteria they measure. Every scenario drives two real `lws-core` processes on loopback
//! through the control channel — the E2 slice as a user runs it, with dry agents standing
//! in for the OS input.
//! Dry file controls also cover held-edge refusal storms, emergency cancellation in
//! Preparing, local click passthrough during failed Temporary capture, and stale
//! topology edges without a live link. None of these controls inject desktop input.
//!
//! ```text
//! # the fast variant that runs with the workspace suite (50 cycles + the fault cases)
//! cargo test -p lws-core --test fault_injection
//!
//! # the full run: 1000 cycles, and the suspend case
//! cargo test -p lws-core --test fault_injection -- --ignored --nocapture --test-threads=1
//! ```
//!
//! The harness is `tests/common/mod.rs`; it polls `status` instead of sleeping, so a slow
//! host stretches a wait rather than breaking a test, and it reports the latency it
//! measured even when the assertion passes.

mod common;

use common::{Harness, AC_02_BUDGET_MS, HOST_SLACK_MS, LEASE_TTL_MS};
use std::time::{Duration, Instant};

/// The budget a shared host may add on top of the product's promise.
fn budget_with_slack() -> f64 {
    (AC_02_BUDGET_MS + HOST_SLACK_MS) as f64
}

/// One of the harness's own numbers, printed so a passing run still reports it.
fn report(label: &str, values: &[f64]) {
    if values.is_empty() {
        println!("{label}: no samples");
        return;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
    let pick = |q: f64| sorted[((sorted.len() - 1) as f64 * q).round() as usize];
    println!(
        "{label}: n={} p50={:.1}ms p95={:.1}ms p99={:.1}ms max={:.1}ms",
        sorted.len(),
        pick(0.50),
        pick(0.95),
        pick(0.99),
        sorted[sorted.len() - 1],
    );
}

/// The first `Local` the node logged at or after `after_ms`.
fn local_after(node: &common::Node, after_ms: i64) -> Option<i64> {
    node.focus_events()
        .into_iter()
        .find(|(at, value)| *at >= after_ms && value == "Local")
        .map(|(at, _)| at)
}

/// Runs `cycles` switch cycles and asserts the invariants of every one of them.
///
/// Returns the source's own latencies: `to_remote` and `to_local`, from the log's
/// `control request request=Switch` to the focus line it produced.
fn run_cycles(harness: &Harness, cycles: usize) -> (Vec<f64>, Vec<f64>) {
    let started = Instant::now();
    for cycle in 0..cycles {
        harness.cycle();
        if cycle % 100 == 99 {
            println!(
                "  {}/{} cycles ({:?})",
                cycle + 1,
                cycles,
                started.elapsed()
            );
        }
    }
    let windows = harness.a.switch_windows();
    let mut to_remote = Vec::new();
    let mut to_local = Vec::new();
    for window in &windows {
        for (state, latency) in window {
            match state.as_str() {
                "Remote" => to_remote.push(*latency),
                "Local" => to_local.push(*latency),
                _ => {}
            }
        }
    }
    assert_eq!(
        windows.len(),
        cycles * 2,
        "a cycle is two switches (to the peer and back), and each is one window"
    );
    (to_remote, to_local)
}

/// The plain slice: a link, a transition, and a clean return.
#[test]
fn a_pair_links_and_returns_to_local_with_an_empty_ledger() {
    // One harness at a time: see `common::serial`.
    let _serial = common::serial();
    let harness = Harness::start(false, false);
    harness.wait_linked();
    harness.cycle();
    let status = harness.a.status().expect("status");
    assert_eq!(status.focus, "Local");
    assert_eq!(status.pressed, 0);
    assert_eq!(status.peer, "-");
}

/// A held edge is one intent, not an unbounded stream of prepare retries.
#[test]
fn an_always_refusing_peer_gets_at_most_five_attempts_in_ten_seconds() {
    let _serial = common::serial();
    let harness = Harness::start_fault_controls(false, true);
    harness.wait_linked();
    harness.a.wait_dry_capture("Observe");
    harness.a.seed_dry_pointer(1600.0, 540.0);
    let before = harness.a.marked("pointer reached a link edge").len();
    harness.a.hold_dry_pointer(1960.0, 540.0);
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        assert!(
            harness.a.marked("pointer reached a link edge").len() - before <= 5,
            "a held pointer caused a transition storm against a refusing peer"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let attempts = harness.a.marked("pointer reached a link edge").len() - before;
    assert!(
        attempts > 0,
        "the held edge must actually exercise preparation"
    );
    assert!(attempts <= 5, "{attempts} attempts in ten seconds");
    harness
        .a
        .wait_for(
            "failed preparation releases input",
            Duration::from_secs(2),
            |s| s.is_idle(),
        )
        .expect("a refusal must leave input local");
    assert!(
        harness
            .a
            .focus_events()
            .iter()
            .any(|(_, focus)| focus == "Preparing"),
        "the refusing receiver must be reached, not bypassed by a source preflight"
    );
    assert!(
        !harness
            .a
            .focus_events()
            .iter()
            .any(|(_, focus)| focus == "Remote"),
        "the always-refusing receiver must never gain input"
    );
}

/// The emergency interrupts a pending prepare, without waiting for its timeout.
#[test]
fn emergency_during_preparing_returns_local_within_one_hundred_milliseconds() {
    let _serial = common::serial();
    let harness = Harness::start_fault_controls(true, false);
    harness.wait_linked();
    harness.proxy.as_ref().expect("proxy").blackhole();
    harness.a.switch(&harness.b.id).expect("start preparation");
    harness.a.wait_dry_capture("Temporary");
    assert_eq!(
        harness.a.status().expect("preparing status").focus,
        "Preparing"
    );
    let fired = common::now_millis();
    harness.a.trigger_emergency();
    harness
        .a
        .wait_for("emergency restores Local", Duration::from_secs(2), |s| {
            s.is_idle()
        })
        .expect("emergency must release the pending prepare");
    let local = local_after(&harness.a, fired).expect("Local transition after emergency");
    assert!(
        local - fired <= 100,
        "emergency took {}ms during Preparing (budget 100ms)",
        local - fired
    );
    harness.a.wait_dry_capture("Observe");
}

/// Temporary does not suppress discrete local input even if the prepare times out.
#[test]
fn a_click_during_a_failed_temporary_capture_reaches_the_local_desktop() {
    let _serial = common::serial();
    let harness = Harness::start_fault_controls(true, false);
    harness.wait_linked();
    harness.proxy.as_ref().expect("proxy").blackhole();
    harness.a.switch(&harness.b.id).expect("start preparation");
    harness.a.wait_dry_capture("Temporary");
    let before = harness.a.dry_local_passthrough();
    harness.a.trigger_dry_click();
    let deadline = Instant::now() + Duration::from_secs(2);
    while harness.a.dry_local_passthrough() == before {
        assert!(
            Instant::now() < deadline,
            "the local click was lost during Preparing"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    harness.a.wait_dry_capture("Temporary");
    assert_eq!(
        harness.a.dry_local_passthrough() - before,
        2,
        "both the local button down and up must pass through exactly once"
    );
    harness
        .a
        .wait_for("prepare times out", Duration::from_secs(2), |s| s.is_idle())
        .expect("failed Temporary capture must release input");
    assert_eq!(harness.a.dry_local_passthrough() - before, 2);
    assert!(
        !harness
            .a
            .focus_events()
            .iter()
            .any(|(_, focus)| focus == "Remote"),
        "the blackholed prepare must not commit"
    );
}

/// A stale topology link cannot arm suppression when its transport is absent.
#[test]
fn a_held_edge_without_a_live_link_never_enters_preparing() {
    let _serial = common::serial();
    let harness = Harness::start_fault_controls(false, false);
    harness.wait_linked();
    harness.a.wait_dry_capture("Observe");
    harness.a.seed_dry_pointer(1600.0, 540.0);
    common::Node::kill_pid(harness.b.pid()).expect("stop the dry peer");
    harness
        .a
        .wait_for("peer link is gone", Duration::from_secs(10), |s| {
            s.members == 0 && s.is_idle()
        })
        .expect("the source must observe link loss");
    let before = harness.a.focus_events().len();
    harness.a.hold_dry_pointer(1960.0, 540.0);
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        assert!(
            harness.a.focus_events()[before..]
                .iter()
                .all(|(_, focus)| focus != "Preparing"),
            "a dead peer must be rejected before Temporary capture"
        );
        assert!(harness.a.status().expect("unlinked status").is_idle());
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A restored source must reconcile a newer remembered pair without a manual claim.
#[test]
fn restored_owner_reconciles_a_receivers_higher_serial_before_switching() {
    let _serial = common::serial();
    const SOURCE_SERIAL: u64 = 1;
    const RECEIVER_SERIAL: u64 = 2;
    let (harness, ()) = Harness::start_with(false, false, |a_root, b_root| {
        common::seed_restored_source_owner(a_root, b_root, SOURCE_SERIAL, RECEIVER_SERIAL);
    });
    harness.wait_linked();

    let source = harness
        .a
        .wait_for(
            "restored source adopts at least the receiver's remembered serial",
            Duration::from_secs(20),
            |status| {
                status.role == "owner"
                    && status.owner == harness.a.id
                    && status.serial >= RECEIVER_SERIAL
            },
        )
        .expect("the lower restored source pair must reconcile automatically");
    let receiver = harness
        .b
        .wait_for(
            "receiver agrees on the restored source's ownership pair",
            Duration::from_secs(20),
            |status| status.owner == source.owner && status.serial == source.serial,
        )
        .expect("both cores must agree on the same ownership pair");
    assert_eq!(receiver.owner, harness.a.id);
    assert_eq!(receiver.serial, source.serial);
    assert!(source.serial > SOURCE_SERIAL);

    // Only normal switch requests: Remote on both, then Local with no pressed inputs.
    harness.cycle();
    for node in [&harness.a, &harness.b] {
        let status = node.status().expect("status after returning Local");
        assert_eq!(status.owner, source.owner);
        assert_eq!(status.serial, source.serial);
        assert!(
            status.is_idle(),
            "cleanup must leave no lease or keys: {status:?}"
        );
    }
}

/// The CI variant of the 1000-cycle run: fifty transitions, every invariant checked.
#[test]
fn fifty_transitions_run_in_ci() {
    // One harness at a time: see `common::serial`.
    let _serial = common::serial();
    let harness = Harness::start(false, false);
    harness.wait_linked();
    let (to_remote, to_local) = run_cycles(&harness, 50);
    assert_eq!(
        to_remote.len(),
        50,
        "every switch to the peer must be logged"
    );
    assert_eq!(to_local.len(), 50, "every return must be logged");
    report("switch -> Remote", &to_remote);
    report("switch -> Local", &to_local);
    // A transition is tens of milliseconds on an idle host; five seconds is the
    // "something is wrong" line, not a performance target.
    for latency in to_remote.iter().chain(&to_local) {
        assert!(
            *latency < 5_000.0,
            "a transition took {latency} ms, which is a stall rather than a transition"
        );
    }
}

/// `AC-01`: 1000 transitions to the peer and back, with no stuck focus and no lost edges.
///
/// Ignored by default because it takes minutes; `docs/07-quality-security.md` asks for it
/// before the E2 gate is called closed.
#[test]
#[ignore = "1000 cycles: cargo test -p lws-core --test fault_injection -- --ignored --nocapture --test-threads=1"]
fn a_thousand_transitions_leave_no_lease_and_no_held_input() {
    // One harness at a time: see `common::serial`.
    let _serial = common::serial();
    let harness = Harness::start(false, false);
    harness.wait_linked();
    let started = Instant::now();
    let (to_remote, to_local) = run_cycles(&harness, 1000);
    println!("1000 cycles in {:?}", started.elapsed());
    report("switch -> Remote", &to_remote);
    report("switch -> Local", &to_local);
    assert_eq!(to_remote.len(), 1000);
    assert_eq!(to_local.len(), 1000);
    for latency in to_remote.iter().chain(&to_local) {
        assert!(*latency < 5_000.0, "a transition took {latency} ms");
    }
}

/// The edge trigger, without the control channel: `--dry-motion` walks the pointer out
/// and back, and the geometry resolves both crossings.
#[test]
fn the_edge_trigger_drives_a_cycle_without_the_ctl_channel() {
    // One harness at a time: see `common::serial`.
    let _serial = common::serial();
    let harness = Harness::start(false, true);
    harness.wait_linked();
    // The aid walks 1600 -> 1960 (crossing 1920) after a three-second lead-in, pauses a
    // second, and walks back to 1200, so the whole round trip is over in ~8 s.
    harness
        .a
        .wait_for("the edge crossing", Duration::from_secs(30), |status| {
            status.focus == "Remote"
        })
        .expect("the pointer must cross the configured edge");
    harness
        .b
        .wait_for(
            "the peer is the receiver",
            Duration::from_secs(10),
            |status| status.focus == "Remote",
        )
        .expect("the peer must receive the lease");
    harness
        .a
        .wait_for("the return sweep", Duration::from_secs(30), |status| {
            status.is_idle()
        })
        .expect("the return sweep must release the lease");
    harness
        .b
        .wait_for("the peer returns", Duration::from_secs(10), |status| {
            status.is_idle()
        })
        .expect("the peer must return to Local");
    harness.assert_idle_or_remote("Local");
    assert!(
        harness.a.switch_windows().is_empty(),
        "the edge cycle must not need the control channel"
    );
}

/// A return to `Local` tears the session capture down — and with it the owner's own
/// observation of the local pointer. If the owner does not re-arm it, the observer is
/// latched as armed forever and no later edge crossing can start: the pointer is never
/// watched again, so the second crossing never happens.
#[test]
fn the_owner_re_arms_its_observation_after_returning_to_local() {
    // One harness at a time: see `common::serial`.
    let _serial = common::serial();
    let harness = Harness::start(false, false);
    harness.wait_linked();
    // `wait_linked` returns when the status says owner; the observation is armed a moment
    // later (the claim itself takes the grant window, and the arm follows it), so wait for
    // the arm rather than reading the count in the same breath as the ownership change.
    let deadline = Instant::now() + Duration::from_secs(10);
    while harness.a.observe_arms() < 1 {
        assert!(
            Instant::now() < deadline,
            "the owning source must observe the local pointer once the workspace is its own"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let armed_before = harness.a.observe_arms();

    // Observation is armed on a session tick, not synchronously with Local. Wait after
    // each return: two ctl-driven cycles can otherwise finish inside one tick and
    // legitimately share the same final re-arm.
    for cycle in 1..=2 {
        harness.cycle();
        let deadline = Instant::now() + Duration::from_secs(10);
        while harness.a.observe_arms() < armed_before + cycle {
            let armed = harness.a.observe_arms();
            assert!(
                Instant::now() < deadline,
                "the owner stopped observing after {armed} arm(s): return {cycle} must \
                 restore a live observer (while {armed_before} were expected before the cycles)"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// The owner's observation follows the agent's capture, it does not latch. The emergency
/// combination pressed while the node is already `Local` tears that capture down on the
/// agent with no FSM transition (core's `on_emergency` is a no-op on an idle slot, so no
/// `EndCapture` is ever emitted); core has to notice the capture is gone and arm a fresh
/// observation. A latched `observe_armed` leaves the pointer unwatched, so the edge the
/// dry walk crosses afterwards never starts a transition until core is restarted.
#[test]
fn the_emergency_while_local_re_arms_the_observation_without_a_restart() {
    // One harness at a time: see `common::serial`.
    let _serial = common::serial();
    let harness = Harness::start(false, true);
    harness.wait_linked();

    // The emergency must land while core is `Local` and the observation is already armed;
    // firing it before that would only test startup.
    let deadline = Instant::now() + Duration::from_secs(20);
    while harness.a.observe_arms() < 1 {
        assert!(
            Instant::now() < deadline,
            "the owner never armed its observation"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let armed_before = harness.a.observe_arms();
    assert_eq!(
        harness.a.status().expect("status").focus,
        "Local",
        "the emergency must land while input is local"
    );

    harness.a.trigger_emergency();

    // The capture is gone; core must arm a fresh observation on its next tick. This is the
    // regression: on the latched build the count never moves.
    let deadline = Instant::now() + Duration::from_secs(15);
    while harness.a.observe_arms() <= armed_before {
        assert!(
            Instant::now() < deadline,
            "the emergency left the observation latched: still {armed_before} arm(s), no \
             re-arm without a restart"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // And the re-armed observation must actually notice the pointer: the dry walk crosses
    // the right edge, a `Local -> Preparing -> Remote` with no restart in between.
    harness
        .a
        .wait_for(
            "the crossing after the emergency",
            Duration::from_secs(30),
            |status| status.focus == "Remote",
        )
        .expect("the pointer must cross the edge once the observation is re-armed");
}

/// `AC-02`, blackhole: the link stays up at the socket level and says nothing.
#[test]
fn a_blackholed_link_returns_both_sides_to_local() {
    // One harness at a time: see `common::serial`.
    let _serial = common::serial();
    let harness = Harness::start(true, false);
    harness.wait_linked();
    harness.a.switch(&harness.b.id).expect("switch");
    harness
        .a
        .wait_for("A is Remote", Duration::from_secs(10), |s| {
            s.focus == "Remote"
        })
        .expect("Remote");
    let proxy = harness.proxy.as_ref().expect("a proxied link");
    let injected = common::now_millis();
    proxy.blackhole();
    let injected_after = common::now_millis();

    harness
        .a
        .wait_for("A returns to Local", Duration::from_secs(10), |s| {
            s.is_idle()
        })
        .expect("A must return to Local");
    harness
        .b
        .wait_for("B returns to Local", Duration::from_secs(10), |s| {
            s.is_idle()
        })
        .expect("B must return to Local");
    let a_ms = local_after(&harness.a, injected).expect("A logs its Local");
    let b_ms = local_after(&harness.b, injected).expect("B logs its Local");
    println!(
        "blackhole: A {} ms, B {} ms (injection {} ms; budget {LEASE_TTL_MS} + 100)",
        a_ms - injected,
        b_ms - injected,
        injected_after - injected,
    );
    for (name, value) in [("A", a_ms), ("B", b_ms)] {
        assert!(
            (value - injected) as f64 <= budget_with_slack(),
            "{name} took {} ms to return to Local after the blackhole",
            value - injected
        );
    }
    // The lease watchdog is the only thing that can have released: the socket is open.
    assert!(
        !harness.a.marked("LeaseExpired").is_empty(),
        "A must report the expired lease, not a link loss"
    );
    proxy.forward();
}

/// `AC-02`, RST: the peer's death is reported as a link failure, not as a lease expiry.
#[test]
fn a_reset_link_returns_both_sides_to_local() {
    // One harness at a time: see `common::serial`.
    let _serial = common::serial();
    let harness = Harness::start(true, false);
    harness.wait_linked();
    harness.a.switch(&harness.b.id).expect("switch");
    harness
        .a
        .wait_for("A is Remote", Duration::from_secs(10), |s| {
            s.focus == "Remote"
        })
        .expect("Remote");
    let proxy = harness.proxy.as_ref().expect("a proxied link");
    let injected = common::now_millis();
    proxy.reset();

    harness
        .a
        .wait_for("A returns to Local", Duration::from_secs(10), |s| {
            s.is_idle()
        })
        .expect("A must return to Local");
    harness
        .b
        .wait_for("B returns to Local", Duration::from_secs(10), |s| {
            s.is_idle()
        })
        .expect("B must return to Local");
    let a_ms = local_after(&harness.a, injected).expect("A logs its Local");
    let b_ms = local_after(&harness.b, injected).expect("B logs its Local");
    println!(
        "RST: A {} ms, B {} ms (budget {LEASE_TTL_MS} + 100)",
        a_ms - injected,
        b_ms - injected,
    );
    for (name, value) in [("A", a_ms), ("B", b_ms)] {
        assert!(
            (value - injected) as f64 <= budget_with_slack(),
            "{name} took {} ms to return to Local after the reset",
            value - injected
        );
    }
}

/// The receiver's agent dies mid-Remote: core tells the FSM, releases, and restarts it in
/// `RECOVERY`.
#[test]
fn a_killed_receiver_agent_returns_to_local() {
    // One harness at a time: see `common::serial`.
    let _serial = common::serial();
    let harness = Harness::start(false, false);
    harness.wait_linked();
    harness.a.switch(&harness.b.id).expect("switch");
    harness
        .a
        .wait_for("A is Remote", Duration::from_secs(10), |s| {
            s.focus == "Remote"
        })
        .expect("Remote");
    let agent = harness.b.agent_pid().expect("the receiver's agent");
    let injected = common::now_millis();
    common::Node::kill_pid(agent).expect("the agent is killed");
    let injected_after = common::now_millis();

    harness
        .a
        .wait_for("A returns to Local", Duration::from_secs(10), |s| {
            s.is_idle()
        })
        .expect("A must return to Local");
    let a_ms = local_after(&harness.a, injected).expect("A logs its Local");
    println!(
        "agent kill: A {} ms (injection {} ms; budget {LEASE_TTL_MS} + 100)",
        a_ms - injected,
        injected_after - injected,
    );
    assert!(
        (a_ms - injected_after) as f64 <= budget_with_slack(),
        "A took {} ms to return to Local after the receiver's agent died",
        a_ms - injected_after
    );
    harness
        .b
        .wait_for("B restarts its agent", Duration::from_secs(20), |s| {
            s.agent == "ready"
        })
        .expect("B must restart its agent");
    // The receiver must have gone through the documented recovery path.
    let recovery = harness.b.marked("restarting the agent in RECOVERY");
    assert!(
        !recovery.is_empty(),
        "B must report «restarting in RECOVERY»:\n{}",
        harness.b.log()
    );
}

/// A frozen core cannot send a `Leave`, and the socket stays open: only the source's own
/// watchdog can end the lease.
#[test]
#[ignore = "needs process suspension (NtSuspendProcess / SIGSTOP): cargo test -p lws-core --test fault_injection -- --ignored --nocapture --test-threads=1"]
fn a_suspended_receiver_core_trips_the_source_watchdog() {
    // One harness at a time: see `common::serial`.
    let _serial = common::serial();
    let harness = Harness::start(false, false);
    harness.wait_linked();
    harness.a.switch(&harness.b.id).expect("switch");
    harness
        .a
        .wait_for("A is Remote", Duration::from_secs(10), |s| {
            s.focus == "Remote"
        })
        .expect("Remote");
    let injected = common::now_millis();
    harness.b.suspend().expect("the receiver is frozen");
    let injected_after = common::now_millis();

    harness
        .a
        .wait_for("A returns to Local", Duration::from_secs(10), |s| {
            s.is_idle()
        })
        .expect("A must return to Local");
    let a_ms = local_after(&harness.a, injected).expect("A logs its Local");
    let path = if !harness.a.marked("LeaseExpired").is_empty() {
        "the lease watchdog"
    } else if !harness.a.marked("peer -> core Leave").is_empty() {
        "a Leave that beat the freeze"
    } else {
        "an unrecognised path"
    };
    println!(
        "suspend: A {} ms after the suspension returned \
         (the call took {} ms; budget {LEASE_TTL_MS} + 100; released by {path})",
        a_ms - injected_after,
        injected_after - injected,
    );
    assert!(
        (a_ms - injected_after) as f64 <= budget_with_slack(),
        "A took {} ms to return to Local after the receiver stopped answering",
        a_ms - injected_after
    );
    harness.b.resume().expect("the receiver is resumed");
    harness
        .b
        .wait_for("B returns to Local", Duration::from_secs(10), |s| {
            s.is_idle()
        })
        .expect("B must return to Local once it runs again");
}
