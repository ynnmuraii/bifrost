//! The trust store: `TRUSTED_PEER` records, administered locally.
//!
//! `docs/03-architecture.md`, ERD `TRUSTED_PEER`:
//!
//! ```text
//! TRUSTED_PEER {
//!     bytes32 peer_node_id PK
//!     string display_name
//!     enum peer_role
//!     string last_address
//!     datetime paired_at
//!     datetime revoked_at "null when active"
//!     bool clipboard_allowed
//! }
//! ```
//!
//! `peer_role` is deliberately absent: v0.3 has no per-node role («Роли source/receiver
//! стали ролями одного lease», `docs/05-protocol.md`), and `Hello.role` is `reserved`.
//! The two `datetime`s are stored as Unix milliseconds, which is the same fact in a
//! sortable, dependency-free encoding; the human-readable rendering is the GUI's job.
//!
//! Two integrity rules are enforced here:
//!
//! * «TRUSTED_PEER с `revoked_at` не может открыть сеанс» — [`TrustStore::is_trusted`]
//!   and [`TrustStore::pinned`] ignore revoked records. Transport may retain a separate,
//!   expiring pin solely to deliver `Unpaired`; it cannot establish a runtime session.
//! * «повторное сопряжение создаёт новую запись» — [`TrustStore::trust`] removes any
//!   previous record for that `node_id` and appends a fresh one, so a changed key always
//!   yields a new `paired_at` and never silently un-revokes by accident.
//!
//! Trust stays pairwise, never transitive: an authenticated `Unpaired` can revoke only
//! its sender's own pairing, not another member. Revoked records remain as local tombstones
//! so core can exclude removed peers even when an older shared layout reappears.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use geometry::NodeId;
use serde::{Deserialize, Serialize};

use crate::atomic::{self, LoadResult, WriteOutcome};
use crate::key::IdentityError;
use crate::spki::fingerprint_grouped;

/// One trusted peer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustedPeer {
    /// `node_id = SHA-256(SPKI DER)` of the peer.
    #[serde(with = "node_id_hex")]
    pub node_id: NodeId,
    /// `Hello.display_name`; non-authoritative («Название устройства/адрес не являются
    /// идентичностью»).
    pub display_name: String,
    /// Where the peer was last reachable; a routing hint, never an identity.
    pub last_address: Option<String>,
    /// When the pairing ceremony committed, in Unix milliseconds.
    pub paired_at_unix_ms: i64,
    /// When the user revoked this peer, in Unix milliseconds (`None` = active).
    pub revoked_at_unix_ms: Option<i64>,
    /// Whether clipboard is allowed with this peer (`docs/config.example.toml`,
    /// `clipboard.enabled` is still a global opt-in).
    pub clipboard_allowed: bool,
}

impl TrustedPeer {
    /// A fresh, active record for a just-paired peer.
    pub fn paired(
        node_id: NodeId,
        display_name: impl Into<String>,
        last_address: Option<String>,
        paired_at_unix_ms: i64,
    ) -> Self {
        Self {
            node_id,
            display_name: display_name.into(),
            last_address,
            paired_at_unix_ms,
            revoked_at_unix_ms: None,
            clipboard_allowed: false,
        }
    }

    /// Whether this record may open a session: «TRUSTED_PEER с `revoked_at` не может
    /// открыть сеанс».
    pub const fn is_active(&self) -> bool {
        self.revoked_at_unix_ms.is_none()
    }

    /// The grouped fingerprint, for the pairing and members UI.
    pub fn fingerprint(&self) -> String {
        fingerprint_grouped(&self.node_id)
    }
}

/// The whole store, serialised as TOML (`[[peers]]` tables).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustStore {
    /// One record per peer; a revoked peer keeps its record until the user removes it.
    #[serde(default)]
    pub peers: Vec<TrustedPeer>,
}

impl TrustStore {
    /// An empty store (the state of a node that has never paired).
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of records, active or revoked.
    pub fn len(&self) -> usize {
        self.peers.len()
    }

    /// Whether there is no record at all.
    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }

    /// The record for `node_id`, active or revoked.
    pub fn get(&self, node_id: &NodeId) -> Option<&TrustedPeer> {
        self.peers.iter().find(|peer| peer.node_id == *node_id)
    }

    /// Whether `node_id` may open a session right now (active record only).
    pub fn is_trusted(&self, node_id: &NodeId) -> bool {
        self.get(node_id).is_some_and(TrustedPeer::is_active)
    }

    /// The active records.
    pub fn active(&self) -> impl Iterator<Item = &TrustedPeer> {
        self.peers.iter().filter(|peer| peer.is_active())
    }

    /// The pinned `node_id`s of every active peer — the input of the pinned verifier.
    pub fn pinned(&self) -> BTreeSet<NodeId> {
        self.active().map(|peer| peer.node_id).collect()
    }

    /// Commits a pairing: replaces any previous record for that `node_id`.
    pub fn trust(&mut self, peer: TrustedPeer) {
        self.peers
            .retain(|existing| existing.node_id != peer.node_id);
        self.peers.push(peer);
    }

    /// Revokes `node_id`. Returns `false` when there was nothing to revoke.
    ///
    /// «Отзыв доверия закрывает оба канала, отменяет lease и запрещает reconnect» — the
    /// link teardown belongs to the caller; the store only records the decision, and
    /// [`Self::is_trusted`] then makes any reconnect fail at the pinned handshake.
    pub fn revoke(&mut self, node_id: &NodeId, at_unix_ms: i64) -> bool {
        match self.peers.iter_mut().find(|peer| peer.node_id == *node_id) {
            Some(peer) if peer.is_active() => {
                peer.revoked_at_unix_ms = Some(at_unix_ms);
                true
            }
            _ => false,
        }
    }

    /// Removes the record entirely (the GUI "forget" action).
    pub fn forget(&mut self, node_id: &NodeId) -> bool {
        let before = self.peers.len();
        self.peers.retain(|peer| peer.node_id != *node_id);
        self.peers.len() != before
    }

    /// Records the address a peer was last reached at.
    pub fn set_last_address(&mut self, node_id: &NodeId, address: impl Into<String>) -> bool {
        match self.peers.iter_mut().find(|peer| peer.node_id == *node_id) {
            Some(peer) => {
                peer.last_address = Some(address.into());
                true
            }
            None => false,
        }
    }

    /// Serialises the store as TOML.
    pub fn to_toml(&self) -> Result<String, IdentityError> {
        toml::to_string_pretty(self).map_err(|error| IdentityError::Key(error.to_string()))
    }

    /// Parses a store, rejecting anything that is not a well-formed record set.
    pub fn from_toml(text: &str) -> Result<Self, String> {
        let store: Self = toml::from_str(text).map_err(|error| error.to_string())?;
        let mut seen = BTreeSet::new();
        for peer in &store.peers {
            if !seen.insert(peer.node_id) {
                return Err(format!(
                    "duplicate TRUSTED_PEER record for {}",
                    fingerprint_grouped(&peer.node_id)
                ));
            }
        }
        Ok(store)
    }

    /// Writes the store atomically (`temp → fsync → rename`, previous version to `.bak`).
    pub fn save(&self, path: &Path) -> Result<WriteOutcome, IdentityError> {
        let text = self.to_toml()?;
        Ok(atomic::write_atomic(path, text.as_bytes())?)
    }

    /// Reads the store, falling back to `.bak` when the primary file does not parse
    /// («При ошибке разбора: загрузить `.bak`, стартовать в Local, показать
    /// предупреждение»).
    pub fn load(path: &Path) -> Result<LoadResult<Self>, IdentityError> {
        Ok(atomic::read_with_backup(path, |bytes| {
            let text = std::str::from_utf8(bytes).map_err(|error| error.to_string())?;
            Self::from_toml(text)
        })?)
    }
}

/// Current wall-clock time in Unix milliseconds, for the `datetime` columns.
pub fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// `NodeId` as 64 lower-case hex characters in TOML.
mod node_id_hex {
    use geometry::NodeId;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(node_id: &NodeId, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&super::super::spki::fingerprint(node_id))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<NodeId, D::Error> {
        let text = String::deserialize(deserializer)?;
        let bytes = parse_hex(&text).map_err(serde::de::Error::custom)?;
        Ok(NodeId::new(bytes))
    }

    fn parse_hex(text: &str) -> Result<[u8; 32], String> {
        if text.len() != 64 {
            return Err(format!(
                "node_id must be 64 hex characters, got {}",
                text.len()
            ));
        }
        let mut bytes = [0u8; 32];
        for (index, slot) in bytes.iter_mut().enumerate() {
            let pair = &text[index * 2..index * 2 + 2];
            *slot = u8::from_str_radix(pair, 16)
                .map_err(|_| format!("node_id has a non-hex pair {pair:?}"))?;
        }
        Ok(bytes)
    }
}
