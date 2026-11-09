//! Monotonic ownership persistence without blocking the session decision loop.
//!
//! One writer merges the latest pending pair into the locked configuration.
//! Orderly shutdown drains it; an ownerless floor keeps the last named owner.

use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use focus_router as fr;
use geometry::NodeId;

type Pair = (u64, NodeId);

struct Pending {
    saved: Option<Pair>,
    writing: Option<Pair>,
    next: Option<Pair>,
    stopping: bool,
}

/// Coalesces ownership changes into one pending atomic read-modify-write.
pub struct OwnershipStore {
    pending: Arc<(Mutex<Pending>, Condvar)>,
    worker: Option<JoinHandle<()>>,
}

impl OwnershipStore {
    /// Seeds the last persisted pair from the configuration just loaded.
    pub fn new(path: PathBuf, config: &config::Config) -> Self {
        let pending = Arc::new((
            Mutex::new(Pending {
                saved: crate::owner_pair(config),
                writing: None,
                next: None,
                stopping: false,
            }),
            Condvar::new(),
        ));
        let queue = Arc::clone(&pending);
        let worker = std::thread::spawn(move || write_pending(path, queue));
        Self {
            pending,
            worker: Some(worker),
        }
    }

    /// Queues the latest named pair; no filesystem operation runs on this caller.
    pub fn observe(&mut self, ownership: &fr::Ownership) {
        let Some(owner) = ownership.owner() else {
            return;
        };
        // A pending claim raises the high-water mark without adopting its owner.
        let pair = (ownership.serial(), *owner);
        if pair.0 == 0 {
            return;
        }
        let (lock, ready) = &*self.pending;
        let mut pending = lock.lock().expect("ownership queue");
        if [pending.saved, pending.writing, pending.next]
            .into_iter()
            .flatten()
            .any(|known| known >= pair)
        {
            return;
        }
        pending.next = Some(pair);
        ready.notify_one();
    }
}

impl Drop for OwnershipStore {
    fn drop(&mut self) {
        let (lock, ready) = &*self.pending;
        {
            let mut pending = lock.lock().expect("ownership queue");
            pending.stopping = true;
            ready.notify_one();
        }
        // A normal restart must see the newest observed serial, not only the
        // last write that happened to finish before shutdown.
        if let Some(worker) = self.worker.take() {
            if worker.join().is_err() {
                tracing::error!("ownership persistence worker panicked");
            }
        }
    }
}

fn write_pending(path: PathBuf, queue: Arc<(Mutex<Pending>, Condvar)>) {
    let (lock, ready) = &*queue;
    loop {
        let pair = {
            let mut pending = lock.lock().expect("ownership queue");
            while pending.next.is_none() && !pending.stopping {
                pending = ready.wait(pending).expect("ownership queue");
            }
            let Some(pair) = pending.next.take() else {
                return;
            };
            pending.writing = Some(pair);
            pair
        };
        let result = config::lock::update_locked(&path, |config| {
            // Another process may have persisted a newer floor while this
            // write was pending; never replace it with an older observation.
            if let Some(saved) = crate::owner_pair(config) {
                if saved > pair {
                    return saved;
                }
            }
            config.workspace.owner_claim_serial = pair.0;
            config.workspace.owner_node_id = identity::fingerprint(&pair.1);
            pair
        });
        if let Err(error) = &result {
            tracing::warn!(
                %error,
                serial = pair.0,
                "cannot persist ownership; a later observation may retry"
            );
        }
        let mut pending = lock.lock().expect("ownership queue");
        pending.writing = None;
        if let Ok(saved) = result {
            pending.saved = Some(saved);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use geometry::NodeId;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// A fresh directory holding a valid `config.toml`, and the configuration loaded from
    /// it - the same pair `main.rs` starts from.
    fn temp_config(label: &str) -> (PathBuf, config::Config) {
        let dir = std::env::temp_dir().join(format!(
            "bifrost-core-ownership-{}-{}-{label}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temporary directory");
        let path = dir.join("config.toml");
        // The smallest document `crates/config` accepts (`tests/validation.rs`).
        std::fs::write(
            &path,
            "config_version = 2\n[network]\nlisten_address = \"192.168.1.10\"\n",
        )
        .expect("write the initial configuration");
        let config = config::Config::load(&path).expect("load the initial configuration");
        (path, config)
    }

    fn node(byte: u8) -> NodeId {
        NodeId::new([byte; 32])
    }

    /// An ownership machine that has adopted `serial` for `owner`.
    fn adopted(local: NodeId, serial: u64, owner: NodeId) -> fr::Ownership {
        let mut ownership = fr::Ownership::new(local, true);
        ownership.adopt(serial, owner);
        ownership
    }

    #[test]
    fn shutdown_persists_the_latest_owner_without_regressing_its_floor() {
        let (path, config) = temp_config("latest");
        let local = node(1);
        let mut store = OwnershipStore::new(path.clone(), &config);
        store.observe(&adopted(local, 3, local));
        store.observe(&adopted(local, 4, node(2)));
        store.observe(&adopted(local, 2, local));
        store.observe(&adopted(local, 4, local));
        drop(store);
        let persisted = config::Config::load(&path).expect("reload");
        assert_eq!(persisted.workspace.owner_claim_serial, 4);
        assert_eq!(
            persisted.workspace.owner_node_id,
            identity::fingerprint(&node(2))
        );
    }

    #[test]
    fn observations_during_file_contention_are_coalesced_and_flushed() {
        let (path, config) = temp_config("contended");
        let held = config::lock::FileLock::acquire(&path).expect("competing writer");
        let local = node(1);
        let mut store = OwnershipStore::new(path.clone(), &config);
        store.observe(&adopted(local, 1, local));
        store.observe(&adopted(local, 2, node(2)));
        assert_eq!(
            config::Config::load(&path)
                .unwrap()
                .workspace
                .owner_claim_serial,
            0
        );
        drop(held);
        drop(store);
        let persisted = config::Config::load(&path).expect("reload");
        assert_eq!(persisted.workspace.owner_claim_serial, 2);
        assert_eq!(
            persisted.workspace.owner_node_id,
            identity::fingerprint(&node(2))
        );
    }

    #[test]
    fn a_later_external_writer_keeps_its_newer_ownership_and_settings() {
        let (path, config) = temp_config("external");
        let mut store = OwnershipStore::new(path.clone(), &config);
        config::lock::update_locked(&path, |config| {
            config.workspace.owner_claim_serial = 9;
            config.workspace.owner_node_id = identity::fingerprint(&node(2));
            config.node.claim_on_startup = false;
        })
        .expect("external writer");
        store.observe(&adopted(node(1), 8, node(1)));
        drop(store);
        let persisted = config::Config::load(&path).expect("reload");
        assert_eq!(persisted.workspace.owner_claim_serial, 9);
        assert_eq!(
            persisted.workspace.owner_node_id,
            identity::fingerprint(&node(2))
        );
        assert!(!persisted.node.claim_on_startup);
    }

    #[test]
    fn a_reload_keeps_the_floor_so_the_next_claim_is_max_seen_plus_one() {
        let (path, config) = temp_config("reload");
        let mut store = OwnershipStore::new(path.clone(), &config);
        let local = node(1);
        store.observe(&adopted(local, 7, local));
        drop(store);

        let reloaded = config::Config::load(&path).expect("reload");
        assert_eq!(reloaded.workspace.owner_claim_serial, 7);
        assert_eq!(
            reloaded.workspace.owner_node_id,
            identity::fingerprint(&local)
        );

        // What `main.rs` does at startup, and what the claim guard compares against:
        // after the restart serial 1 is stale, and the next claim is 8, not 1.
        let restored = fr::Ownership::new(local, true).restore(
            reloaded.workspace.owner_claim_serial,
            Some((reloaded.workspace.owner_claim_serial, local)),
        );
        assert_eq!(restored.max_seen_serial(), 7, "the floor was lost");
        assert_eq!(
            restored.max_seen_serial() + 1,
            8,
            "the next claim is max_seen_serial + 1"
        );
        assert!(
            !restored.is_newer_than_seen(1, &local),
            "serial 1 is stale after the restart"
        );
    }

    #[test]
    fn alternating_store_writes_preserve_ownership_layout_and_external_settings() {
        let (path, config) = temp_config("interleaved");
        let local = node(1);
        // Both stores start with the same snapshot, as they do in the running core.
        let mut layout_store = crate::layout_store::LayoutStore::new(path.clone(), &config, local);
        let mut layout = layout_store.restored();

        for serial in 1..=3 {
            let mut ownership_store = OwnershipStore::new(path.clone(), &config);
            ownership_store.observe(&adopted(local, serial, local));
            drop(ownership_store);
            let persisted = config::Config::load(&path).expect("reload after ownership");
            assert_eq!(persisted.workspace.owner_claim_serial, serial);
            assert_eq!(persisted.topology.revision, serial - 1);
            if serial > 1 {
                assert!(!persisted.node.claim_on_startup);
                assert_eq!(
                    persisted.network.listen_address,
                    format!("192.168.1.{}", 9 + serial)
                );
            }

            // A settings editor writes between the two core stores.
            let mut edited = persisted;
            edited.node.claim_on_startup = false;
            edited.network.listen_address = format!("192.168.1.{}", 10 + serial);
            edited.save(&path).expect("save external settings");

            let outcome = layout.apply_edit(
                &crate::layout::Edit {
                    expected: layout.applied(),
                    editor: local,
                    layout_toml: "links = []".to_owned(),
                },
                local,
                true,
                |_| Ok(()),
            );
            assert!(matches!(outcome, crate::layout::EditOutcome::Applied(_)));
            layout_store.observe(&layout, &config::TopologyConfig::default());

            let persisted = config::Config::load(&path).expect("reload after layout");
            assert_eq!(persisted.workspace.owner_claim_serial, serial);
            assert_eq!(
                persisted.workspace.owner_node_id,
                identity::fingerprint(&local)
            );
            assert_eq!(persisted.topology.revision, serial);
            assert_eq!(persisted.topology.max_seen_revision, serial);
            assert!(!persisted.node.claim_on_startup);
            assert_eq!(
                persisted.network.listen_address,
                format!("192.168.1.{}", 10 + serial)
            );

            // Exercise the same reload views the next core consumes.
            let restored_ownership = fr::Ownership::new(local, true).restore(
                persisted.workspace.owner_claim_serial,
                Some((persisted.workspace.owner_claim_serial, local)),
            );
            assert_eq!(restored_ownership.serial(), serial);
            assert_eq!(restored_ownership.max_seen_serial(), serial);
            let restored_layout =
                crate::layout_store::LayoutStore::new(path.clone(), &persisted, local).restored();
            assert_eq!(restored_layout.applied(), layout.applied());
            assert_eq!(restored_layout.max_seen(), layout.max_seen());
        }
    }

    #[test]
    fn a_floor_without_an_owner_leaves_the_file_alone() {
        let (path, config) = temp_config("ownerless");
        let mut store = OwnershipStore::new(path.clone(), &config);
        let before = std::fs::read_to_string(&path).expect("read");

        // `max_seen_serial = 5` with nobody to name: the schema's consistent pair has
        // nowhere to put it, so the last known pair stays as it is.
        let mut ownership = fr::Ownership::new(node(1), true);
        ownership.observe_serial(5);
        store.observe(&ownership);

        assert_eq!(std::fs::read_to_string(&path).expect("read"), before);
    }
}
