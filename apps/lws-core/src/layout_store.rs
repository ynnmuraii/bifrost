//! Persists the applied topology document and revision floor in `[topology]` of `config.toml`.
//!
//! `docs/06-development-plan.md`, KVM-072: «персистятся `max_seen_revision`/`max_seen_author`,
//! ревизия назад не идёт». The file is the same one the rest of the configuration lives in, so
//! the store reloads the latest [`config::Config`] before each rewrite, preserving unrelated
//! settings while replacing the topology section.
//!
//! The authors are stored as the lower-case hex fingerprint (`identity::fingerprint`), which is
//! what the schema asks for and what a human can compare; [`node_id_from_hex`] is the way back,
//! and an unparsable author is treated as "no revision" rather than as a panic on startup.

use std::path::PathBuf;

use geometry::NodeId;

use crate::layout::{Layout, Pair};

/// The `[topology]` pair and floor, persisted across restarts.
pub struct LayoutStore {
    /// The configuration file this core was started with.
    path: PathBuf,
    /// What the file holds, so an unchanged pair is not rewritten.
    saved: (Pair, Pair),
}

impl LayoutStore {
    /// Seeds the store from the configuration that was just loaded.
    ///
    /// `self_node` is what the configuration means by `"self"`: a document authored here is
    /// stored with this node's own fingerprint, and a hand-written `"self"` must mean the same
    /// thing rather than being dropped as unparsable.
    pub fn new(path: PathBuf, config: &config::Config, self_node: NodeId) -> Self {
        let applied = pair_from_config(
            config.topology.revision,
            config.topology.author.as_str(),
            self_node,
        );
        let max_seen = pair_from_config(
            config.topology.max_seen_revision,
            config.topology.max_seen_author.as_str(),
            self_node,
        );
        Self {
            path,
            saved: (applied, max_seen),
        }
    }

    /// The view restored from the file: the applied revision and the floor.
    #[must_use]
    pub fn restored(&self) -> Layout {
        Layout::restore(self.saved.0, self.saved.1)
    }

    /// Writes a divergent local edit to `layout.pending.toml` (KVM-072).
    ///
    /// The path comes from `config::Paths` and lives next to the applied document; the caller
    /// owns the in-memory copy ([`crate::layout::Layout::hold_pending`]), this only survives a
    /// restart.
    pub fn write_pending(&self, layout_toml: &str) {
        let path = self.pending_path();
        if let Err(error) = config::save::save_atomic(&path, layout_toml) {
            tracing::warn!(%error, "cannot write the pending layout edit");
        }
    }

    /// Removes `layout.pending.toml` — the other half of [`Self::write_pending`].
    ///
    /// The file and the in-memory slot are one fact, so the file is written when an edit enters
    /// the slot ([`crate::layout::Layout::hold_pending`]) and removed when the slot clears: a
    /// silent `layout.pending.toml` left behind would tell the next reader (the GUI, a human)
    /// that an unapplied edit exists when the node holds none.
    ///
    /// `remove_file` is the atomic delete on both platforms — unlike a write there is no
    /// half-written state to protect against — and a file that is already gone is the normal
    /// case, not a failure.
    pub fn clear_pending(&self) {
        let path = self.pending_path();
        match std::fs::remove_file(&path) {
            Ok(()) => {
                tracing::debug!(path = %path.display(), "the pending layout edit was removed");
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(
                    %error,
                    path = %path.display(),
                    "cannot remove the pending layout edit"
                );
            }
        }
    }

    /// Where `layout.pending.toml` lives: next to the configuration file it belongs to.
    fn pending_path(&self) -> PathBuf {
        self.path
            .parent()
            .map(|dir| dir.join(config::paths::LAYOUT_PENDING_FILE))
            .unwrap_or_else(|| PathBuf::from(config::paths::LAYOUT_PENDING_FILE))
    }

    /// Writes the applied document and revision pair back when the pair changes.
    ///
    /// A failure is logged and left for the next change to retry: a configuration write must
    /// not stop the session, and the pair is still correct in memory.
    pub fn observe(&mut self, layout: &Layout, topology: &config::TopologyConfig) {
        let applied = layout.applied();
        let max_seen = layout.max_seen();
        if (applied, max_seen) == self.saved {
            return;
        }
        let _write_guard = crate::ownership_store::CONFIG_WRITE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut config = match config::Config::load(&self.path) {
            Ok(config) => config,
            Err(error) => {
                tracing::warn!(
                    %error,
                    revision = applied.revision,
                    "cannot reload configuration to persist layout; the next change will retry"
                );
                return;
            }
        };
        config.topology = topology.clone();
        config.topology.revision = applied.revision;
        config.topology.author = applied
            .author
            .map_or_else(String::new, |id| identity::fingerprint(&id));
        config.topology.max_seen_revision = max_seen.revision;
        config.topology.max_seen_author = max_seen
            .author
            .map_or_else(String::new, |id| identity::fingerprint(&id));
        match config.save(&self.path) {
            Ok(()) => self.saved = (applied, max_seen),
            Err(error) => tracing::warn!(
                %error,
                revision = applied.revision,
                "cannot persist the layout revision; the next change will retry"
            ),
        }
    }
}

/// A pair from the two `[topology]` fields; revision 0 means "no document", so a missing or
/// unparsable author is not an error there.
fn pair_from_config(revision: u64, author: &str, self_node: NodeId) -> Pair {
    if author.trim() == "self" {
        return Pair::new(revision, self_node);
    }
    match node_id_from_hex(author) {
        Some(author) => Pair::new(revision, author),
        None if revision == 0 => Pair::NONE,
        None => {
            tracing::warn!(
                revision,
                author,
                "the stored layout author is not a node id; treating the revision as unseen"
            );
            Pair::NONE
        }
    }
}

/// The 32 identity bytes of a lower-case hex fingerprint.
pub(crate) fn node_id_from_hex(text: &str) -> Option<NodeId> {
    let text = text.trim();
    if text.len() != 64 {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(text.get(index * 2..index * 2 + 2)?, 16).ok()?;
    }
    Some(NodeId::new(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn temp_config() -> (PathBuf, config::Config) {
        static NEXT: AtomicU32 = AtomicU32::new(1);
        let dir = std::env::temp_dir().join(format!(
            "lws-layout-store-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("a temp directory");
        let path = dir.join("config.toml");
        let mut config = config::Config::default();
        config.network.listen_address = "127.0.0.1".to_owned();
        config.save(&path).expect("a writable config");
        (path, config)
    }

    fn node(byte: u8) -> NodeId {
        let mut id = [0u8; 32];
        id[0] = byte;
        NodeId::new(id)
    }

    /// The hex round-trip the schema depends on.
    #[test]
    fn a_fingerprint_parses_back() {
        let id = node(0xab);
        let text = identity::fingerprint(&id);
        assert_eq!(node_id_from_hex(&text), Some(id));
        assert_eq!(node_id_from_hex("nonsense"), None);
        assert_eq!(node_id_from_hex(&text[..63]), None);
        assert_eq!(
            node_id_from_hex(&text.to_uppercase()),
            Some(id),
            "reading is lenient about case; writing is not"
        );
    }

    /// The pending file appears with the edit and goes away with the slot, so
    /// «a `layout.pending.toml` is here» always means «this node holds an unapplied edit».
    /// (The file is not read back yet; the pairing is what keeps it from lying in the
    /// meantime.)
    #[test]
    fn the_pending_file_follows_the_slot() {
        let (path, config) = temp_config();
        let store = LayoutStore::new(path, &config, node(7));
        let pending = store.pending_path();

        // Nothing pending: removing nothing is the normal case, not an error.
        store.clear_pending();
        assert!(!pending.exists(), "{pending:?} must not exist yet");

        store.write_pending("links = []");
        assert_eq!(
            std::fs::read_to_string(&pending).expect("the pending edit is written"),
            "links = []"
        );

        store.clear_pending();
        assert!(!pending.exists(), "the file went with the slot");
        // And it stays gone: a second clear is what every release path may call.
        store.clear_pending();
        assert!(!pending.exists());
    }

    /// A stored pair survives a restart: the file is what the next core reads.
    #[test]
    fn the_pair_survives_a_reload() {
        let (path, config) = temp_config();
        // `self_node` is this node, the same one the reload below names; the pair that comes
        // back is the one `observe` wrote, so the identity is only here to resolve `"self"`.
        let mut store = LayoutStore::new(path.clone(), &config, node(7));
        let mut layout = store.restored();
        assert_eq!(layout.applied(), Pair::NONE);

        let outcome = layout.apply_edit(
            &crate::layout::Edit {
                expected: Pair::NONE,
                editor: node(9),
                layout_toml: "links = []".to_owned(),
            },
            node(1),
            true,
            |_| Ok(()),
        );
        assert!(matches!(outcome, crate::layout::EditOutcome::Applied(_)));
        store.observe(&layout, &config::TopologyConfig::default());

        let reloaded = config::Config::load(&path).expect("the file still parses");
        let restored = LayoutStore::new(path, &reloaded, node(7)).restored();
        assert_eq!(restored.applied(), Pair::new(1, node(1)));
        assert_eq!(restored.max_seen(), Pair::new(1, node(1)));
    }

    /// A hand-edited file whose `max_seen` trails the applied revision cannot lower the floor.
    #[test]
    fn a_hand_edited_floor_is_raised() {
        let (path, mut config) = temp_config();
        config.topology.revision = 5;
        config.topology.author = identity::fingerprint(&node(1));
        config.topology.max_seen_revision = 2;
        config.topology.max_seen_author = identity::fingerprint(&node(2));
        let store = LayoutStore::new(path, &config, node(7));
        assert_eq!(store.restored().max_seen(), Pair::new(5, node(1)));
    }
}
