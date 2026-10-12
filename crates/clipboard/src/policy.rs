//! What may leave the machine at all.
//!
//! ADR-008: the clipboard is opt-in on **both** sides, off by default, and content a
//! password manager marked as a secret is not sent while `skip_secret_marked = true`
//! (`AC-15`). The markers themselves are platform vocabulary — Win32's clipboard formats,
//! X11's `x-kde-passwordManagerHint`, the portal's flag — so the adapters translate into
//! [`Marker`] and this module makes the decision.

use crate::{ClipboardError, MAX_BYTES};

/// `[clipboard]` from `config.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// Opt-in on both sides.
    pub enabled: bool,
    /// Skip content marked as a secret by password managers.
    pub skip_secret_marked: bool,
    /// Maximum transfer size; `docs/config.example.toml` allows at most the protocol limit.
    pub max_bytes: u64,
}

impl Default for Policy {
    fn default() -> Self {
        // The configuration's own defaults: off, secrets skipped, 1 MiB.
        Policy {
            enabled: false,
            skip_secret_marked: true,
            max_bytes: MAX_BYTES,
        }
    }
}

/// A platform's way of saying "this is a secret".
///
/// One variant per *documented* marker, so a test can name the case it covers instead of
/// pretending all platforms agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Marker {
    /// Win32 `ExcludeClipboardContentFromMonitorProcessing`: «not for any monitoring».
    ExcludeFromMonitor,
    /// Win32 `CanIncludeInClipboardHistory = 0`: the password manager's own signal.
    NoHistory,
    /// Win32 `Clipboard Viewer Ignore`: the oldest of the three.
    ViewerIgnore,
    /// X11 `x-kde-passwordManagerHint` with the value `secret` (KeePassXC, KWallet).
    KdeSecret,
    /// The Wayland portal's own secret flag, when the compositor sets one.
    PortalSecret,
}

/// The content the platform reports, before anything is decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Content {
    /// The payload size in bytes (UTF-8 text).
    pub bytes: u64,
    /// Every secret marker the platform reported for it.
    pub markers: Vec<Marker>,
}

impl Content {
    /// Plain text of `bytes`, with no marker.
    pub fn text(bytes: u64) -> Self {
        Content {
            bytes,
            markers: Vec::new(),
        }
    }

    /// The same, carrying one marker.
    pub fn marked(bytes: u64, marker: Marker) -> Self {
        Content {
            bytes,
            markers: vec![marker],
        }
    }
}

/// Whether the content may be sent, or why not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Offer it.
    Send,
    /// Do not offer it; the reason is a wire code.
    Refuse(ClipboardError),
}

impl Policy {
    /// The decision for one clipboard item.
    ///
    /// Order matters: disabled beats everything (ADR-008: «выключается отдельно от KVM»),
    /// then the size, then the secret markers — so a disabled node never even inspects the
    /// content, and a secret is reported as a secret rather than as a size problem.
    pub fn decide(&self, content: &Content) -> Decision {
        if !self.enabled {
            return Decision::Refuse(ClipboardError::Disabled);
        }
        if content.bytes > self.max_bytes || content.bytes > MAX_BYTES {
            return Decision::Refuse(ClipboardError::TooLarge);
        }
        if self.skip_secret_marked && !content.markers.is_empty() {
            return Decision::Refuse(ClipboardError::Secret);
        }
        Decision::Send
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled() -> Policy {
        Policy {
            enabled: true,
            skip_secret_marked: true,
            max_bytes: MAX_BYTES,
        }
    }

    #[test]
    fn the_default_is_off_and_secret_skipping() {
        assert!(
            !Policy::default().enabled,
            "the clipboard is off by default"
        );
        assert!(Policy::default().skip_secret_marked);
        assert_eq!(
            Policy::default().decide(&Content::text(10)),
            Decision::Refuse(ClipboardError::Disabled)
        );
    }

    #[test]
    fn the_size_boundary_is_one_mebibyte() {
        // `AC-09`: the payload limit is exactly 1 MiB; one byte more is refused.
        assert_eq!(enabled().decide(&Content::text(MAX_BYTES)), Decision::Send);
        assert_eq!(
            enabled().decide(&Content::text(MAX_BYTES + 1)),
            Decision::Refuse(ClipboardError::TooLarge)
        );
        // A configured maximum below the protocol limit is honoured too.
        let smaller = Policy {
            max_bytes: 1024,
            ..enabled()
        };
        assert_eq!(smaller.decide(&Content::text(1024)), Decision::Send);
        assert_eq!(
            smaller.decide(&Content::text(1025)),
            Decision::Refuse(ClipboardError::TooLarge)
        );
        // And a configuration that tries to exceed the protocol limit cannot: the limit is
        // the protocol's, not the file's.
        let wider = Policy {
            max_bytes: MAX_BYTES * 4,
            ..enabled()
        };
        assert_eq!(
            wider.decide(&Content::text(MAX_BYTES + 1)),
            Decision::Refuse(ClipboardError::TooLarge)
        );
    }

    #[test]
    fn every_documented_secret_marker_is_refused() {
        // `AC-15` names one case; each platform marker is its own case here.
        for marker in [
            Marker::ExcludeFromMonitor,
            Marker::NoHistory,
            Marker::ViewerIgnore,
            Marker::KdeSecret,
            Marker::PortalSecret,
        ] {
            assert_eq!(
                enabled().decide(&Content::marked(11, marker)),
                Decision::Refuse(ClipboardError::Secret),
                "{marker:?}"
            );
        }
        // With the filter off the same content is sent: the setting is what decides.
        let permissive = Policy {
            skip_secret_marked: false,
            ..enabled()
        };
        assert_eq!(
            permissive.decide(&Content::marked(11, Marker::KdeSecret)),
            Decision::Send
        );
    }

    #[test]
    fn a_disabled_node_never_looks_at_the_content() {
        // The refusal is `DISABLED`, not `TOO_LARGE` or `SECRET`, whatever the content is.
        let off = Policy {
            enabled: false,
            ..enabled()
        };
        assert_eq!(
            off.decide(&Content::marked(MAX_BYTES * 2, Marker::KdeSecret)),
            Decision::Refuse(ClipboardError::Disabled)
        );
    }
}
