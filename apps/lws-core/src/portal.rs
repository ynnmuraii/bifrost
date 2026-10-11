//! Portal restore tokens: `portal-restore.toml`, owned by core.
//!
//! `docs/03-architecture.md`, «Разрешения и персистентность»: «Токен хранит core в
//! `portal-restore.toml` (0600, каталог 0700) в `XDG_STATE_HOME`, поэтому он переживает
//! перезагрузку, но не переживает смену композитора: при несовпадении
//! `compositor_signature` токен отбрасывается, а не переиспользуется».
//!
//! Two rules from the same document shape the API:
//!
//! * «Секреты (`private_key_ref`, `PORTAL_GRANT.restore_token`) никогда не попадают в
//!   diagnostic export» — the token is a secret, so [`StoredGrant`]'s `Debug` prints a
//!   placeholder and the value is never part of a log line or a status string.
//! * the file belongs to core alone; the agent receives the tokens in
//!   `AgentAttach.restore_tokens` and hands new ones back as `PortalGrantChanged`.
//!
//! The file is Linux-only in practice (only Wayland has portals). The type itself is
//! platform-neutral so its logic can be unit-tested anywhere; on a non-Unix host
//! [`PortalGrants::save`] refuses to create a file that nothing would ever read.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Mode of the token file: owner read/write only (`0600`).
#[cfg(unix)]
const FILE_MODE: u32 = 0o600;
/// Mode of the directory holding it: owner only (`0700`).
#[cfg(unix)]
const DIR_MODE: u32 = 0o700;

/// The on-disk shape of one grant (`[[grant]]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileGrant {
    portal_interface: String,
    restore_token: String,
    compositor_signature: String,
}

/// The on-disk document.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileBody {
    #[serde(default)]
    grant: Vec<FileGrant>,
}

/// One stored token.
///
/// `Debug` is hand-written: the token must never reach a log, a diagnostic export or
/// a panic message.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredGrant {
    /// The D-Bus interface the token belongs to (`org.freedesktop.portal.InputCapture`
    /// or `…RemoteDesktop`).
    pub portal_interface: String,
    /// The opaque portal token. A secret: never logged, never exported.
    pub restore_token: String,
    /// The compositor/portal signature at issue time; a mismatch means the token is
    /// dropped rather than reused.
    pub compositor_signature: String,
}

impl fmt::Debug for StoredGrant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoredGrant")
            .field("portal_interface", &self.portal_interface)
            .field("restore_token", &"<redacted>")
            .field("compositor_signature", &self.compositor_signature)
            .finish()
    }
}

impl StoredGrant {
    /// Converts to the wire message `AgentAttach.restore_tokens` carries.
    pub fn to_ipc(&self) -> protocol::ipc::PortalGrant {
        protocol::ipc::PortalGrant {
            portal_interface: self.portal_interface.clone(),
            restore_token: self.restore_token.clone(),
            compositor_signature: self.compositor_signature.clone(),
        }
    }
}

/// The portals' restore tokens, keyed by interface (one token per interface).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PortalGrants {
    path: Option<PathBuf>,
    by_interface: BTreeMap<String, StoredGrant>,
}

impl PortalGrants {
    /// Reads `path`; a missing file is an empty store, and a malformed one is an empty
    /// store plus a warning — the documents make a broken file fall back rather than
    /// stop the node («При ошибке разбора: загрузить `.bak`…»).
    pub fn load(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Self {
                    path: Some(path),
                    by_interface: BTreeMap::new(),
                }
            }
            Err(error) => {
                tracing::warn!(error = %error, "cannot read the portal token file; starting empty");
                return Self {
                    path: Some(path),
                    by_interface: BTreeMap::new(),
                };
            }
        };
        match Self::parse(&text) {
            Ok(mut grants) => {
                grants.path = Some(path);
                grants
            }
            Err(error) => {
                tracing::warn!(error = %error, "malformed portal token file; starting empty");
                Self {
                    path: Some(path),
                    by_interface: BTreeMap::new(),
                }
            }
        }
    }

    /// Parses the document body.
    pub fn parse(text: &str) -> Result<Self, String> {
        let body: FileBody = toml::from_str(text).map_err(|error| error.to_string())?;
        let mut by_interface = BTreeMap::new();
        for grant in body.grant {
            if grant.portal_interface.is_empty() || grant.restore_token.is_empty() {
                continue;
            }
            by_interface.insert(
                grant.portal_interface.clone(),
                StoredGrant {
                    portal_interface: grant.portal_interface,
                    restore_token: grant.restore_token,
                    compositor_signature: grant.compositor_signature,
                },
            );
        }
        Ok(Self {
            path: None,
            by_interface,
        })
    }

    /// Renders the document body. Only the platform that persists the file needs it,
    /// plus the tests on every platform.
    #[cfg(any(unix, test))]
    pub fn to_toml(&self) -> String {
        let body = FileBody {
            grant: self
                .by_interface
                .values()
                .map(|grant| FileGrant {
                    portal_interface: grant.portal_interface.clone(),
                    restore_token: grant.restore_token.clone(),
                    compositor_signature: grant.compositor_signature.clone(),
                })
                .collect(),
        };
        toml::to_string(&body).unwrap_or_default()
    }

    /// How many tokens are stored.
    pub fn len(&self) -> usize {
        self.by_interface.len()
    }

    /// Whether nothing is stored.
    pub fn is_empty(&self) -> bool {
        self.by_interface.is_empty()
    }

    /// The token of one interface. Only the tests need the value itself: `status`
    /// shows [`PortalGrants::interfaces`], never a token.
    #[cfg(test)]
    pub fn get(&self, portal_interface: &str) -> Option<&StoredGrant> {
        self.by_interface.get(portal_interface)
    }

    /// The interfaces present, for the status line.
    pub fn interfaces(&self) -> Vec<&str> {
        self.by_interface.keys().map(String::as_str).collect()
    }

    /// `AgentAttach.restore_tokens`.
    pub fn to_ipc(&self) -> Vec<protocol::ipc::PortalGrant> {
        self.by_interface
            .values()
            .map(StoredGrant::to_ipc)
            .collect()
    }

    /// Stores a token the agent received; returns whether the store changed.
    ///
    /// An empty interface or token is refused: a grant without a token would make the
    /// portal ask again while looking like persistence.
    pub fn accept(&mut self, grant: protocol::ipc::PortalGrant) -> bool {
        if grant.portal_interface.is_empty() || grant.restore_token.is_empty() {
            return false;
        }
        let stored = StoredGrant {
            portal_interface: grant.portal_interface.clone(),
            restore_token: grant.restore_token,
            compositor_signature: grant.compositor_signature,
        };
        match self.by_interface.get(&grant.portal_interface) {
            Some(existing) if *existing == stored => false,
            _ => {
                self.by_interface.insert(grant.portal_interface, stored);
                true
            }
        }
    }

    /// Drops one interface's token (a revocation, or a stale compositor signature).
    pub fn revoke(&mut self, portal_interface: &str) -> bool {
        self.by_interface.remove(portal_interface).is_some()
    }

    /// Drops everything (`ForgetPortalGrants` / `ResetPortalGrants`).
    pub fn clear(&mut self) {
        self.by_interface.clear();
    }

    /// Writes the store back atomically (temp file, fsync, rename) with owner-only
    /// permissions.
    ///
    /// On a non-Unix host this is a no-op: no compositor will ever hand out a portal
    /// token there, and creating an empty secrets file would be worse than not having
    /// one.
    #[cfg(unix)]
    pub fn save(&self) -> Result<(), String> {
        let Some(path) = self.path.as_deref() else {
            return Ok(());
        };
        // Nothing to say and nothing to replace: do not create an empty secrets file.
        if self.is_empty() && !path.exists() {
            return Ok(());
        }
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            if !parent.exists() {
                use std::os::unix::fs::DirBuilderExt;
                std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(DIR_MODE)
                    .create(parent)
                    .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
            }
        }
        config::save_atomic(path, &self.to_toml())
            .map_err(|error| format!("cannot write the portal token file: {error}"))?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(FILE_MODE))
            .map_err(|error| format!("cannot set {FILE_MODE:o} on {}: {error}", path.display()))?;
        Ok(())
    }

    /// See the Unix version: nothing is persisted on a non-Unix host.
    #[cfg(not(unix))]
    pub fn save(&self) -> Result<(), String> {
        if !self.is_empty() {
            tracing::debug!("portal tokens are not persisted on this host");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant(interface: &str, token: &str, signature: &str) -> protocol::ipc::PortalGrant {
        protocol::ipc::PortalGrant {
            portal_interface: interface.to_owned(),
            restore_token: token.to_owned(),
            compositor_signature: signature.to_owned(),
        }
    }

    #[test]
    fn a_document_round_trips() {
        let mut grants = PortalGrants::default();
        assert!(grants.accept(grant(
            "org.freedesktop.portal.InputCapture",
            "tok-1",
            "gnome-46"
        )));
        assert!(grants.accept(grant(
            "org.freedesktop.portal.RemoteDesktop",
            "tok-2",
            "gnome-46"
        )));

        let text = grants.to_toml();
        let reparsed = PortalGrants::parse(&text).expect("the document we wrote must parse");
        assert_eq!(reparsed.len(), 2);
        assert_eq!(
            reparsed.get("org.freedesktop.portal.InputCapture"),
            grants.get("org.freedesktop.portal.InputCapture")
        );
        assert!(reparsed.get("nothing.provided").is_none());

        // `to_ipc` carries exactly the three fields the agent needs.
        let ipc = reparsed.to_ipc();
        assert_eq!(ipc.len(), 2);
        for message in &ipc {
            assert!(!message.portal_interface.is_empty());
            assert!(!message.restore_token.is_empty());
            assert_eq!(message.compositor_signature, "gnome-46");
        }
    }

    #[test]
    fn accepting_the_same_token_twice_changes_nothing() {
        let mut grants = PortalGrants::default();
        assert!(grants.accept(grant("iface", "tok", "sig")));
        assert!(!grants.accept(grant("iface", "tok", "sig")), "no change");
        assert!(
            grants.accept(grant("iface", "tok-2", "sig")),
            "a new token replaces"
        );
        assert_eq!(
            grants.get("iface").map(|g| g.restore_token.as_str()),
            Some("tok-2")
        );

        // A grant without a token is not persistence and is refused.
        assert!(!grants.accept(grant("iface", "", "sig")));
        assert!(!grants.accept(grant("", "tok", "sig")));
        assert_eq!(
            grants.get("iface").map(|g| g.restore_token.as_str()),
            Some("tok-2")
        );
    }

    #[test]
    fn revocation_and_clear() {
        let mut grants = PortalGrants::default();
        grants.accept(grant("input", "tok-1", "gnome-46"));
        grants.accept(grant("desktop", "tok-2", "gnome-46"));

        // A foreign compositor's signature is kept as issued: the *agent* decides
        // whether a token matches the running compositor
        // (`platform_wayland::GrantStore`), so core does not throw it away here.
        assert_eq!(grants.len(), 2);
        assert!(grants.revoke("input"));
        assert!(!grants.revoke("input"), "revoking twice is not a change");
        assert_eq!(grants.interfaces(), vec!["desktop"]);

        grants.clear();
        assert!(grants.is_empty());
    }

    #[test]
    fn a_malformed_or_empty_document_is_an_empty_store() {
        assert!(PortalGrants::parse("").expect("empty is valid").is_empty());
        // A completed record with an empty token is not persistence: it is skipped, so
        // the portal is asked again instead of looking persisted.
        let skipped = PortalGrants::parse(
            "[[grant]]\nportal_interface = \"iface\"\nrestore_token = \"\"\ncompositor_signature = \"sig\"\n",
        )
        .expect("a syntactically valid document parses");
        assert!(skipped.is_empty());
        // A record that is missing a field is a malformed document, not a skipped one:
        // a truncated file must not be read as "no tokens to restore".
        assert!(PortalGrants::parse("[[grant]]\nportal_interface = \"x\"\n").is_err());
        assert!(PortalGrants::parse("not toml at all").is_err());
        // An unknown key is a typo, not something to guess about.
        assert!(PortalGrants::parse("something_else = 1").is_err());
    }

    /// `docs/03-architecture.md`: the file is `0600` in a `0700` directory, and a
    /// missing file simply means "nothing to restore".
    #[cfg(unix)]
    #[test]
    fn save_writes_owner_only_and_load_reads_it_back() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("lws-portal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("portal-restore.toml");

        // Nothing there yet: an empty store bound to the path.
        let mut grants = PortalGrants::load(&path);
        assert!(grants.is_empty());
        assert!(!dir.exists(), "loading must not create anything");

        // An empty store does not create an empty secrets file.
        grants.save().expect("an empty save is a no-op");
        assert!(!path.exists());

        grants.accept(grant(
            "org.freedesktop.portal.InputCapture",
            "tok-1",
            "gnome-46",
        ));
        grants.save().expect("save");

        let mode = |path: &std::path::Path| {
            std::fs::metadata(path)
                .expect("exists")
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode(&dir), 0o700, "the directory is owner-only");
        assert_eq!(mode(&path), 0o600, "the token file is owner-only");

        // What was written is what is read back, with the three fields intact.
        let reloaded = PortalGrants::load(&path);
        assert_eq!(reloaded.len(), 1);
        assert_eq!(reloaded.to_ipc(), grants.to_ipc());
        let text = std::fs::read_to_string(&path).expect("readable");
        assert!(text.contains("restore_token"), "{text}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_token_never_appears_in_debug_output() {
        let mut grants = PortalGrants::default();
        grants.accept(grant(
            "org.freedesktop.portal.InputCapture",
            "s3cr3t-token",
            "gnome-46",
        ));
        let debug = format!("{grants:?}");
        assert!(!debug.contains("s3cr3t-token"), "{debug}");
        assert!(debug.contains("<redacted>"));
        let stored = format!(
            "{:?}",
            grants.get("org.freedesktop.portal.InputCapture").unwrap()
        );
        assert!(!stored.contains("s3cr3t-token"), "{stored}");
    }
}
