//! The trust store: the `TRUSTED_PEER` rules of the ERD, revoke, and the atomic
//! load/save path.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use geometry::NodeId;
use identity::atomic::{self, LoadResult, WriteOutcome};
use identity::{fingerprint, now_unix_ms, TrustStore, TrustedPeer};

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bifrost-identity-tests-{}-{}-{label}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temporary directory");
    dir
}

fn node(byte: u8) -> NodeId {
    NodeId::new([byte; 32])
}

fn peer(byte: u8, name: &str, paired_at: i64) -> TrustedPeer {
    TrustedPeer::paired(
        node(byte),
        name,
        Some("127.0.0.1:47901".to_owned()),
        paired_at,
    )
}

#[test]
fn an_empty_store_trusts_nobody() {
    let store = TrustStore::new();
    assert!(store.is_empty());
    assert_eq!(store.len(), 0);
    assert!(store.pinned().is_empty());
    assert!(!store.is_trusted(&node(1)));
    assert_eq!(store.get(&node(1)), None);
}

#[test]
fn trusting_a_peer_makes_it_pinnable() {
    let mut store = TrustStore::new();
    store.trust(peer(1, "laptop-a", 1_000));

    assert_eq!(store.len(), 1);
    assert!(store.is_trusted(&node(1)));
    assert_eq!(store.pinned(), [node(1)].into_iter().collect());
    assert_eq!(
        store.get(&node(1)).map(|entry| entry.display_name.as_str()),
        Some("laptop-a")
    );
    assert_eq!(store.active().count(), 1);
}

#[test]
fn revoking_takes_the_peer_out_of_the_pin_but_keeps_its_record() {
    // «TRUSTED_PEER с revoked_at не может открыть сеанс»: the pin is what the handshake
    // checks, so leaving it is the revocation.
    let mut store = TrustStore::new();
    store.trust(peer(1, "laptop-a", 1_000));
    store.trust(peer(2, "laptop-b", 2_000));

    assert!(store.revoke(&node(1), 3_000));

    assert!(
        !store.is_trusted(&node(1)),
        "a revoked peer cannot open a session"
    );
    assert!(!store.pinned().contains(&node(1)));
    assert!(store.pinned().contains(&node(2)), "other peers stay pinned");
    assert_eq!(store.len(), 2, "the record is kept for the audit/UI");
    assert_eq!(
        store
            .get(&node(1))
            .and_then(|entry| entry.revoked_at_unix_ms),
        Some(3_000)
    );
    assert!(!store.get(&node(1)).expect("record").is_active());
}

#[test]
fn revoke_reports_whether_it_changed_anything() {
    let mut store = TrustStore::new();
    store.trust(peer(1, "laptop-a", 1_000));

    assert!(
        store.revoke(&node(1), 2_000),
        "first revoke changes the record"
    );
    assert!(!store.revoke(&node(1), 3_000), "already revoked");
    assert_eq!(
        store
            .get(&node(1))
            .and_then(|entry| entry.revoked_at_unix_ms),
        Some(2_000),
        "a second revoke must not move the stamp"
    );
    assert!(!store.revoke(&node(9), 3_000), "unknown peer");
}

#[test]
fn forget_removes_the_record_entirely() {
    let mut store = TrustStore::new();
    store.trust(peer(1, "laptop-a", 1_000));
    store.revoke(&node(1), 2_000);

    assert!(store.forget(&node(1)));
    assert!(store.is_empty());
    assert_eq!(store.get(&node(1)), None);
    assert!(!store.forget(&node(1)), "nothing left to forget");
}

#[test]
fn re_pairing_replaces_the_record_wholesale() {
    let mut store = TrustStore::new();
    store.trust(peer(1, "laptop-a", 1_000));
    store.revoke(&node(1), 2_000);
    assert!(!store.is_trusted(&node(1)));

    // An explicit new ceremony writes a fresh record: the old stamp cannot leak into it,
    // and the peer becomes active again because a human just approved it.
    store.trust(TrustedPeer::paired(
        node(1),
        "laptop-a-renamed",
        Some("10.0.0.5:47901".to_owned()),
        5_000,
    ));

    assert_eq!(store.len(), 1, "one record per node_id");
    let record = store.get(&node(1)).expect("record");
    assert_eq!(record.display_name, "laptop-a-renamed");
    assert_eq!(record.paired_at_unix_ms, 5_000);
    assert_eq!(record.revoked_at_unix_ms, None, "the new record is active");
    assert_eq!(record.last_address.as_deref(), Some("10.0.0.5:47901"));
    assert!(store.is_trusted(&node(1)));
}

#[test]
fn set_last_address_only_touches_known_peers() {
    let mut store = TrustStore::new();
    store.trust(peer(1, "laptop-a", 1_000));

    assert!(store.set_last_address(&node(1), "192.168.1.20:47901"));
    assert_eq!(
        store
            .get(&node(1))
            .and_then(|entry| entry.last_address.as_deref()),
        Some("192.168.1.20:47901")
    );
    assert!(!store.set_last_address(&node(2), "192.168.1.21:47901"));
}

#[test]
fn toml_round_trip_preserves_every_field() {
    let mut store = TrustStore::new();
    let mut active = peer(1, "laptop-a", 1_000);
    active.clipboard_allowed = true;
    store.trust(active);
    store.trust(peer(2, "laptop-b", 2_000));
    store.revoke(&node(2), 3_000);

    let text = store.to_toml().expect("serialize");
    let parsed = TrustStore::from_toml(&text).expect("parse");
    assert_eq!(parsed, store);

    // The record identity is stored as hex, and the hex is the documented fingerprint.
    assert!(text.contains(&fingerprint(&node(1))), "{text}");
}

#[test]
fn from_toml_refuses_duplicate_records_and_malformed_node_ids() {
    let hex = fingerprint(&node(1));
    let record = |node_id: &str| {
        format!(
            "[[peers]]\nnode_id = \"{node_id}\"\ndisplay_name = \"x\"\n\
             last_address = \"127.0.0.1:1\"\npaired_at_unix_ms = 1\n\
             revoked_at_unix_ms = 1\nclipboard_allowed = false\n"
        )
    };

    let duplicate = format!("{}{}", record(&hex), record(&hex));
    let error = TrustStore::from_toml(&duplicate).expect_err("duplicate node_id");
    assert!(error.contains("duplicate"), "{error}");

    for bad in ["", "abcd", "zzzz", &"g".repeat(64)] {
        let text = record(bad);
        assert!(
            TrustStore::from_toml(&text).is_err(),
            "node_id `{bad}` must be refused"
        );
    }

    assert!(TrustStore::from_toml("this is not toml [[[").is_err());
}

#[test]
fn save_then_load_round_trips_through_the_filesystem() {
    let root = temp_dir("trust-save");
    let path = root.join("trust.toml");
    let mut store = TrustStore::new();
    store.trust(peer(1, "laptop-a", 1_000));

    let outcome = store.save(&path).expect("save");
    assert_eq!(outcome, WriteOutcome::Created, "a fresh file is Created");

    match TrustStore::load(&path).expect("load") {
        LoadResult::Loaded(loaded) => assert_eq!(loaded, store),
        other => panic!("expected a clean load, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_corrupt_primary_file_recovers_from_the_backup_and_both_corrupt_is_reported() {
    let root = temp_dir("trust-recover");
    let path = root.join("trust.toml");
    let backup = root.join("trust.toml.bak");

    let mut first = TrustStore::new();
    first.trust(peer(1, "laptop-a", 1_000));
    first.save(&path).expect("first save");

    let mut second = TrustStore::new();
    second.trust(peer(2, "laptop-b", 2_000));
    let outcome = second.save(&path).expect("second save");
    assert_eq!(
        outcome,
        WriteOutcome::Replaced {
            backup: backup.clone()
        }
    );
    assert_eq!(
        std::fs::read(&backup).expect("backup exists"),
        first.to_toml().unwrap().into_bytes()
    );

    // «При ошибке разбора: загрузить .bak» — the recovered value is the previous one.
    std::fs::write(&path, "corrupted [[[").expect("corrupt primary");
    match TrustStore::load(&path).expect("load") {
        LoadResult::Recovered {
            value,
            backup: used,
            primary_error,
        } => {
            assert_eq!(value, first);
            assert_eq!(used, backup);
            assert!(!primary_error.is_empty());
        }
        other => panic!("expected a recovery, got {other:?}"),
    }

    std::fs::write(&backup, "corrupted too [[[").expect("corrupt backup");
    match TrustStore::load(&path).expect("load") {
        LoadResult::Corrupt { primary_error } => assert!(!primary_error.is_empty()),
        other => panic!("expected corruption, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_missing_file_is_a_missing_load_not_an_error() {
    let root = temp_dir("trust-missing");
    let path = root.join("trust.toml");
    assert_eq!(TrustStore::load(&path).expect("load"), LoadResult::Missing);
    assert_eq!(atomic::read(&path).expect("read"), None);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_wall_clock_helper_is_usable_and_monotone_enough_to_stamp_records() {
    let now = now_unix_ms();
    assert!(now > 1_600_000_000_000, "after 2020: {now}");
    assert!(now < 4_000_000_000_000, "before year 2096: {now}");
    let record = TrustedPeer::paired(node(1), "laptop-a", None, now);
    assert_eq!(record.paired_at_unix_ms, now);
    assert!(record.is_active());
    assert_eq!(
        record.fingerprint(),
        identity::fingerprint_grouped(&node(1))
    );
}

#[test]
fn write_atomic_leaves_no_temporary_file_and_keeps_the_previous_bytes() {
    let root = temp_dir("atomic");
    let path = root.join("payload.bin");
    let backup = root.join("payload.bin.bak");
    let temporary = root.join("payload.bin.tmp");

    assert_eq!(
        atomic::write_atomic(&path, b"one").expect("write"),
        WriteOutcome::Created
    );
    assert!(!backup.exists());
    assert!(!temporary.exists(), "the temporary file must not survive");

    assert_eq!(
        atomic::write_atomic(&path, b"two").expect("write"),
        WriteOutcome::Replaced {
            backup: backup.clone()
        }
    );
    assert_eq!(std::fs::read(&path).expect("read"), b"two");
    assert_eq!(std::fs::read(&backup).expect("read backup"), b"one");
    assert!(!temporary.exists());

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn read_with_backup_follows_the_documented_recovery_order() {
    let root = temp_dir("read-with-backup");
    let path = root.join("store.bin");
    let backup = root.join("store.bin.bak");
    let parse = |bytes: &[u8]| {
        std::str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(|error| error.to_string())
    };

    assert_eq!(
        atomic::read_with_backup(&path, parse).expect("read"),
        LoadResult::Missing
    );

    atomic::write_atomic(&path, b"good").expect("write");
    assert_eq!(
        atomic::read_with_backup(&path, parse).expect("read"),
        LoadResult::Loaded("good".to_owned())
    );

    atomic::write_atomic(&path, b"new").expect("write"); // moves "good" to .bak
    std::fs::write(&path, [0xff, 0xfe]).expect("corrupt primary");
    match atomic::read_with_backup(&path, parse).expect("read") {
        LoadResult::Recovered {
            value,
            backup: used,
            ..
        } => {
            assert_eq!(value, "good");
            assert_eq!(used, backup);
        }
        other => panic!("expected a recovery, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn saved_trust_is_readable_by_path_only_the_caller_chooses() {
    // The store never picks a path itself: it writes exactly where it is told, which is
    // what keeps tests (and the pairing demo) away from the real `%APPDATA%`.
    let root = temp_dir("trust-path");
    let path = root.join("nested").join("trust.toml");
    let store = TrustStore::new();
    store
        .save(&path)
        .expect("save creates the parent directory");
    assert!(path.exists());
    assert_eq!(
        store,
        TrustStore::from_toml(&std::fs::read_to_string(&path).unwrap()).unwrap()
    );
    let _ = std::fs::remove_dir_all(&root);
}
