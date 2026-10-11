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

use crate::WaylandError;

/// The TOML table that starts one grant record.
const GRANT_TABLE: &str = "[[grant]]";

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
        let mut out = String::from(
            "# LAN Workspace portal restore-token grants (KVM-017).\n\
             # SECURITY: contains bearer tokens; restrict file permissions.\n",
        );
        for grant in self.grants.values() {
            out.push('\n');
            out.push_str(GRANT_TABLE);
            out.push('\n');
            push_entry(&mut out, "portal_interface", &grant.portal_interface);
            push_entry(&mut out, "restore_token", &grant.restore_token);
            push_entry(
                &mut out,
                "compositor_signature",
                &grant.compositor_signature,
            );
        }
        out
    }

    /// Parses the text produced by [`GrantStore::to_toml`].
    ///
    /// Unknown keys, unknown tables and unterminated strings are reported as
    /// [`WaylandError::InvalidGrant`] rather than ignored. A record without a token
    /// (the portal can hand back an empty token for an expired grant) is skipped.
    pub fn from_toml(text: &str) -> Result<Self, WaylandError> {
        let mut store = Self::new();
        let mut pending: Option<PendingGrant> = None;
        for (index, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line == GRANT_TABLE {
                finish_pending(&mut store, pending.take())?;
                pending = Some(PendingGrant::default());
                continue;
            }
            let Some(record) = pending.as_mut() else {
                return Err(invalid(index, "key outside of a [[grant]] table"));
            };
            let Some((key, value)) = line.split_once('=') else {
                return Err(invalid(index, "expected `key = \"value\"`"));
            };
            let key = key.trim();
            let value = parse_string(value.trim())
                .ok_or_else(|| invalid(index, "value must be a quoted string"))?;
            match key {
                "portal_interface" => record.portal_interface = Some(value),
                "restore_token" => record.restore_token = Some(value),
                "compositor_signature" => record.compositor_signature = Some(value),
                other => return Err(invalid(index, &format!("unknown key `{other}`"))),
            }
        }
        finish_pending(&mut store, pending)?;
        Ok(store)
    }
}

/// One grant record while it is being parsed.
#[derive(Default)]
struct PendingGrant {
    portal_interface: Option<String>,
    restore_token: Option<String>,
    compositor_signature: Option<String>,
}

fn invalid(line: usize, message: &str) -> WaylandError {
    WaylandError::InvalidGrant(format!("line {}: {message}", line + 1))
}

fn finish_pending(
    store: &mut GrantStore,
    pending: Option<PendingGrant>,
) -> Result<(), WaylandError> {
    let Some(pending) = pending else {
        return Ok(());
    };
    let portal_interface = pending.portal_interface.ok_or_else(|| {
        WaylandError::InvalidGrant("grant record without portal_interface".into())
    })?;
    if portal_interface.is_empty() {
        return Err(WaylandError::InvalidGrant(
            "grant record with empty portal_interface".into(),
        ));
    }
    let restore_token = pending.restore_token.ok_or_else(|| {
        WaylandError::InvalidGrant(format!("{portal_interface}: no restore_token"))
    })?;
    if restore_token.is_empty() {
        // Tokens can legitimately come back empty (revoked/expired); such a record is
        // not a grant at all.
        return Ok(());
    }
    let compositor_signature = pending.compositor_signature.ok_or_else(|| {
        WaylandError::InvalidGrant(format!("{portal_interface}: no compositor_signature"))
    })?;
    if compositor_signature.is_empty() {
        return Err(WaylandError::InvalidGrant(format!(
            "{portal_interface}: empty compositor_signature (can never match this endpoint)"
        )));
    }
    // Loaded records are stored as-is: their own signature is what they were issued
    // for, and `GrantStore::get` is what compares it against the live endpoint.
    store.grants.insert(
        portal_interface.clone(),
        PortalGrant::new(portal_interface, restore_token, compositor_signature),
    );
    Ok(())
}

fn push_entry(out: &mut String, key: &str, value: &str) {
    out.push_str(key);
    out.push_str(" = \"");
    escape_into(out, value);
    out.push_str("\"\n");
}

fn escape_into(out: &mut String, value: &str) {
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ch.is_control() => out.push_str(&format!("\\u{:04x}", u32::from(ch))),
            ch => out.push(ch),
        }
    }
}

/// Parses a double-quoted TOML basic string. Returns `None` if it is malformed.
fn parse_string(value: &str) -> Option<String> {
    let inner = value.strip_prefix('"')?.strip_suffix('"')?;
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next()? {
            '\\' => out.push('\\'),
            '"' => out.push('"'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'u' => {
                let hex: String = chars.by_ref().take(4).collect();
                if hex.chars().count() != 4 {
                    return None;
                }
                out.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?);
            }
            _ => return None,
        }
    }
    Some(out)
}
