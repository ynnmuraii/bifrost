//! The application id the desktop portal attributes to this process.
//!
//! Portals key permissions and, on KDE, global-shortcut bindings by the *app id* of the
//! caller. A host (non-sandboxed) application cannot put its id into a D-Bus message:
//! `xdg-desktop-portal` derives it from the caller's cgroup, i.e. from the systemd unit that
//! launched the process, and only recognizes the XDG convention
//! `app-<app-id>@<instance>.service` (systemd's `DESKTOP_ENVIRONMENTS`). A process started by
//! a unit called anything else — the owner's `bifrost-gui.service`, a shell, a terminal — is
//! left with an empty app id.
//!
//! `xdg-desktop-portal` 1.19 added `org.freedesktop.host.portal.Registry.Register`, which lets
//! a host application name *its connection* instead. [`register`] calls it once, before any
//! other portal call, and reports which of the two mechanisms gave us the id, so the log says
//! what the compositor will call this application.
//!
//! The portal accepts a registration only for a connection it has not resolved an id for yet,
//! and only once, so this must be the first portal *method* call of the process. Building an
//! `ashpd` proxy (`Proxy::new`) is a version *property* read, which the portal answers from the
//! interface skeleton without resolving the caller, so it does not consume the registration.

use std::collections::HashMap;
use std::sync::OnceLock;

use ashpd::zbus;
use ashpd::zvariant::Value;

/// The id the portal uses for this application.
///
/// It is the basename of the desktop file the package installs (`bifrost.desktop`), which is
/// what `Register` demands: the portal resolves the registered id as `<id>.desktop` and
/// refuses one it cannot resolve.
pub const APP_ID: &str = "bifrost";

/// `org.freedesktop.host.portal.Registry`, present since `xdg-desktop-portal` 1.19.
pub const HOST_REGISTRY_INTERFACE: &str = "org.freedesktop.host.portal.Registry";

/// What to do when neither mechanism names this process.
///
/// XDG's parser wants the instance part of `app-<app-id>@<instance>.service` to be made of
/// letters and digits only (`[[:alnum:]]*`), so a dash there makes it unrecognizable.
pub const LAUNCH_HINT: &str = "launch Bifrost as `systemd-run --user --unit app-bifrost@<letters-or-digits>.service …` (the XDG app-id convention), or from its desktop file, so the portal can derive the app id from the launching unit";

const PORTAL_DESTINATION: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";

/// How the portal learned which application this process is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// `Register` answered: this connection is [`APP_ID`] from now on.
    Registered,
    /// The launching systemd unit follows the XDG convention, so the portal reads the id
    /// from the cgroup.
    LaunchUnit,
    /// Neither: the portal has no app id for this process.
    Unknown,
}

/// The app id the portal attributes to this process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppId {
    /// The id itself; empty when the portal has none.
    pub id: String,
    /// How it was established.
    pub source: Source,
    /// The systemd unit this process runs under, when it runs under one.
    pub unit: Option<String>,
    /// Why `Register` did not name the connection; empty when it did.
    pub reason: String,
}

impl AppId {
    /// Whether the portal knows which application is talking to it.
    #[must_use]
    pub fn known(&self) -> bool {
        !self.id.is_empty()
    }

    /// One sentence for the log: what the portal sees, and where it came from.
    #[must_use]
    pub fn note(&self) -> String {
        let unit = self.unit.as_deref().unwrap_or("no systemd unit");
        match self.source {
            Source::Registered => {
                format!("portal app id {:?} registered for this connection", self.id)
            }
            Source::LaunchUnit => format!(
                "portal app id {:?} comes from the launching unit {unit:?} ({})",
                self.id, self.reason
            ),
            Source::Unknown => format!(
                "the portal sees no app id for this process ({unit}; {}): {LAUNCH_HINT}",
                self.reason
            ),
        }
    }
}

/// The answer of `register` for this process; `None` before the backend connects.
static CURRENT: OnceLock<AppId> = OnceLock::new();
static REGISTER_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The app id the portal uses for this process, as established by [`register`].
#[must_use]
pub fn current() -> Option<&'static AppId> {
    CURRENT.get()
}

/// Names this process with the portal, before any other portal call.
///
/// Idempotent: the first answer is kept, because the portal accepts one registration per
/// connection. See the module documentation for why the call has to be this early.
pub async fn register() -> AppId {
    let _guard = REGISTER_LOCK.lock().await;
    if let Some(app_id) = CURRENT.get() {
        return app_id.clone();
    }
    let unit = current_launch_unit();
    let reply = match portal_connection().await {
        Ok(connection) => call_register(&connection).await,
        Err(reason) => Reply::Refused(reason),
    };
    let app_id = decide(reply, unit);
    if app_id.source == Source::Registered {
        tracing::info!(unit = ?app_id.unit, "{}", app_id.note());
    } else {
        tracing::warn!(unit = ?app_id.unit, hint = LAUNCH_HINT, "{}", app_id.note());
    }
    let _ = CURRENT.set(app_id.clone());
    app_id
}

/// What `org.freedesktop.host.portal.Registry.Register` answered.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Reply {
    /// The portal accepted the id for this connection.
    Registered,
    /// The portal has no host registry: this `xdg-desktop-portal` predates 1.19.
    NoRegistry,
    /// The call was made and failed.
    Refused(String),
}

/// The session connection `ashpd` hands to every proxy of this process.
///
/// `ashpd` keeps one session connection in a process-wide cell, so any proxy gives it back and
/// the registration lands on the connection the portal calls use. `GlobalShortcuts` is the
/// portal this backend exists for; `RemoteDesktop` is the one it cannot work without, and it
/// exists on the compositors whose hotkey portal does not (GNOME 46).
async fn portal_connection() -> Result<zbus::Connection, String> {
    let hotkey = match ashpd::desktop::global_shortcuts::GlobalShortcuts::new().await {
        Ok(portal) => return Ok(portal.connection().clone()),
        Err(error) => error,
    };
    match ashpd::desktop::remote_desktop::RemoteDesktop::new().await {
        Ok(portal) => Ok(portal.connection().clone()),
        Err(error) => Err(format!("{hotkey}; {error}")),
    }
}

/// `Register(in s app_id, in a{sv} options)` on the portal object.
async fn call_register(connection: &zbus::Connection) -> Reply {
    let options: HashMap<&str, Value<'static>> = HashMap::new();
    match connection
        .call_method(
            Some(PORTAL_DESTINATION),
            PORTAL_PATH,
            Some(HOST_REGISTRY_INTERFACE),
            "Register",
            &(APP_ID, options),
        )
        .await
    {
        Ok(_) => Reply::Registered,
        Err(error) => classify(&error),
    }
}

/// `Register` on a portal that predates the interface answers `UnknownInterface`/`UnknownMethod`;
/// every other failure (a sandbox, an id that resolves to no desktop file, a connection that
/// already has an id) is a refusal the log should name.
fn classify(error: &zbus::Error) -> Reply {
    let no_interface = match error {
        zbus::Error::MethodError(name, _, _) => {
            let name = name.as_str();
            name.ends_with("UnknownInterface") || name.ends_with("UnknownMethod")
        }
        zbus::Error::InterfaceNotFound => true,
        _ => false,
    };
    if no_interface {
        Reply::NoRegistry
    } else {
        Reply::Refused(error.to_string())
    }
}

/// The app id this process ends up with, from the portal's answer and the launching unit.
fn decide(reply: Reply, unit: Option<String>) -> AppId {
    let from_unit = unit.as_deref().and_then(app_id_from_unit);
    match reply {
        Reply::Registered => AppId {
            id: APP_ID.to_owned(),
            source: Source::Registered,
            unit,
            reason: String::new(),
        },
        Reply::NoRegistry | Reply::Refused(_) => {
            let reason = match reply {
                Reply::NoRegistry => format!("this portal has no {HOST_REGISTRY_INTERFACE}"),
                Reply::Refused(reason) => format!("the portal refused the registration: {reason}"),
                Reply::Registered => unreachable!("handled above"),
            };
            match from_unit {
                Some(id) => AppId {
                    id,
                    source: Source::LaunchUnit,
                    unit,
                    reason,
                },
                None => AppId {
                    id: String::new(),
                    source: Source::Unknown,
                    unit,
                    reason,
                },
            }
        }
    }
}

/// The systemd unit of this process, as the kernel's cgroup says it.
///
/// The portal reads the same file to derive an app id, so this is what it sees when we cannot
/// register one ourselves.
fn current_launch_unit() -> Option<String> {
    let cgroup = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    unit_from_cgroup(&cgroup)
}

/// The last path element of a `/proc/self/cgroup` dump that names a unit.
///
/// Both cgroup layouts put the process's unit at the end of the path
/// (`0::/user.slice/…/app-bifrost@12ab.service`, `1:name=systemd:/user.slice/…/bifrost-gui.service`).
fn unit_from_cgroup(cgroup: &str) -> Option<String> {
    cgroup
        .lines()
        .filter_map(|line| line.rsplit_once(':').map(|(_, path)| path))
        .flat_map(|path| path.rsplit('/'))
        .find(|element| {
            !element.is_empty() && (element.ends_with(".service") || element.ends_with(".scope"))
        })
        .map(str::to_owned)
}

/// The app id `xdg-desktop-portal` derives from a systemd unit name, if it derives one.
///
/// The recognized shapes are `app-<app-id>[@<instance>].service`,
/// `app-<app-id>-autostart.service` and `app-<app-id>-<instance>.scope` (or `.slice`),
/// optionally prefixed by an alphanumeric launcher name.
#[must_use]
pub fn app_id_from_unit(unit: &str) -> Option<String> {
    let rest = unit.strip_prefix("app-")?;
    if let Some(rest) = rest.strip_suffix(".service") {
        let id = match rest.split_once('@') {
            Some((id, instance)) if instance.chars().all(|c| c.is_ascii_alphanumeric()) => id,
            Some(_) => return None,
            None => rest.strip_suffix("-autostart").unwrap_or(rest),
        };
        return unit_app_id(id);
    }
    if let Some(rest) = rest
        .strip_suffix(".scope")
        .or_else(|| rest.strip_suffix(".slice"))
    {
        let (id, instance) = rest.rsplit_once('-')?;
        if instance.chars().all(|c| c.is_ascii_alphanumeric()) {
            return unit_app_id(id);
        }
    }
    None
}

fn unit_app_id(id: &str) -> Option<String> {
    let id = match id.split_once('-') {
        Some((launcher, rest))
            if !launcher.is_empty() && launcher.chars().all(|c| c.is_ascii_alphanumeric()) =>
        {
            rest
        }
        _ => id,
    };
    (!id.is_empty()).then(|| id.to_owned())
}

#[cfg(test)]
mod tests {
    use super::{
        app_id_from_unit, classify, decide, unit_from_cgroup, AppId, Reply, Source, APP_ID,
    };

    fn method_error(name: &str) -> ashpd::zbus::Error {
        let message =
            ashpd::zbus::Message::method_call("/org/freedesktop/portal/desktop", "Register")
                .expect("a method call builds")
                .build(&())
                .expect("an empty body builds");
        ashpd::zbus::Error::MethodError(
            ashpd::zbus::names::OwnedErrorName::try_from(name).expect("a valid error name"),
            Some("detail".to_owned()),
            message,
        )
    }

    #[test]
    fn the_launching_unit_is_read_from_the_cgroup_path() {
        assert_eq!(
            unit_from_cgroup(
                "0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-bifrost@12ab.service\n"
            )
            .as_deref(),
            Some("app-bifrost@12ab.service")
        );
        assert_eq!(
            unit_from_cgroup("1:name=systemd:/user.slice/user-1000.slice/bifrost-gui.service\n")
                .as_deref(),
            Some("bifrost-gui.service")
        );
        assert_eq!(unit_from_cgroup("0::/\n"), None);
        assert_eq!(unit_from_cgroup(""), None);
    }

    #[test]
    fn only_the_xdg_unit_shapes_yield_an_app_id() {
        assert_eq!(
            app_id_from_unit("app-bifrost@12ab.service").as_deref(),
            Some("bifrost")
        );
        assert_eq!(
            app_id_from_unit("app-bifrost.service").as_deref(),
            Some("bifrost")
        );
        assert_eq!(
            app_id_from_unit("app-bifrost-autostart.service").as_deref(),
            Some("bifrost")
        );
        assert_eq!(
            app_id_from_unit("app-bifrost-12ab.scope").as_deref(),
            Some("bifrost")
        );
        // A dash in the instance part is what the portal's own parser refuses.
        assert_eq!(app_id_from_unit("app-bifrost@live-1.service"), None);
        assert_eq!(app_id_from_unit("bifrost-gui.service"), None);
        assert_eq!(app_id_from_unit("app-@12ab.service"), None);
        assert_eq!(app_id_from_unit("app-bifrost.slice"), None);
        assert_eq!(app_id_from_unit("app-bifrost.scope"), None);
    }

    #[test]
    fn a_missing_registry_is_not_a_refusal_and_an_unknown_method_is() {
        assert_eq!(
            classify(&method_error("org.freedesktop.DBus.Error.UnknownMethod")),
            Reply::NoRegistry
        );
        assert_eq!(
            classify(&method_error("org.freedesktop.DBus.Error.UnknownInterface")),
            Reply::NoRegistry
        );
        let refused = classify(&method_error("org.freedesktop.portal.Error.Failed"));
        assert!(matches!(refused, Reply::Refused(reason) if reason.contains("Failed")));
    }

    #[test]
    fn the_answer_and_the_unit_decide_what_the_portal_calls_us() {
        let registered = decide(Reply::Registered, Some("bifrost-gui.service".to_owned()));
        assert_eq!(registered.id, APP_ID);
        assert_eq!(registered.source, Source::Registered);
        assert!(registered.known());
        assert!(registered.note().contains(APP_ID));

        let from_unit = decide(
            Reply::NoRegistry,
            Some("app-bifrost@12ab.service".to_owned()),
        );
        assert_eq!(from_unit.id, "bifrost");
        assert_eq!(from_unit.source, Source::LaunchUnit);
        assert!(from_unit.note().contains("app-bifrost@12ab.service"));

        let unknown = decide(
            Reply::Refused("denied".to_owned()),
            Some("bifrost-gui.service".to_owned()),
        );
        assert!(!unknown.known());
        assert_eq!(unknown.source, Source::Unknown);
        assert!(unknown.reason.contains("denied"));
        assert!(unknown.note().contains("bifrost-gui.service"));

        let no_unit = decide(Reply::NoRegistry, None);
        assert!(matches!(
            no_unit,
            AppId {
                source: Source::Unknown,
                unit: None,
                ..
            }
        ));
    }
}
