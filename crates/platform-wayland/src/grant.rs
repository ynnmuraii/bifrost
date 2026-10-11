//! Portal restore-token grants and their persistent store (KVM-017).
//!
//! A restore token lets the compositor skip its permission dialog after the first
//! grant, but it is issued for one portal interface on one *specific* portal endpoint.
//! The token is therefore stored together with a
//! [`compositor_signature`](crate::compositor_signature) and thrown away as soon as
//! the signature changes (compositor upgraded, portal upgraded, interface appeared or
//! disappeared).
//!
//! The token is a bearer credential: it must never be logged or exported. The [`Debug`]
//! implementations here redact it, and the only way to read it is
//! [`PortalGrant::token`]. The serialized form is for `core` to persist, and is
//! deliberately not [`Debug`]-printable through the store's `Debug` either.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::WaylandError;

/// One `PortalGrant` from `docs/ipc.proto`: the restore token for a single portal
/// interface, valid for a single compositor/portal signature.
#[derive(Clone, PartialEq, Eq)]
pub struct PortalGrant {
    /// The D-Bus interface the token belongs to (for example
    /// [`crate::INPUT_CAPTURE_INTERFACE`]).
    pub portal_interface: String,
    /// The opaque portal token. Never log or export this value.
    pub restore_token: String,
    /// [`compositor_signature`](crate::compositor_signature) at issue time.
    pub compositor_signature: String,
}

impl PortalGrant {
    /// Builds a grant.
    pub fn new(
        portal_interface: impl Into<String>,
        restore_token: impl Into<String>,
        compositor_signature: impl Into<String>,
    ) -> Self {
        Self {
            portal_interface: portal_interface.into(),
            restore_token: restore_token.into(),
            compositor_signature: compositor_signature.into(),
        }
    }

    /// The opaque token. Do not log the returned value.
    pub fn token(&self) -> &str {
        &self.restore_token
    }

    /// `true` if the grant actually carries a token.
    pub fn has_token(&self) -> bool {
        !self.restore_token.is_empty()
    }
}

/// Stands in for the token in [`Debug`] output.
struct Redacted;

impl fmt::Debug for Redacted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl fmt::Debug for PortalGrant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PortalGrant")
            .field("portal_interface", &self.portal_interface)
            .field("restore_token", &Redacted)
            .field("compositor_signature", &self.compositor_signature)
            .finish()
    }
}

/// Grants keyed by portal interface.
///
/// A grant is only handed back while its signature matches the signature of the
/// endpoint that is asking; a mismatching grant is dropped instead of returned, so a
/// stale token can never be sent to a different (or upgraded) portal.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GrantStore {
    grants: BTreeMap<String, PortalGrant>,
}

impl GrantStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of stored grants.
    #[must_use]
    pub fn len(&self) -> usize {
        self.grants.len()
    }

    /// `true` when nothing is stored.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.grants.is_empty()
    }

    /// Stores `grant` if it belongs to `current_signature` and carries a token.
    ///
    /// A grant already stored for the same interface is replaced. Anything stored for a
    /// *different* endpoint is dropped as well: the signature identifies one portal
    /// endpoint, so once it changes no stored token can still be valid, and a
    /// mismatching or empty grant always *removes* the stored entry rather than leaving
    /// a stale token behind.
    ///
    /// Returns whether the grant was accepted.
    pub fn accept(&mut self, grant: PortalGrant, current_signature: &str) -> bool {
        let usable = grant.has_token() && grant.compositor_signature == current_signature;
        if !usable {
            self.grants.remove(&grant.portal_interface);
            return false;
        }
        self.grants
            .retain(|_, stored| stored.compositor_signature == current_signature);
        self.grants.insert(grant.portal_interface.clone(), grant);
        true
    }

    /// The stored grant for `portal_interface`, if it still matches `current_signature`.
    ///
    /// A mismatching entry is dropped here as well, so callers cannot forget to.
    pub fn get(&mut self, portal_interface: &str, current_signature: &str) -> Option<&PortalGrant> {
        let stale = self
            .grants
            .get(portal_interface)
            .is_some_and(|grant| grant.compositor_signature != current_signature);
        if stale {
            self.grants.remove(portal_interface);
            return None;
        }
        self.grants
            .get(portal_interface)
            .filter(|grant| grant.has_token())
    }

    /// Removes and returns the grant for `portal_interface`.
    pub fn remove(&mut self, portal_interface: &str) -> Option<PortalGrant> {
        self.grants.remove(portal_interface)
    }

    /// Drops every grant that does not match `current_signature`.
    ///
    /// Returns how many were dropped.
    pub fn retain_signature(&mut self, current_signature: &str) -> usize {
        let before = self.grants.len();
        self.grants
            .retain(|_, grant| grant.compositor_signature == current_signature);
        before - self.grants.len()
    }

    /// Drops everything.
    pub fn clear(&mut self) {
        self.grants.clear();
    }

    /// The grants in deterministic (interface name) order.
    pub fn iter(&self) -> impl Iterator<Item = &PortalGrant> {
        self.grants.values()
    }

    /// Serializes the store for `core` to persist.
    ///
    /// # Security
    ///
    /// The returned text contains bearer tokens; treat it as a secret.
    #[must_use]
    pub fn to_toml(&self) -> String {
        let file = GrantFile {
            grant: self.grants.values().map(GrantRecord::from).collect(),
        };
        // Serializing a `Vec` of three `String`s cannot fail; the header carries the
        // security note, which the TOML encoder does not emit by itself.
        let body = toml::to_string_pretty(&file).expect("a grant store always serializes");
        format!("{FILE_HEADER}{body}")
    }

    /// Parses the text produced by [`GrantStore::to_toml`].
    ///
    /// Parsing is done by the `toml` crate with `deny_unknown_fields`, so unknown keys,
    /// unknown tables, malformed strings (an unescaped quote inside a basic string, an
    /// unterminated literal) and wrong value types are all reported as
    /// [`WaylandError::InvalidGrant`] instead of being silently accepted. A record
    /// without a token (the portal can hand back an empty token for an expired grant) is
    /// skipped.
    ///
    /// A store may only ever describe **one** portal endpoint: records carrying different
    /// `compositor_signature`s are refused here, which keeps the
    /// [`GrantStore::accept`] invariant (one signature per store) true for loaded files as
    /// well.
    pub fn from_toml(text: &str) -> Result<Self, WaylandError> {
        let file: GrantFile =
            toml::from_str(text).map_err(|error| WaylandError::InvalidGrant(error.to_string()))?;
        let mut store = Self::new();
        let mut signature: Option<&str> = None;
        for record in &file.grant {
            if record.portal_interface.is_empty() {
                return Err(WaylandError::InvalidGrant(
                    "grant record without portal_interface".into(),
                ));
            }
            if record.restore_token.is_empty() {
                // Tokens can legitimately come back empty (revoked/expired); such a record
                // is not a grant at all.
                continue;
            }
            if record.compositor_signature.is_empty() {
                return Err(WaylandError::InvalidGrant(format!(
                    "{}: empty compositor_signature (can never match this endpoint)",
                    record.portal_interface
                )));
            }
            match signature {
                None => signature = Some(&record.compositor_signature),
                Some(existing) if existing == record.compositor_signature => {}
                Some(_) => {
                    return Err(WaylandError::InvalidGrant(
                        "grant records carry different compositor signatures; a store may only \
                         hold one portal endpoint"
                            .into(),
                    ))
                }
            }
            store.grants.insert(
                record.portal_interface.clone(),
                PortalGrant::new(
                    record.portal_interface.clone(),
                    record.restore_token.clone(),
                    record.compositor_signature.clone(),
                ),
            );
        }
        Ok(store)
    }
}

/// The persisted document: a `[[grant]]` array of tables.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantFile {
    /// One record per portal interface.
    #[serde(default)]
    grant: Vec<GrantRecord>,
}

/// One persisted grant. Field names are the on-disk schema.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantRecord {
    portal_interface: String,
    restore_token: String,
    compositor_signature: String,
}

impl From<&PortalGrant> for GrantRecord {
    fn from(grant: &PortalGrant) -> Self {
        Self {
            portal_interface: grant.portal_interface.clone(),
            restore_token: grant.restore_token.clone(),
            compositor_signature: grant.compositor_signature.clone(),
        }
    }
}

/// Header of the persisted file, including the security note.
const FILE_HEADER: &str = "# LAN Workspace portal restore-token grants (KVM-017).\n\
                           # SECURITY: contains bearer tokens; restrict file permissions.\n";
