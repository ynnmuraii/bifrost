//! The Wayland clipboard: what the portal can honestly promise, and what it cannot.
//!
//! ADR-008 puts the clipboard on `org.freedesktop.portal.Clipboard`, attached to the session
//! `InputCapture` or `RemoteDesktop` already opened, and names the constraints that make it
//! best-effort:
//!
//! * the portal gives access **only while the session is `Active`** — a session that is
//!   disabled or not yet activated has no clipboard;
//! * Mutter implements neither `ext-data-control-v1` nor `wlr-data-control`, so on GNOME the
//!   portal is the *only* route, and a compositor without the interface has no clipboard at
//!   all;
//! * `xdg-desktop-portal-gnome` 50 changed the portal's behaviour, which is why the version
//!   matters and why the capability is recorded rather than assumed.
//!
//! This module is the pure part: the decision, its reasons, and the vocabulary the GUI uses
//! for a degradation. Driving the portal's clipboard stream is deliberately **not** wired
//! yet — the capture side already reads the portal's `Clipboard` version and the session's
//! `clipboard_enabled` flag (`crate::capture`), and what ADR-008 asks for first is that the
//! absence be explicit instead of a silent no-op.

/// Why the clipboard is not available.
///
/// Each variant is a distinct, checkable condition, because the GUI has to tell the user
/// which one it is (`AC-13a`'s style: a declared limitation, not a failure).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unavailable {
    /// The compositor's portal has no `Clipboard` interface, or one older than the minimum.
    NoPortal,
    /// The portal itself reported the clipboard as off for this session.
    PortalReportsDisabled,
    /// The capture session is not `Active`: the portal grants clipboard access only then.
    SessionNotActive,
}

impl Unavailable {
    /// The sentence the session status and the GUI show.
    pub const fn reason(self) -> &'static str {
        match self {
            Unavailable::NoPortal => {
                "this compositor has no Clipboard portal, and it is the only route on Wayland"
            }
            Unavailable::PortalReportsDisabled => {
                "the desktop portal reports the clipboard as disabled for this session"
            }
            Unavailable::SessionNotActive => {
                "the portal grants clipboard access only while the capture session is active"
            }
        }
    }

    /// The short word for logs.
    pub const fn as_str(self) -> &'static str {
        match self {
            Unavailable::NoPortal => "no-portal",
            Unavailable::PortalReportsDisabled => "portal-disabled",
            Unavailable::SessionNotActive => "session-not-active",
        }
    }
}

/// Whether this session can carry clipboard text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    /// All three conditions hold: text may be offered and applied.
    Available,
    /// One of them does not, and the reason is user-visible.
    Unavailable(Unavailable),
}

impl Availability {
    /// Whether the clipboard may be used at all.
    pub const fn is_available(self) -> bool {
        matches!(self, Availability::Available)
    }
}

/// The decision, from the three things the portal layer actually knows.
///
/// The order is the order of discovery: without the interface there is nothing to ask;
/// a portal that answers "disabled" is the next most specific answer; a session that is not
/// active is the reason that changes minute to minute.
pub const fn availability(
    portal_interface: bool,
    portal_enabled: bool,
    session_active: bool,
) -> Availability {
    if !portal_interface {
        return Availability::Unavailable(Unavailable::NoPortal);
    }
    if !portal_enabled {
        return Availability::Unavailable(Unavailable::PortalReportsDisabled);
    }
    if !session_active {
        return Availability::Unavailable(Unavailable::SessionNotActive);
    }
    Availability::Available
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_three_conditions_are_needed() {
        assert!(availability(true, true, true).is_available());
        assert!(!availability(false, true, true).is_available());
        assert!(!availability(true, false, true).is_available());
        assert!(!availability(true, true, false).is_available());
        assert!(!availability(false, false, false).is_available());
    }

    #[test]
    fn the_reason_is_the_most_specific_one() {
        assert_eq!(
            availability(false, false, false),
            Availability::Unavailable(Unavailable::NoPortal)
        );
        assert_eq!(
            availability(true, false, false),
            Availability::Unavailable(Unavailable::PortalReportsDisabled)
        );
        assert_eq!(
            availability(true, true, false),
            Availability::Unavailable(Unavailable::SessionNotActive)
        );
    }

    #[test]
    fn every_unavailability_has_a_sentence_and_a_word() {
        for reason in [
            Unavailable::NoPortal,
            Unavailable::PortalReportsDisabled,
            Unavailable::SessionNotActive,
        ] {
            assert!(!reason.reason().is_empty());
            assert!(!reason.as_str().is_empty());
            assert_ne!(reason.as_str(), "available");
        }
        // The three sentences are distinct, so the GUI never shows two causes as one.
        let sentences: Vec<&str> = [
            Unavailable::NoPortal,
            Unavailable::PortalReportsDisabled,
            Unavailable::SessionNotActive,
        ]
        .iter()
        .map(|reason| reason.reason())
        .collect();
        let mut sorted = sentences.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), sentences.len());
    }
}
