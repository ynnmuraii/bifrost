//! The ownership pair, persisted so a restart cannot replay it.
//!
//! `docs/03-architecture.md` keeps the pair in `config.toml`
//! (`workspace.owner_claim_serial` «персистится: последний известный serial владения» and
//! `workspace.owner_node_id` «последний известный owner»), and `main.rs` already feeds it
//! back at startup: `FocusRouter::with_persisted_ownership` -> `Ownership::restore`. Nothing
//! wrote it back, and that is the restart bug the E2 smoke hit: the claimant mints
//! `max_seen_serial + 1` (see `focus-router/src/wire.rs`), so a restarted node that lost the
//! pair claimed serial 1 again while the survivor still held the pair it had learned - the
//! fresh claim then looked stale (`claim.serial < max_seen_serial` is refused) and the two
//! nodes disagreed about who owned the workspace.
//!
//! This store owns exactly that write. `Session::apply` calls [`OwnershipStore::observe`]
//! after every decision batch; the pair is compared with what the file already says and the
//! file is rewritten (atomically, `.bak` kept) only when it changed.
//!
//! What the schema cannot hold: `WorkspaceConfig` requires a *consistent* pair
//! («serial > 0 requires a non-empty node_id and vice versa»), so a floor with no owner to
//! name has nowhere to live. In that case the store leaves the file alone - the documented
//! meaning of the field is «последний известный owner», and clearing it would throw away the
//! floor the next start needs.

use std::path::PathBuf;

use focus_router as fr;

/// Writes the ownership pair into `config.toml` when it changes.
pub struct OwnershipStore {
    /// The configuration file this core was started with.
    path: PathBuf,
    /// The loaded configuration, kept so a rewrite preserves every other setting in it.
    config: config::Config,
    /// The pair the file holds: `(serial, owner node id as lower-case hex)`.
    saved: (u64, String),
}

impl OwnershipStore {
    /// Seeds the store from the configuration that was just loaded.
    pub fn new(path: PathBuf, config: config::Config) -> Self {
        let saved = (
            config.workspace.owner_claim_serial,
            config.workspace.owner_node_id.clone(),
        );
        Self {
            path,
            config,
            saved,
        }
    }

    /// Persists the router's ownership pair when it changed.
    ///
    /// A failure is logged and left for the next change to retry: a configuration write
    /// must not stop the session, and the pair is still correct in memory.
    pub fn observe(&mut self, ownership: &fr::Ownership) {
        let Some(owner) = ownership.owner() else {
            // No owner to name, so no consistent pair to write; see the module note.
            return;
        };
        // `owner_claim_serial` is restored as both the floor and the owner's serial
        // (`Ownership::restore`), so the higher of the two is what has to survive.
        let pair = (
            ownership.max_seen_serial().max(ownership.serial()),
            identity::fingerprint(owner),
        );
        // A serial of 0 with a named owner is the pair the schema refuses; nothing to say.
        if pair.0 == 0 || pair == self.saved {
            return;
        }
        self.config.workspace.owner_claim_serial = pair.0;
        self.config.workspace.owner_node_id = pair.1.clone();
        match self.config.save(&self.path) {
            Ok(()) => self.saved = pair,
            Err(error) => {
                tracing::warn!(
                    %error,
                    serial = pair.0,
                    "cannot persist the ownership pair; the next change will retry"
                );
            }
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
            "lws-core-ownership-{}-{}-{label}",
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
    fn the_pair_is_written_only_when_it_changes() {
        let (path, config) = temp_config("changed");
        let mut store = OwnershipStore::new(path.clone(), config);
        let local = node(1);

        store.observe(&adopted(local, 3, local));
        let persisted = config::Config::load(&path).expect("reload");
        assert_eq!(persisted.workspace.owner_claim_serial, 3);
        assert_eq!(
            persisted.workspace.owner_node_id,
            identity::fingerprint(&local)
        );

        // A rewrite would drop this comment, so the file itself proves the point: an
        // unchanged pair must not touch it.
        let edited = format!(
            "{}# written by hand\n",
            std::fs::read_to_string(&path).expect("read")
        );
        std::fs::write(&path, &edited).expect("edit");
        store.observe(&adopted(local, 3, local));
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            edited,
            "an unchanged pair rewrote the file"
        );

        store.observe(&adopted(local, 4, local));
        let after = config::Config::load(&path).expect("reload");
        assert_eq!(after.workspace.owner_claim_serial, 4);
        assert!(
            !std::fs::read_to_string(&path)
                .expect("read")
                .contains("# written by hand"),
            "a changed pair did not rewrite the file"
        );
    }

    #[test]
    fn a_reload_keeps_the_floor_so_the_next_claim_is_max_seen_plus_one() {
        let (path, config) = temp_config("reload");
        let mut store = OwnershipStore::new(path.clone(), config);
        let local = node(1);
        store.observe(&adopted(local, 7, local));

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
    fn a_floor_without_an_owner_leaves_the_file_alone() {
        let (path, config) = temp_config("ownerless");
        let mut store = OwnershipStore::new(path.clone(), config);
        let before = std::fs::read_to_string(&path).expect("read");

        // `max_seen_serial = 5` with nobody to name: the schema's consistent pair has
        // nowhere to put it, so the last known pair stays as it is.
        let mut ownership = fr::Ownership::new(node(1), true);
        ownership.observe_serial(5);
        store.observe(&ownership);

        assert_eq!(std::fs::read_to_string(&path).expect("read"), before);
    }
}
