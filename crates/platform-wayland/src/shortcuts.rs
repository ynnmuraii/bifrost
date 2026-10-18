//! GlobalShortcuts portal bindings. Preferred triggers may open a compositor
//! approval dialog; the compositor's returned trigger descriptions are authoritative.

use std::pin::Pin;

use ashpd::desktop::global_shortcuts::{
    Activated, BindShortcutsOptions, GlobalShortcuts, NewShortcut,
};
use ashpd::desktop::{CreateSessionOptions, Session};
use futures_util::stream::StreamExt;
use futures_util::Stream;

use crate::{appid, WaylandError, GLOBAL_SHORTCUTS_INTERFACE};

/// Shortcut id the backend binds; a compositor echoes it back on activation.
pub const EMERGENCY_SHORTCUT_ID: &str = "emergency";

/// The trigger KVM-018 specifies, in the `shortcuts` XDG spec notation.
pub const EMERGENCY_TRIGGER: &str = "CTRL+ALT+SHIFT+F12";

/// Human-readable description bound together with the shortcut, shown in the
/// compositor's shortcut editor.
const EMERGENCY_DESCRIPTION: &str =
    "Bifrost: release remote input and return the keyboard to this machine";

/// The desktop portal's well-known bus name: sessions live at an object path it chooses.
const PORTAL_DESTINATION: &str = "org.freedesktop.portal.Desktop";

/// The session object's interface, per the XDG desktop portal specification.
const SESSION_INTERFACE: &str = "org.freedesktop.portal.Session";

type ActivationStream = Pin<Box<dyn Stream<Item = Activated> + Send + Sync>>;

/// A registered emergency hotkey.
pub struct EmergencyShortcuts {
    session: Session<GlobalShortcuts>,
    session_path: String,
    activated: ActivationStream,
    closed: Pin<Box<dyn Stream<Item = ()> + Send + Sync>>,
    trigger_description: Option<String>,
    trigger_descriptions: Vec<(String, String)>,
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
        Self::register_bindings(&[(EMERGENCY_SHORTCUT_ID, EMERGENCY_DESCRIPTION, trigger)]).await
    }

    /// Registers a complete candidate session without touching the active session.
    /// Every requested action must be granted with a usable trigger before commit.
    pub async fn register_bindings(bindings: &[(&str, &str, &str)]) -> Result<Self, WaylandError> {
        let shortcuts = GlobalShortcuts::new().await.map_err(classify)?;
        let portal_version = shortcuts.version();
        // What the compositor keys the binding by: KDE names its `KGlobalAccel` component
        // after it, so an empty id means the binding is stored under a name that changes
        // with every run (`appid` registers the real one before the first portal call).
        let app_id = appid::current().map_or_else(String::new, |app_id| app_id.id.clone());
        let session = shortcuts
            .create_session(CreateSessionOptions::default())
            .await
            .map_err(classify)?;
        // Session's object path is exposed through its Serialize implementation.
        let session_path = match toml::Value::try_from(&session) {
            Ok(toml::Value::String(path)) => path,
            _ => {
                let _ = session.close().await;
                return Err(WaylandError::Io(std::io::Error::other(
                    "GlobalShortcuts session did not serialize as an object path",
                )));
            }
        };
        // Subscribe before binding so an activation cannot fall between the two calls.
        let activated = shortcuts.receive_activated().await.map_err(classify)?;
        let closed = closed_stream(&session_path).await?;
        let requested: Vec<_> = bindings
            .iter()
            .map(|(id, description, trigger)| {
                NewShortcut::new(*id, *description).preferred_trigger(Some(*trigger))
            })
            .collect();
        let bound = async {
            shortcuts
                .bind_shortcuts(&session, &requested, None, BindShortcutsOptions::default())
                .await?
                .response()
        }
        .await;
        let bound = match bound {
            Ok(bound) => bound,
            Err(error) => {
                let _ = session.close().await;
                let refused = classify(error);
                let reason = refused.to_string();
                tracing::warn!(
                    portal_version,
                    app_id = app_id.as_str(),
                    reason = reason.as_str(),
                    "the GlobalShortcuts portal refused the bind request"
                );
                return Err(refused);
            }
        };
        let returned: Vec<_> = bound
            .shortcuts()
            .iter()
            .map(|shortcut| (shortcut.id(), shortcut.trigger_description()))
            .collect();
        if !all_bindings_granted(bindings, &returned) {
            let _ = session.close().await;
            let missing = missing_bindings(bindings, &returned);
            let answered: Vec<String> = returned
                .iter()
                .map(|(id, trigger)| format!("{id}={trigger}"))
                .collect();
            let answered = answered.join(", ");
            tracing::warn!(
                portal_version,
                app_id = app_id.as_str(),
                missing = missing.as_str(),
                answered = answered.as_str(),
                "the GlobalShortcuts portal answered without a usable trigger for some shortcuts"
            );
            return Err(WaylandError::HotkeyNotGranted { missing });
        }
        let trigger_description = returned
            .iter()
            .find(|(id, _)| *id == EMERGENCY_SHORTCUT_ID)
            .map(|(_, trigger)| (*trigger).to_owned());
        let trigger_descriptions = returned
            .iter()
            .map(|(id, trigger)| ((*id).to_owned(), (*trigger).to_owned()))
            .collect();
        // Success was the silent path: only failures logged. The compositor may
        // remap a preferred trigger, so the granted descriptions are the truth.
        let granted: Vec<_> = returned
            .iter()
            .map(|(id, trigger)| format!("{id}={trigger}"))
            .collect();
        tracing::info!(
            portal_version,
            app_id = app_id.as_str(),
            bindings = %granted.join(", "),
            "GlobalShortcuts bindings granted by the portal"
        );
        Ok(Self {
            session,
            session_path,
            activated: Box::pin(activated),
            closed: Box::pin(closed.map(|_| ())),
            trigger_description,
            trigger_descriptions,
            portal_version,
        })
    }

    /// The trigger the compositor actually bound and will show to the user.
    #[must_use]
    pub fn bound_trigger_description(&self) -> Option<&str> {
        self.trigger_description.as_deref()
    }

    /// User-visible actual trigger returned by the compositor for an action.
    #[must_use]
    pub fn trigger_description(&self, id: &str) -> Option<&str> {
        self.trigger_descriptions
            .iter()
            .find(|(bound_id, _)| bound_id == id)
            .map(|(_, trigger)| trigger.as_str())
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
        loop {
            let activated = tokio::select! {
                biased;
                _ = self.closed.next() => return None,
                activated = self.activated.next() => activated?,
            };
            if activated.session_handle().as_str() == self.session_path {
                return Some(activated.shortcut_id().to_owned());
            }
        }
    }

    /// Closes the shortcut session.
    pub async fn close(&self) -> Result<(), WaylandError> {
        self.session.close().await.map_err(classify)
    }
}

/// Subscribe to the session's `Closed` signal through a proxy of our own.
///
/// ashpd 0.13's `Session::receive_closed()` returns an `impl Stream` that captures the borrow of
/// the session it was called on (no precise-capturing bound on the `impl Trait`), so the stream
/// cannot be stored in a value that also moves that session in. `zbus::Proxy::receive_signal` has
/// no such capture - the stream owns a clone of its proxy - and ashpd re-exports zbus, so this
/// needs no dependency of its own.
async fn closed_stream(
    session_path: &str,
) -> Result<Pin<Box<dyn Stream<Item = ()> + Send + Sync>>, WaylandError> {
    let zbus_error = |error| WaylandError::Portal(ashpd::Error::Zbus(error));
    let connection = ashpd::zbus::Connection::session()
        .await
        .map_err(zbus_error)?;
    let proxy = ashpd::zbus::Proxy::new(
        &connection,
        PORTAL_DESTINATION,
        session_path,
        SESSION_INTERFACE,
    )
    .await
    .map_err(zbus_error)?;
    let closed = proxy.receive_signal("Closed").await.map_err(zbus_error)?;
    Ok(Box::pin(closed.map(|_| ())))
}

/// Whether the portal answered `id` with a trigger the user can actually press.
///
/// A compositor reports a shortcut it did not bind as an entry with an empty
/// `trigger_description` (KDE does this for every row of a dismissed dialog), so an id alone
/// is not a grant.
fn granted(id: &str, returned: &[(&str, &str)]) -> bool {
    returned
        .iter()
        .any(|(bound_id, trigger)| id == *bound_id && !trigger.trim().is_empty())
}

fn all_bindings_granted(requested: &[(&str, &str, &str)], returned: &[(&str, &str)]) -> bool {
    requested.iter().all(|(id, _, _)| granted(id, returned))
}

/// The requested actions without a usable trigger, as `id (chord)` pairs, for the log and for
/// the error the backend reports (`WaylandError::HotkeyNotGranted`).
fn missing_bindings(requested: &[(&str, &str, &str)], returned: &[(&str, &str)]) -> String {
    requested
        .iter()
        .filter(|(id, _, _)| !granted(id, returned))
        .map(|(id, _, chord)| format!("{id} ({chord})"))
        .collect::<Vec<_>>()
        .join(", ")
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

#[cfg(test)]
mod tests {
    use super::{all_bindings_granted, missing_bindings};
    #[test]
    fn candidate_requires_every_action_and_nonempty_triggers() {
        let requested = [
            ("emergency", "release", "CTRL+F12"),
            ("next", "next", "CTRL+F11"),
        ];
        assert!(all_bindings_granted(
            &requested,
            &[("emergency", "Ctrl+F12"), ("next", "Ctrl+F11")]
        ));
        assert!(!all_bindings_granted(
            &requested,
            &[("emergency", "Ctrl+F12")]
        ));
        assert!(!all_bindings_granted(
            &requested,
            &[("emergency", "Ctrl+F12"), ("next", " ")]
        ));
    }

    #[test]
    fn the_refused_actions_are_named_with_the_chord_that_was_asked_for() {
        let requested = [("emergency", "release", "CTRL+ALT+SHIFT+F12")];
        // KDE answers `Success` with an empty `trigger_description` for a row its dialog
        // did not bind; that is not a grant.
        assert_eq!(
            missing_bindings(&requested, &[("emergency", "")]),
            "emergency (CTRL+ALT+SHIFT+F12)"
        );
        assert_eq!(
            missing_bindings(&requested, &[]),
            "emergency (CTRL+ALT+SHIFT+F12)"
        );
        assert_eq!(
            missing_bindings(&requested, &[("emergency", "Ctrl+Alt+Shift+F12")]),
            ""
        );
    }
}
