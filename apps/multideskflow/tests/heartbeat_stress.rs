//! The IPC heartbeat under load — the regression test for CI-38220074904.
//!
//! On a loaded `windows-latest` runner (2 vCPU) the cores repeatedly declared their agent
//! lost and restarted it in `RECOVERY`, five cycles in a row, leaving the node
//! `Blocked`/`agent=starting` when the at-rest check of `three_nodes` looked at it. Two
//! product causes were fixed for this (`apps/lws-core/src/agent.rs`,
//! `apps/lws-agent/src/session.rs`): the silence clock ran while an agent was still
//! *starting*, and blocking platform work (the clipboard's `OpenClipboard` retry loop,
//! `EnumDisplayMonitors`) sat on the runtime thread the heartbeat rides.
//!
//! The test pins the tree to the two CPUs available on Windows CI and holds a three-node
//! cluster at rest for a minute: no transition, no input, just heartbeat. CI run
//! 38291198470 recorded all three nodes declaring silence at 393–394 ms while the CI
//! default spawned two pure-spin burners competing for those same CPUs.
//! The roots show no operation in flight, and pinned Windows reruns passed both with two
//! burners and with none. These burners fill available CPU and model deliberate
//! starvation, not ordinary runner background load, so they remain opt-in.
//! The promise being checked remains: the nodes stay `Local` with their agents `Ready`
//! the whole time: not one `the agent stopped answering`, not one
//! `agent lost; telling the FSM`, not one `core silent past the limit` in any log.
//!
//! ```text
//! # the faithful emulation of the CI runner (2 vCPU, everything on them)
//! powershell -NoProfile -Command "Start-Process cargo -ArgumentList 'test','-p','lws-core','--test','heartbeat_stress' -PassThru | %%{ $_.ProcessorAffinity = 3; $_.WaitForExit() }"
//!
//! # a longer or lighter run
//! LWS_STRESS_SECONDS=120 LWS_STRESS_BURNERS=1 cargo test -p lws-core --test heartbeat_stress
//! ```

mod common;

use common::Cluster;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long the loaded, at-rest cluster must run without a single false agent-lost.
fn stress_seconds() -> u64 {
    std::env::var("LWS_STRESS_SECONDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(60)
}

/// Number of optional pure-spin burners for an explicit starvation probe. The CI default
/// is zero: these threads fill the available CPUs and do not model ordinary runner load.
fn burner_count() -> usize {
    std::env::var("LWS_STRESS_BURNERS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

/// Pins this process — and therefore every child it spawns, which is the whole test
/// tree — to the first two CPUs, the way the CI runner's two vCPUs pinned everything.
///
/// Best effort on purpose: a machine with one CPU, or a sandbox that refuses the call,
/// still runs the stress, just without the emulation's squeeze.
fn pin_to_two_cpus() -> bool {
    #[cfg(windows)]
    {
        // `windows-sys` is already a dependency for the agent's job object; only the
        // `Win32_System_Threading` feature these two calls need is used.
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, SetProcessAffinityMask};
        // SAFETY: `GetCurrentProcess` is a pseudo-handle that needs no cleanup, and the
        // mask names CPUs 0 and 1, both of which exist on every machine Windows itself
        // runs on.
        let ok = unsafe { SetProcessAffinityMask(GetCurrentProcess(), 0b11) };
        if ok == 0 {
            eprintln!("heartbeat stress: could not pin to two CPUs; running unpinned");
            return false;
        }
        true
    }
    #[cfg(not(windows))]
    {
        eprintln!("heartbeat stress: pinning is Windows-only; running unpinned");
        false
    }
}

#[test]
fn loaded_agents_keep_answering_the_heartbeat() {
    let _serial = common::serial();
    let seconds = stress_seconds();
    let pinned = pin_to_two_cpus();
    println!(
        "heartbeat stress: {} s on {} pinned CPUs with {} burners",
        seconds,
        if pinned { "two" } else { "all" },
        burner_count()
    );

    let stop = Arc::new(AtomicBool::new(false));
    let mut burners = Vec::new();
    for index in 0..burner_count() {
        let stop = Arc::clone(&stop);
        burners.push(
            std::thread::Builder::new()
                .name(format!("lws-burner-{index}"))
                .spawn(move || {
                    // A pure spin: no I/O, no allocation, just scheduler pressure, which
                    // is what a loaded runner puts on the heartbeat's timer.
                    while !stop.load(Ordering::Relaxed) {
                        std::hint::spin_loop();
                    }
                })
                .expect("a burner thread starts"),
        );
    }

    let cluster = Cluster::start("heartbeat-stress");
    let started = Instant::now();
    // At rest on purpose: the failure was a false agent-lost with no transition in
    // flight, so the load is the only variable.
    while started.elapsed() < Duration::from_secs(seconds) {
        std::thread::sleep(Duration::from_millis(200));
    }

    stop.store(true, Ordering::Relaxed);
    for burner in burners {
        let _ = burner.join();
    }

    // The product's promise, read out of the logs the cluster wrote.
    let mut losses = 0;
    for (index, node) in cluster.nodes.iter().enumerate() {
        let log = node.log();
        let lost = log.matches("the agent stopped answering").count();
        let restarted = log.matches("agent lost; telling the FSM").count();
        let released = log.matches("core silent past the limit").count();
        assert_eq!(
            lost, 0,
            "node {index} lost its agent {} time(s) under load:\n{log}",
            lost
        );
        assert_eq!(
            restarted, 0,
            "node {index} restarted its agent {} time(s) under load:\n{log}",
            restarted
        );
        assert_eq!(
            released, 0,
            "node {index} released locally {} time(s) under load:\n{log}",
            released
        );
        losses += lost + restarted + released;
    }
    println!("heartbeat stress: 0 false agent losses over {} s", seconds);

    // And the nodes are where they were: at rest, agents attached.
    cluster.assert_idle();
    assert_eq!(losses, 0);
}
