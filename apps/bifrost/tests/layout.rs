//! KVM-072 end to end on two real cores: the document, its CAS, and the partition rejoin.
//!
//! `docs/06-development-plan.md`: «`Topology{revision, author_node_id}` — единственный
//! документ, реплика на каждом узле; `LayoutEdit` применяет только координатор (текущий
//! owner), устаревшая база → `LAYOUT_CONFLICT`; после партиции координатор забирает
//! победившую ревизию…». Every test here drives the harness in `tests/common` — two `lws-core`
//! processes on loopback, real control channel, real links — because a document that only
//! replicates in a unit test has not replicated at all.
//!
//! The proxy's blackhole is the partition: `Proxy::blackhole` drops what the two cores say to
//! each other while both keep running, which is exactly the case the pending slot exists for.

mod common;

use std::time::{Duration, Instant};

use common::{Harness, Node};

/// The pending file the layout store keeps next to the node's `config.toml`.
///
/// This is the contract the GUI reads: «проигравшие правки лежат в локальном
/// `layout.pending.toml`» (`docs/03-architecture.md`). It exists exactly while the node holds
/// an edit the coordinator has not applied.
fn pending_file(node: &Node) -> std::path::PathBuf {
    node.root
        .join("config")
        .join(config::paths::LAYOUT_PENDING_FILE)
}

/// A valid whole document: one display, which is all the validator requires.
fn document_toml(label: &str) -> String {
    format!(
        "[[displays]]\nnode = \"self\"\nid = \"{label}\"\nlogical = [0.0, 0.0, 1920.0, 1080.0]\nprimary = true\n"
    )
}

/// Writes an edit document and returns its path.
fn edit_file(harness: &Harness, name: &str, label: &str) -> String {
    let path = harness.dir.join(name);
    std::fs::write(&path, document_toml(label)).expect("the edit file");
    path.to_string_lossy().into_owned()
}

/// One field of a `layout show` line, e.g. `revision`.
fn field(line: &str, name: &str) -> String {
    line.split_whitespace()
        .find_map(|part| part.strip_prefix(&format!("{name}=")))
        .unwrap_or_else(|| panic!("{line:?} has no {name}"))
        .to_owned()
}

/// Waits until both nodes report the *same* document, i.e. the same `(revision, author)` pair.
///
/// Comparing pairs inside the predicate is what keeps this race-free: the coordinator may move
/// on (or apply a forwarded edit) while the follower is catching up, and "the same document" is
/// the property the replication actually promises.
fn wait_same_document(harness: &Harness, what: &str) -> (String, String) {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut last = String::new();
    while Instant::now() < deadline {
        let follower = harness.b.ctl("layout show").unwrap_or_default();
        let coordinator = harness.a.ctl("layout show").unwrap_or_default();
        if let (Ok(follower_line), Ok(coordinator_line)) =
            (harness.b.ctl("layout show"), harness.a.ctl("layout show"))
        {
            let same = field(&follower_line, "revision") == field(&coordinator_line, "revision")
                && field(&follower_line, "author") == field(&coordinator_line, "author");
            if same {
                return (follower_line, coordinator_line);
            }
            last = format!("{follower_line} | {coordinator_line}");
        }
        let _ = (follower, coordinator);
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("{what} did not happen; last: {last}");
}

/// The last few lines of a node's log that mention the layout.
fn tail(node: &Node) -> String {
    node.log()
        .lines()
        .filter(|line| line.contains("layout") || line.contains("topology"))
        .rev()
        .take(8)
        .map(|line| line.chars().take(200).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Waits until *either* node reports this refusal code.
///
/// Which one is the coordinator after a partition depends on the ownership pair (the larger
/// node id wins at equal serials), and the test is about the refusal, not about which side of
/// the healed pair had to make it.
fn wait_refusal(harness: &Harness, code: &str, what: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut last = String::new();
    while Instant::now() < deadline {
        for node in [&harness.a, &harness.b] {
            if let Ok(line) = node.ctl("layout show") {
                if line.contains(&format!("last_error={code}")) {
                    return line;
                }
                last = line;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("{what} did not happen; last: {last}");
}

/// One revision field as a number.
fn revision(line: &str) -> u64 {
    field(line, "revision").parse().expect("a revision number")
}

/// Polls `layout show` until `predicate` accepts the line, or panics with the last one.
fn wait_layout(node: &Node, what: &str, predicate: impl Fn(&str) -> bool) -> String {
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut last = String::new();
    while Instant::now() < deadline {
        if let Ok(line) = node.ctl("layout show") {
            if predicate(&line) {
                return line;
            }
            last = line;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let relevant: Vec<String> = node
        .log()
        .lines()
        .filter(|line| line.contains("topology") || line.contains("layout"))
        .rev()
        .take(12)
        .map(|line| line.chars().take(160).collect())
        .collect();
    panic!(
        "{what} did not happen; last layout show: {last}\n{}",
        relevant.join("\n")
    );
}

/// The follower edits, the coordinator applies through the CAS, and the new revision reaches
/// both nodes — the whole point of replicating one document instead of merging rows.
#[test]
fn a_follower_edit_is_applied_by_the_coordinator_and_replicated() {
    let _guard = common::serial();
    let mut harness = Harness::start(false, true);
    harness.wait_linked();

    // The harness writes a `[topology]` snapshot document, so the pair starts at its revision;
    // what matters here is that the edit lands *above* it.
    let before = harness.a.ctl("layout show").expect("layout show");
    let start = revision(&before);
    assert_eq!(field(&before, "pending"), "no");

    let path = edit_file(&harness, "first.toml", "eDP-1");
    let reply = harness
        .b
        .ctl(&format!("layout edit {path}"))
        .expect("the follower sends the edit");
    // The follower's answer *is* the coordinator's verdict: the CAS ran one hop away, and a
    // bare «sent to the coordinator» would leave the editor guessing whether it landed.
    assert_eq!(
        reply,
        format!("OK applied revision={}", start + 1),
        "a follower's edit answers with the coordinator's verdict"
    );

    let applied = wait_layout(&harness.a, "the coordinator applies the edit", |line| {
        revision(line) == start + 1
    });
    // `wait_layout`'s own predicate already fixed the revision; the author is the part the
    // predicate cannot see.
    let author = field(&applied, "author");
    assert_eq!(
        author, harness.a.id,
        "the published revision is authored by the coordinator"
    );

    let follower = wait_layout(&harness.b, "the follower adopts the revision", |line| {
        revision(line) == start + 1
    });
    assert_eq!(
        field(&follower, "author"),
        harness.a.id,
        "the same pair on both"
    );
    assert_eq!(
        field(&follower, "pending"),
        "no",
        "an edit that reached the coordinator is not pending"
    );
    // And the document the store writes for the GUI is gone with the slot: an applied edit
    // must not leave a `layout.pending.toml` claiming otherwise.
    assert!(
        !pending_file(&harness.b).exists(),
        "an applied edit leaves no pending file: {:?}",
        pending_file(&harness.b)
    );
    harness.a.stop();
    harness.b.stop();
}

/// The partition case: an edit on each side while the link is blackholed, then a heal.
///
/// `docs/03-architecture.md`: «после партиции координатор забирает победившую ревизию у её
/// автора и публикует её как max_seen+1 под своим авторством». The coordinator's own edit wins
/// by the CAS; the follower's divergent one lies in `layout.pending.toml` with the base it was
/// written against (revision 1). After the heal that base is stale, so the coordinator refuses
/// the pending edit by name instead of applying it on top of the winner — ADR-013 leaves the
/// way out to the user («перезагрузить и применить своё»), which is the reload walk at the end
/// of this test: the stale entry goes, and the edit is written against the base that is
/// current now.
#[test]
fn a_partition_leaves_the_loser_pending_and_a_reload_resolves_it() {
    let _guard = common::serial();
    let mut harness = Harness::start(true, true);
    harness.wait_linked();
    let proxy = harness.proxy.as_ref().expect("a proxied pair");

    // Both sides edit with the link down. The coordinator applies locally; the follower
    // cannot reach it and keeps the document.
    proxy.blackhole();
    // The partition is visible through the link manager, not through a send: a blackhole eats
    // the frames, so the follower learns the coordinator is gone when the link is reported
    // down (`members = 0`), which is the state the pending slot is for.
    harness
        .b
        .wait_for(
            "the link to the coordinator is reported down",
            Duration::from_secs(20),
            |status| status.members == 0,
        )
        .expect("the blackhole must be visible as a down link");
    harness
        .a
        .ctl(&format!(
            "layout edit {}",
            edit_file(&harness, "a.toml", "eDP-A")
        ))
        .expect("the coordinator applies its own edit");
    let loser = harness
        .b
        .ctl(&format!(
            "layout edit {}",
            edit_file(&harness, "b.toml", "eDP-B")
        ))
        .expect_err("a blackholed coordinator cannot take the edit");
    assert!(
        loser.contains("NO_COORDINATOR"),
        "the follower says why the edit waits: {loser}"
    );
    let pending = harness.b.ctl("layout show").expect("layout show");
    assert_eq!(
        field(&pending, "pending"),
        "yes",
        "the edit waits as pending"
    );
    // The losing edit is on disk, not only in memory: that is what the GUI's Apply/Discard
    // buttons read after a restart (`layout.pending.toml`).
    assert!(
        pending_file(&harness.b).exists(),
        "a held edit is written to {:?}",
        pending_file(&harness.b)
    );
    // The follower applies nothing on its own; the coordinator's edit is where it will land.
    let loser_start = revision(&pending);
    let coordinator = harness.a.ctl("layout show").expect("layout show");
    assert_eq!(
        revision(&coordinator),
        loser_start + 1,
        "the coordinator applied its own edit locally"
    );

    // Heal: the coordinator's revision reaches the follower, which keeps its own edit aside.
    proxy.forward();
    let (healed, _) = wait_same_document(&harness, "the follower adopts the winner");
    assert_eq!(field(&healed, "author"), harness.a.id);
    assert_eq!(
        field(&healed, "pending"),
        "yes",
        "the loser's edit is still pending"
    );
    let winner = revision(&healed);
    assert_eq!(
        winner,
        loser_start + 1,
        "the winner is the coordinator's edit"
    );

    // `layout apply` sends the pending edit with the base *it* names (revision 1), not with the
    // revision this node has applied by now (2). The coordinator's CAS therefore refuses it by
    // name, and the pair stays exactly where the rejoin left it: the winning edit is not
    // silently reverted by the loser's stale document.
    let refused = harness
        .b
        .ctl("layout apply")
        .expect_err("the stale base must be refused, not applied");
    assert!(
        refused.contains("LAYOUT_CONFLICT"),
        "the refusal names the contract's code: {refused}"
    );
    // And the revision to reload: the editor reloads what the coordinator is on, not what it
    // happens to have (docs/05-protocol.md, the error table).
    assert!(
        refused.contains(&format!("revision {winner} applied")),
        "the refusal names the applied revision: {refused}"
    );
    wait_refusal(
        &harness,
        "ERROR_CODE_LAYOUT_CONFLICT",
        "the stale pending edit is refused",
    );
    // Refused, but not lost: ADR-013 keeps the edit aside for the user's reload.
    let held = harness.b.ctl("layout show").expect("layout show");
    assert_eq!(
        revision(&held),
        winner,
        "a refused edit does not move the revision"
    );
    assert_eq!(
        field(&held, "author"),
        harness.a.id,
        "the winner still owns the document"
    );
    assert_eq!(
        field(&held, "pending"),
        "yes",
        "the refused edit is still there to reload"
    );
    assert!(
        pending_file(&harness.b).exists(),
        "a refused edit keeps its pending file: {:?}",
        pending_file(&harness.b)
    );

    // The reload: the stale entry goes, and the edit is written against the revision that is
    // applied now — the base the CAS accepts.
    let discarded = harness
        .b
        .ctl("layout discard")
        .expect("the stale entry is discarded");
    assert!(discarded.contains("pending=discarded"), "{discarded}");
    assert!(
        !pending_file(&harness.b).exists(),
        "discard takes the file with the slot: {:?}",
        pending_file(&harness.b)
    );
    let reply = harness
        .b
        .ctl(&format!(
            "layout edit {}",
            edit_file(&harness, "b.toml", "eDP-B")
        ))
        .expect("the reloaded edit is sent");
    assert_eq!(
        reply,
        format!("OK applied revision={}", winner + 1),
        "the reloaded edit answers with the coordinator's verdict"
    );
    let (done, _) = wait_same_document(&harness, "the reloaded edit is applied");
    assert_eq!(
        revision(&done),
        winner + 1,
        "the reloaded edit lands above the winner"
    );
    assert_eq!(field(&done, "author"), harness.a.id);
    assert_eq!(field(&done, "pending"), "no");
    assert!(
        !pending_file(&harness.b).exists(),
        "the reloaded edit applied: no pending file is left behind"
    );
    harness.a.stop();
    harness.b.stop();
}

/// A stale base is refused with the contract's own code, and the editor can see it.
///
/// The pending edit is based on revision 1; the coordinator moves to 2 while the follower is
/// not looking, so `layout apply` carries a base the CAS must refuse — `LAYOUT_CONFLICT`
/// (`docs/05-protocol.md`, the error table).
#[test]
fn a_stale_edit_is_refused_with_layout_conflict() {
    let _guard = common::serial();
    let mut harness = Harness::start(true, true);
    harness.wait_linked();
    let proxy = harness.proxy.as_ref().expect("a proxied pair");

    // Get the follower into the pending state first (the partition above, in short).
    proxy.blackhole();
    harness
        .b
        .wait_for(
            "the link to the coordinator is reported down",
            Duration::from_secs(20),
            |status| status.members == 0,
        )
        .expect("the blackhole must be visible as a down link");
    harness
        .a
        .ctl(&format!(
            "layout edit {}",
            edit_file(&harness, "a.toml", "eDP-A")
        ))
        .expect("the coordinator applies its own edit");
    let _ = harness.b.ctl(&format!(
        "layout edit {}",
        edit_file(&harness, "b.toml", "eDP-B")
    ));
    proxy.forward();
    wait_same_document(&harness, "the follower adopts the coordinator's revision");

    // The pending edit is stale for a simpler reason: the coordinator applied *its* edit while
    // the follower was cut off, so the revision B's copy was based on no longer exists.
    // The refusal is what this test is about, and the adoption of the second revision is
    // asserted where it belongs (the test above); here the follower's pending edit is simply
    // based on the revision it had when the link came back.
    let follower_before = revision(&harness.b.ctl("layout show").expect("layout show"));

    // The pending edit is still based on revision 1: the coordinator's CAS refuses it by name,
    // and the editor reads that refusal from its own connection — the same words the
    // coordinator would have answered with, had it been the node that was asked.
    let rejection = harness
        .b
        .ctl("layout apply")
        .expect_err("the stale base must be refused, not applied");
    assert!(
        rejection.contains("ERR LAYOUT_CONFLICT"),
        "the verdict is the contract's own refusal: {rejection}"
    );
    assert!(
        rejection.contains(&format!("revision {follower_before} applied")),
        "the refusal tells the editor which revision to reload: {rejection}"
    );
    let refused = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        wait_refusal(
            &harness,
            "ERROR_CODE_LAYOUT_CONFLICT",
            "the stale edit is refused by name",
        )
    })) {
        Ok(line) => line,
        Err(_) => panic!(
            "the stale edit is refused by name did not happen.\nA log:\n{}\nB log:\n{}",
            tail(&harness.a),
            tail(&harness.b)
        ),
    };
    assert!(
        revision(&refused) >= follower_before,
        "the revision never goes back"
    );
    // Whatever the refusal was, both nodes still hold one document: the same pair.
    wait_same_document(&harness, "the pair keeps one document after the refusal");
    harness.a.stop();
    harness.b.stop();
}
