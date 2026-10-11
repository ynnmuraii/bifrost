//! Emergency hotkey: `org.freedesktop.portal.GlobalShortcuts` (KVM-018).
//!
//! The agent registers `Ctrl+Alt+Shift+F12` for its whole lifetime and handles it
//! itself, so a stuck capture session can always be released without a second machine.
//!
//! GNOME 46 has no `GlobalShortcuts` portal at all (it arrived in
//! `xdg-desktop-portal-gnome` 48), and this backend must not pretend otherwise:
//! [`EmergencyShortcuts::register`] then fails with
//! [`WaylandError::HotkeyUnavailable`], which names the missing portal interface. The
//! caller is expected to fall back to a documented alternative instead of losing the
//! emergency path silently.

use std::pin::Pin;

use ashpd::desktop::global_shortcuts::{
    Activated, BindShortcutsOptions, GlobalShortcuts, NewShortcut,
};
use ashpd::desktop::{CreateSessionOptions, Session};
use futures_util::stream::StreamExt;
use futures_util::Stream;

use crate::{WaylandError, GLOBAL_SHORTCUTS_INTERFACE};

/// Shortcut id the backend binds; a compositor echoes it back on activation.
pub const EMERGENCY_SHORTCUT_ID: &str = "emergency";

/// The trigger KVM-018 specifies, in the `shortcuts` XDG spec notation.
pub const EMERGENCY_TRIGGER: &str = "CTRL+ALT+SHIFT+F12";

/// Human-readable description bound together with the shortcut, shown in the
/// compositor's shortcut editor.
const EMERGENCY_DESCRIPTION: &str =
    "LAN Workspace: release remote input and return the keyboard to this machine";

type ActivationStream = Pin<Box<dyn Stream<Item = Activated> + Send>>;

/// A registered emergency hotkey.
pub struct EmergencyShortcuts {
    session: Session<GlobalShortcuts>,
    activated: ActivationStream,
    trigger_description: Option<String>,
    portal_version: u32,
}

impl EmergencyShortcuts {
    /// The trigger KVM-018 specifies.
    pub const DEFAULT_TRIGGER: &'static str = EMERGENCY_TRIGGER;

    /// Binds the emergency shortcut to the compositor for the lifetime of this value.
    ///
    /// # Errors
    ///
    /// * [`WaylandError::HotkeyUnavailable`] when the compositor has no
    ///   `GlobalShortcuts` portal (GNOME 46), or when the portal answers
    ///   `UnknownInterface`/`UnknownMethod`/`ServiceUnknown`.
    /// * [`WaylandError::Portal`] for any other D-Bus failure, which is a real error and
    ///   not "unsupported".
    pub async fn register(trigger: &str) -> Result<Self, WaylandError> {
        let shortcuts = GlobalShortcuts::new().await.map_err(classify)?;
        let portal_version = shortcuts.version();
        let session = shortcuts
            .create_session(CreateSessionOptions::default())
            .await
            .map_err(classify)?;
        // Subscribe before binding so an activation cannot fall between the two calls.
        let activated = shortcuts.receive_activated().await.map_err(classify)?;
        let shortcut = NewShortcut::new(EMERGENCY_SHORTCUT_ID, EMERGENCY_DESCRIPTION)
            .preferred_trigger(Some(trigger));
        let bound = shortcuts
            .bind_shortcuts(&session, &[shortcut], None, BindShortcutsOptions::default())
            .await
            .map_err(classify)?
            .response()
            .map_err(classify)?;
        let Some(shortcut) = bound
            .shortcuts()
            .iter()
            .find(|shortcut| shortcut.id() == EMERGENCY_SHORTCUT_ID)
        else {
            // The portal answered but did not bind our shortcut: without it there is no
            // emergency path, so this cannot be reported as success.
            return Err(WaylandError::HotkeyUnavailable {
                portal_interface: GLOBAL_SHORTCUTS_INTERFACE,
            });
        };
        Ok(Self {
            session,
            activated: Box::pin(activated),
            trigger_description: Some(shortcut.trigger_description().to_owned()),
            portal_version,
        })
    }

    /// The trigger the compositor actually bound and will show to the user.
    #[must_use]
    pub fn bound_trigger_description(&self) -> Option<&str> {
        self.trigger_description.as_deref()
    }

    /// The version of the `GlobalShortcuts` interface.
    #[must_use]
    pub fn portal_version(&self) -> u32 {
        self.portal_version
    }

    /// Waits for the next hotkey activation and returns the shortcut id.
    ///
    /// `None` means the portal closed the session or the D-Bus connection went away.
    pub async fn next_activation(&mut self) -> Option<String> {
        let activated = self.activated.next().await?;
        Some(activated.shortcut_id().to_owned())
    }

    /// Closes the shortcut session.
    pub async fn close(&self) -> Result<(), WaylandError> {
        self.session.close().await.map_err(classify)
    }
}

/// Classifies a portal failure as "no emergency hotkey here" or as a real error.
fn classify(error: ashpd::Error) -> WaylandError {
    let unavailable = match &error {
        ashpd::Error::PortalNotFound(_) | ashpd::Error::RequiresVersion(..) => true,
        ashpd::Error::Zbus(ashpd::zbus::Error::MethodError(name, _, _)) => {
            let name = name.as_str();
            name.contains("UnknownInterface")
                || name.contains("UnknownMethod")
                || name.contains("ServiceUnknown")
        }
        _ => false,
    };
    if unavailable {
        WaylandError::HotkeyUnavailable {
            portal_interface: GLOBAL_SHORTCUTS_INTERFACE,
        }
    } else {
        WaylandError::Portal(error)
    }
}
