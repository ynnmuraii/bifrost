// The crate is unsafe-free except for the platform-enumeration half of
// `first_run`, which must call Win32 (`GetAdaptersAddresses`) and `getifaddrs` to
// learn this host's interfaces. As in `identity`, the `#[allow(unsafe_code)]` is
// granted on exactly those modules and every unsafe block carries its own SAFETY
// comment; a `forbid` cannot be relaxed by an inner attribute, hence `deny` here.
#![deny(unsafe_code)]
//! Typed loader, validator and writer for `config.toml` (`config_version = 2`).
//!
//! # What is here
//!
//! * [`schema::Config`] — the whole document as typed structs. Unknown keys are
//!   rejected at every depth, every field has the documented default, and
//!   `config_version` is checked before anything else so a v1 file is reported as
//!   an unsupported version instead of as an unknown v1 key.
//! * [`Config::validate`] — every rule of the «Валидация при загрузке» table of
//!   `docs/03-architecture.md`, plus the timeout bounds of «Что параметризуется,
//!   а что нет». Each failure lists the key, the value and what was allowed, and
//!   carries its `CONFIG_*` code ([`ErrorCode`]).
//! * [`save_atomic`] — temp file, `fsync`, rename, `.bak`, as required by
//!   «Правила записи».
//! * [`first_run`] — the `config.toml` the first `lws-core run` writes when there
//!   is none, and the LAN-address chooser `lws-gui` drives from its settings
//!   screen.
//! * [`paths::Paths`] / [`default_paths`] — the per-OS locations of «Файлы и
//!   пути».
//! * [`hotkey`] — the `Ctrl+Alt+Shift+F12` parser the hotkey rules need.
//! * [`topology`] — `[topology]` links converted into [`geometry::TransitionLink`]
//!   with `"self"` / `"peer:<hex>"` node references resolved against a local
//!   node id.
//!
//! The loadable copy of the example is
//! `crates/config/examples-data/config.example.toml`, byte-identical to
//! `docs/config.example.toml` (which is git-ignored); keep the two in sync.
//!
//! # Deliberate boundaries
//!
//! * A bad *enum value* (`platform.backend = "foo"`) is a parse error
//!   ([`ConfigError::Parse`]) whose message lists the accepted values; the
//!   `CONFIG_BACKEND` code of the table belongs to the *session* check ("a
//!   manually chosen backend must match the session"), which only the platform
//!   layer can make.
//! * `CONFIG_TAKEOVER` (`input.takeover_detection = "evdev"` needs membership in
//!   the `input` group) is likewise an environment check, not a document check.
//! * Peer `node_id` text is not policed by [`Config::validate`] — the validation
//!   table states no format rule — but [`schema::PeerConfig::node_id_bytes`] and
//!   [`parse_node_ref`] reject anything that is not 64 hexadecimal digits.
//! * `[[topology.displays]]` is a GUI snapshot, so it is not converted into
//!   [`geometry::Display`] values; see [`topology`].
//! * Directory permissions (0700 / owner-only ACL) belong to the platform setup
//!   layer; this crate makes no platform calls — with the one exception of
//!   `first_run`, which enumerates interfaces (Win32 `GetAdaptersAddresses` /
//!   Unix `getifaddrs`) to choose a `listen_address`, and creates the
//!   configuration directory itself, `0700` on Unix.

pub mod error;
pub mod first_run;
pub mod hotkey;
pub mod paths;
pub mod save;
pub mod schema;
pub mod topology;

pub use error::{ConfigError, ErrorCode};
pub use first_run::{choose, ensure_config, lan_addresses, set_listen_address, Candidate};
pub use hotkey::{Hotkey, HotkeyError, Modifier};
pub use paths::{default_paths, Paths};
pub use save::save_atomic;
pub use schema::{
    Backend, ClipboardConfig, ClipboardTransfer, Config, DisplayConfig, Edge, EndpointConfig,
    InputConfig, Language, LinkConfig, LogConfig, LogLevel, NetworkConfig, NodeConfig, PeerConfig,
    PlatformConfig, RepeatMode, TakeoverDetection, TimingConfig, TopologyConfig, UiConfig,
    WorkspaceConfig,
};
pub use topology::{node_id_from_hex, parse_node_ref, peer_reference};

/// The only accepted `config_version`.
///
/// `docs/03-architecture.md`: «Текущая схема — `config_version = 2` (см. раздел
/// «Доменная модель»); загрузка v1 запускает миграцию.» — the v1→v2 migration is
/// not implemented here, so a v1 file is rejected with a message saying so.
pub const SUPPORTED_CONFIG_VERSION: u32 = 2;

/// Hard membership cap, re-exported from the `protocol` crate so the number has
/// one definition: `N_max = 4`.
pub use protocol::N_MAX;

/// Serde default for the boolean fields the example enables.
pub(crate) fn truthy() -> bool {
    true
}
