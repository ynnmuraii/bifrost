//! The `config_version = 2` schema, its documented defaults, parsing and
//! validation.
//!
//! Every key, default and range comes from `docs/config.example.toml` and the
//! «Валидация при загрузке» / «Что параметризуется» tables of
//! `docs/03-architecture.md`; the loadable copy of the example lives in
//! `crates/config/examples-data/config.example.toml`.

use core::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::{ConfigError, ErrorCode};
use crate::hotkey::{Hotkey, HotkeyBindings};
use crate::paths;
use crate::{N_MAX, SUPPORTED_CONFIG_VERSION};

/// Protocols limits reused from the `protocol` crate, so the wire limits have
/// exactly one definition.
const MAX_CLIPBOARD_PAYLOAD_BYTES: u64 = protocol::MAX_CLIPBOARD_PAYLOAD_BYTES;
const DEFAULT_PORT_INPUT: u16 = protocol::PORT_INPUT_CONTROL;
const DEFAULT_PORT_CLIPBOARD: u16 = protocol::PORT_CLIPBOARD;

/// Defines a closed string enum: lowercase literal values, a case-insensitive
/// parser that lists the allowed values on error, and serde support.
macro_rules! closed_enum {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $( $variant:ident => $text:literal ),+ $(,)?
        } default $default:ident
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        $vis enum $name {
            $(
                #[allow(missing_docs)]
                $variant,
            )+
        }

        impl $name {
            /// Every accepted literal value, in the order of the documents.
            pub const ALL: &'static [&'static str] = &[ $($text),+ ];

            /// The literal value as written in the configuration.
            pub const fn as_str(self) -> &'static str {
                match self {
                    $( Self::$variant => $text ),+
                }
            }

            /// Parses a literal value, case-insensitively.
            pub fn parse(text: &str) -> Result<Self, String> {
                let lowered = text.to_ascii_lowercase();
                $(
                    if lowered == $text {
                        return Ok(Self::$variant);
                    }
                )+
                Err(format!(
                    "unknown value `{text}`; expected one of: {}",
                    Self::ALL.join(", ")
                ))
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::$default
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.as_str().to_owned()
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let text = String::deserialize(deserializer)?;
                Self::parse(&text).map_err(serde::de::Error::custom)
            }
        }
    };
}

closed_enum! {
    /// `platform.backend`: how the platform layer picks its backend.
    pub enum Backend {
        Auto => "auto",
        X11 => "x11",
        Wayland => "wayland",
    } default Auto
}

closed_enum! {
    /// `input.repeat_mode`: which side generates keyboard auto-repeat.
    pub enum RepeatMode {
        ReceiverNative => "receiver_native",
        ReceiverSoftware => "receiver_software",
    } default ReceiverSoftware
}

closed_enum! {
    /// `input.takeover_detection`: how physical input on the receiver is seen.
    pub enum TakeoverDetection {
        Auto => "auto",
        Evdev => "evdev",
        Off => "off",
    } default Auto
}

closed_enum! {
    /// `clipboard.transfer`: when an offered clipboard payload is requested.
    pub enum ClipboardTransfer {
        OnOffer => "on_offer",
        OnFocusEnter => "on_focus_enter",
    } default OnOffer
}

closed_enum! {
    /// `ui.language`: the GUI language.
    pub enum Language {
        Ru => "ru",
        En => "en",
    } default Ru
}

closed_enum! {
    /// `log.level`: the minimum level written to the log file.
    pub enum LogLevel {
        Trace => "trace",
        Debug => "debug",
        Info => "info",
        Warn => "warn",
        Error => "error",
    } default Info
}

closed_enum! {
    /// `[topology]` display-edge name (`[[topology.links]] from/to .edge`).
    pub enum Edge {
        Left => "left",
        Right => "right",
        Top => "top",
        Bottom => "bottom",
    } default Left
}

impl Edge {
    /// The matching [`geometry::Edge`].
    pub const fn to_geometry(self) -> geometry::Edge {
        match self {
            Edge::Left => geometry::Edge::Left,
            Edge::Right => geometry::Edge::Right,
            Edge::Top => geometry::Edge::Top,
            Edge::Bottom => geometry::Edge::Bottom,
        }
    }
}

/// The whole `config.toml` (`config_version = 2`).
///
/// Unknown keys are rejected at any depth (`deny_unknown_fields`), and every
/// section and field has the documented default, so a file may carry only the
/// keys it wants to override.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Schema version; must be [`SUPPORTED_CONFIG_VERSION`].
    pub config_version: u32,
    /// `[node]`.
    #[serde(default)]
    pub node: NodeConfig,
    /// `[platform]`.
    #[serde(default)]
    pub platform: PlatformConfig,
    /// `[network]`.
    #[serde(default)]
    pub network: NetworkConfig,
    /// `[workspace]`.
    #[serde(default)]
    pub workspace: WorkspaceConfig,
    /// `[timing]`.
    #[serde(default)]
    pub timing: TimingConfig,
    /// `[input]`.
    #[serde(default)]
    pub input: InputConfig,
    /// `[clipboard]`.
    #[serde(default)]
    pub clipboard: ClipboardConfig,
    /// `[ui]`.
    #[serde(default)]
    pub ui: UiConfig,
    /// `[appearance]`.
    #[serde(default)]
    pub appearance: AppearanceConfig,
    /// `[log]`.
    #[serde(default)]
    pub log: LogConfig,
    /// `[topology]`.
    #[serde(default)]
    pub topology: TopologyConfig,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            config_version: SUPPORTED_CONFIG_VERSION,
            node: NodeConfig::default(),
            platform: PlatformConfig::default(),
            network: NetworkConfig::default(),
            workspace: WorkspaceConfig::default(),
            timing: TimingConfig::default(),
            input: InputConfig::default(),
            clipboard: ClipboardConfig::default(),
            ui: UiConfig::default(),
            appearance: AppearanceConfig::default(),
            log: LogConfig::default(),
            topology: TopologyConfig::default(),
        }
    }
}

impl core::str::FromStr for Config {
    type Err = ConfigError;

    /// Same as [`Config::from_str`]: parse the text, without validating.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Config::from_str(text)
    }
}

impl Config {
    /// Read and parse `path`, then [`Config::validate`] it.
    pub fn load(path: impl AsRef<std::path::Path>) -> Result<Config, ConfigError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let config = Config::from_str(&text)?;
        config.validate()?;
        Ok(config)
    }

    /// Parse TOML text. Does not run [`Config::validate`]: the caller may want
    /// to inspect an out-of-range value before rejecting it.
    ///
    /// The name is the crate's public API for this operation; the equivalent
    /// [`std::str::FromStr`] implementation exists as well and delegates here.
    ///
    /// `config_version` is checked *before* the rest of the document is
    /// deserialized, so a v1 file reports the unsupported version instead of an
    /// unknown key that only exists in v1.
    ///
    /// The three hotkey keys of a pre-`[input.hotkeys]` file
    /// (`input.emergency_hotkey`, `input.switch_hotkey`,
    /// `workspace.claim_hotkey`) are accepted and mapped into `[input.hotkeys]`
    /// (see the private `legacy` module). Everything else keeps
    /// `deny_unknown_fields`. Rewriting such a file is the core's job, not the
    /// loader's: [`crate::migrate_legacy_hotkeys`].
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(text: &str) -> Result<Config, ConfigError> {
        let mut value: toml::Value =
            toml::from_str(text).map_err(|source| ConfigError::Parse { source })?;
        match value.get("config_version") {
            None => return Err(ConfigError::MissingVersion),
            Some(found) => {
                if found.as_integer() != Some(i64::from(SUPPORTED_CONFIG_VERSION)) {
                    return Err(ConfigError::UnsupportedVersion {
                        found: found.to_string(),
                    });
                }
            }
        }
        let overridden = crate::legacy::strip(&mut value)?;
        if !overridden.is_empty() {
            let actions = overridden
                .iter()
                .map(|action| action.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            tracing::warn!(
                actions,
                "legacy hotkey key ignored: [input.hotkeys] already sets the action"
            );
        }
        value
            .try_into()
            .map_err(|source| ConfigError::Parse { source })
    }

    /// Run every documented load-validation rule.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.network.validate()?;
        self.workspace.validate()?;
        self.timing.validate()?;
        self.input.validate()?;
        self.clipboard.validate()?;
        self.log.validate()?;
        self.topology.validate()?;
        Ok(())
    }

    /// Serialize back to TOML text, as written by [`crate::save_atomic`].
    pub fn to_toml(&self) -> Result<String, ConfigError> {
        toml::to_string_pretty(self).map_err(|source| ConfigError::Serialize { source })
    }

    /// Serialize and write atomically (temp file, fsync, rename, `.bak`).
    pub fn save(&self, path: impl AsRef<std::path::Path>) -> Result<(), ConfigError> {
        let text = self.to_toml()?;
        crate::save_atomic(path.as_ref(), &text)
    }

    /// The documented file locations for the current user.
    pub fn default_paths() -> Result<paths::Paths, ConfigError> {
        paths::default_paths()
    }
}

/// `[node]`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NodeConfig {
    /// Human-readable label; never an identity (`docs/protocol.proto`
    /// `Hello.display_name`). The example's value is `"Workstation"`.
    #[serde(default = "default_display_name")]
    pub display_name: String,
    /// This node may become the input owner.
    #[serde(default = "crate::truthy")]
    pub can_own: bool,
    /// Claim ownership at startup with `serial = max_seen + 1`
    /// (the example's comment: Windows `true`, laptops `false`).
    #[serde(default = "crate::truthy")]
    pub claim_on_startup: bool,
}

impl Default for NodeConfig {
    fn default() -> Self {
        NodeConfig {
            display_name: default_display_name(),
            can_own: true,
            claim_on_startup: true,
        }
    }
}

fn default_display_name() -> String {
    "Workstation".to_owned()
}

/// `[platform]`.
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlatformConfig {
    /// Backend selection; a manually selected backend must match the session
    /// (`CONFIG_BACKEND`, checked by the platform layer, not here).
    #[serde(default)]
    pub backend: Backend,
}

/// `[network]`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkConfig {
    /// Concrete LAN address; every node listens on it. `0.0.0.0` requires
    /// `allow_any_interface = true`.
    #[serde(default)]
    pub listen_address: String,
    /// Input/control port. `docs/05-protocol.md`: both nodes listen on 47901.
    #[serde(default = "default_port_input")]
    pub port_input: u16,
    /// Clipboard port. `docs/05-protocol.md`: 47902.
    #[serde(default = "default_port_clipboard")]
    pub port_clipboard: u16,
    /// Reconnect backoff bounds in milliseconds. `docs/05-protocol.md`:
    /// «backoff 0.5..5 s».
    #[serde(default = "default_backoff")]
    pub reconnect_backoff_ms: [u64; 2],
    /// Members allowed in the workspace: `1..4` (`N_max = 4`); 3 is supported
    /// and tested.
    #[serde(default = "default_max_members")]
    pub max_members: u8,
    /// Required to bind an unspecified address (`0.0.0.0` / `::`).
    ///
    /// The validation table names this flag but the example does not list it;
    /// the default matches the table's «только с явным `allow_any_interface = true`».
    #[serde(default)]
    pub allow_any_interface: bool,
    /// Trusted peers; the list is filled by pairing and an address may be edited
    /// by hand.
    #[serde(default)]
    pub peers: Vec<PeerConfig>,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        NetworkConfig {
            listen_address: String::new(),
            port_input: default_port_input(),
            port_clipboard: default_port_clipboard(),
            reconnect_backoff_ms: default_backoff(),
            max_members: default_max_members(),
            allow_any_interface: false,
            peers: Vec::new(),
        }
    }
}

fn default_port_input() -> u16 {
    DEFAULT_PORT_INPUT
}

fn default_port_clipboard() -> u16 {
    DEFAULT_PORT_CLIPBOARD
}

fn default_backoff() -> [u64; 2] {
    [500, 5_000]
}

fn default_max_members() -> u8 {
    3
}

/// One `[[network.peers]]` entry.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PeerConfig {
    /// `SHA-256(SPKI DER)` of the trusted node. Kept as text: this loader does
    /// not police the id format, but [`PeerConfig::node_id_bytes`] does.
    pub node_id: String,
    /// Last known address; empty while it is still unknown.
    #[serde(default)]
    pub address: String,
}

impl PeerConfig {
    /// The peer identity as 32 raw bytes, or a `CONFIG_OWNER`-style error when
    /// the text is not 64 hexadecimal digits.
    pub fn node_id_bytes(&self) -> Result<[u8; 32], ConfigError> {
        crate::topology::node_id_from_hex(&self.node_id).ok_or_else(|| {
            ConfigError::invalid(
                "network.peers.node_id",
                &self.node_id,
                "64 hexadecimal digits (SHA-256 of the SPKI DER)",
            )
        })
    }
}

impl NetworkConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        if self.listen_address.is_empty() {
            return Err(ConfigError::coded(
                ErrorCode::Listen,
                "network.listen_address",
                "<empty>",
                "a concrete private LAN address, e.g. 192.168.1.10",
            ));
        }
        let parsed: std::net::IpAddr = self.listen_address.parse().map_err(|_| {
            ConfigError::coded(
                ErrorCode::Listen,
                "network.listen_address",
                &self.listen_address,
                "an IP address literal, e.g. 192.168.1.10",
            )
        })?;
        if parsed.is_unspecified() && !self.allow_any_interface {
            return Err(ConfigError::coded(
                ErrorCode::Listen,
                "network.listen_address",
                &self.listen_address,
                "a concrete address, or allow_any_interface = true",
            ));
        }
        for (key, port) in [
            ("network.port_input", self.port_input),
            ("network.port_clipboard", self.port_clipboard),
        ] {
            if port == 0 {
                return Err(ConfigError::invalid(key, port, "1..=65535"));
            }
        }
        let [low, high] = self.reconnect_backoff_ms;
        if low < 500 || high > 5_000 || low > high {
            return Err(ConfigError::invalid(
                "network.reconnect_backoff_ms",
                format!("[{low}, {high}]"),
                "[low, high] with 500 <= low <= high <= 5000 (docs/05-protocol.md: backoff 0.5..5 s)",
            ));
        }
        let max = usize::from(N_MAX);
        if self.max_members < 1 || usize::from(self.max_members) > max {
            return Err(ConfigError::coded(
                ErrorCode::Members,
                "network.max_members",
                self.max_members,
                format!("1..={max} (N_max = {max}); 3 is supported and tested"),
            ));
        }
        Ok(())
    }
}

/// `[workspace]`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceConfig {
    /// Persisted last known ownership claim serial (Lamport counter).
    #[serde(default)]
    pub owner_claim_serial: u64,
    /// Persisted last known owner; empty means none.
    #[serde(default)]
    pub owner_node_id: String,
    /// `Ping` period per link while idle.
    #[serde(default = "default_link_ping_ms")]
    pub link_ping_ms: u32,
    /// No `Pong` within this marks a member offline.
    #[serde(default = "default_link_timeout_ms")]
    pub link_timeout_ms: u32,
    /// After this a `OwnerClaim` becomes a self-grant.
    #[serde(default = "default_claim_grant_timeout_ms")]
    pub claim_grant_timeout_ms: u32,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        WorkspaceConfig {
            owner_claim_serial: 0,
            owner_node_id: String::new(),
            link_ping_ms: default_link_ping_ms(),
            link_timeout_ms: default_link_timeout_ms(),
            claim_grant_timeout_ms: default_claim_grant_timeout_ms(),
        }
    }
}

fn default_link_ping_ms() -> u32 {
    1_000
}

fn default_link_timeout_ms() -> u32 {
    3_000
}

fn default_claim_grant_timeout_ms() -> u32 {
    600
}

impl WorkspaceConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        if (self.owner_claim_serial > 0) != !self.owner_node_id.is_empty() {
            return Err(ConfigError::coded(
                ErrorCode::Owner,
                "workspace.owner_claim_serial / workspace.owner_node_id",
                format!(
                    "serial = {}, node_id = {:?}",
                    self.owner_claim_serial, self.owner_node_id
                ),
                "a consistent pair: serial > 0 requires a non-empty node_id and vice versa",
            ));
        }
        if self.link_ping_ms == 0
            || self.link_timeout_ms == 0
            || self.link_timeout_ms <= self.link_ping_ms
        {
            return Err(ConfigError::invalid(
                "workspace.link_ping_ms / workspace.link_timeout_ms",
                format!("{} / {}", self.link_ping_ms, self.link_timeout_ms),
                "link_ping_ms >= 1 and link_timeout_ms > link_ping_ms",
            ));
        }
        if self.claim_grant_timeout_ms == 0 {
            return Err(ConfigError::invalid(
                "workspace.claim_grant_timeout_ms",
                self.claim_grant_timeout_ms,
                ">= 1 (default 600)",
            ));
        }
        Ok(())
    }
}

/// `[timing]` — the timeouts that may be tuned, inside documented bounds.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TimingConfig {
    /// `150..=400`, default 200.
    #[serde(default = "default_prepare_timeout_ms")]
    pub prepare_timeout_ms: u32,
    /// `300..=1000`, default 1000. Every lease deadline derives from this value.
    #[serde(default = "default_lease_ttl_ms")]
    pub lease_ttl_ms: u32,
    /// `200..=500`, default 300. Carried in `AgentAttach`; the IPC heartbeat is
    /// this value divided by 4.
    #[serde(default = "default_agent_core_silence_ms")]
    pub agent_core_silence_ms: u32,
    /// Fixed at 150: the receiver stops auto-repeat after this much silence.
    #[serde(default = "default_repeat_guard_ms")]
    pub repeat_guard_ms: u32,
}

impl Default for TimingConfig {
    fn default() -> Self {
        TimingConfig {
            prepare_timeout_ms: default_prepare_timeout_ms(),
            lease_ttl_ms: default_lease_ttl_ms(),
            agent_core_silence_ms: default_agent_core_silence_ms(),
            repeat_guard_ms: default_repeat_guard_ms(),
        }
    }
}

fn default_prepare_timeout_ms() -> u32 {
    200
}

fn default_lease_ttl_ms() -> u32 {
    1_000
}

fn default_agent_core_silence_ms() -> u32 {
    300
}

fn default_repeat_guard_ms() -> u32 {
    150
}

impl TimingConfig {
    /// The challenge period, `min(lease_ttl_ms / 5, 100)`. Computed, never configured.
    pub const fn challenge_interval_ms(&self) -> u32 {
        let interval_ms = self.lease_ttl_ms / 5;
        if interval_ms < 100 {
            interval_ms
        } else {
            100
        }
    }

    /// The IPC heartbeat period, `agent_core_silence_ms / 4`
    /// («IPC heartbeat = value div 4», ≤ 125 ms).
    pub const fn ipc_heartbeat_ms(&self) -> u32 {
        self.agent_core_silence_ms / 4
    }

    fn validate(&self) -> Result<(), ConfigError> {
        for (key, value, low, high, default) in [
            (
                "timing.prepare_timeout_ms",
                self.prepare_timeout_ms,
                150,
                400,
                200,
            ),
            ("timing.lease_ttl_ms", self.lease_ttl_ms, 300, 1_000, 1_000),
            (
                "timing.agent_core_silence_ms",
                self.agent_core_silence_ms,
                200,
                500,
                300,
            ),
        ] {
            if value < low || value > high {
                return Err(ConfigError::coded(
                    ErrorCode::Timing,
                    key,
                    value,
                    format!("{low}..={high} (default {default})"),
                ));
            }
        }
        if self.repeat_guard_ms != 150 {
            return Err(ConfigError::coded(
                ErrorCode::Timing,
                "timing.repeat_guard_ms",
                self.repeat_guard_ms,
                "150 — this timeout is not tunable",
            ));
        }
        Ok(())
    }
}

/// `[input]`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InputConfig {
    /// Configurable global shortcuts, including the mandatory emergency release.
    #[serde(default)]
    pub hotkeys: HotkeyBindings,
    /// Pointer gain applied to deltas; a positive finite factor.
    #[serde(default = "default_pointer_gain")]
    pub pointer_gain: f64,
    /// Dwell time at an edge before crossing, to avoid accidental transitions.
    #[serde(default)]
    pub edge_dwell_ms: u32,
    /// Retreat from an edge a return came home through, in logical units: the edge is
    /// re-armed that far inward (or after a short pause without pushing into it).
    #[serde(default = "default_edge_hysteresis_px")]
    pub edge_hysteresis_px: u32,
    /// Physical movement on the receiver that ends a lease, in logical units.
    #[serde(default = "default_takeover_threshold_px")]
    pub takeover_threshold_px: u32,
    /// Which side generates auto-repeat.
    #[serde(default)]
    pub repeat_mode: RepeatMode,
    /// A source without an emergency hotkey must not enter Remote when this is
    /// true. `false` is accepted, but the GUI must warn on every start.
    #[serde(default = "crate::truthy")]
    pub require_emergency_hotkey: bool,
    /// How physical input on the receiver is detected.
    #[serde(default)]
    pub takeover_detection: TakeoverDetection,
}

impl Default for InputConfig {
    fn default() -> Self {
        InputConfig {
            hotkeys: HotkeyBindings::default(),
            pointer_gain: default_pointer_gain(),
            edge_dwell_ms: 0,
            edge_hysteresis_px: default_edge_hysteresis_px(),
            takeover_threshold_px: default_takeover_threshold_px(),
            repeat_mode: RepeatMode::default(),
            require_emergency_hotkey: true,
            takeover_detection: TakeoverDetection::default(),
        }
    }
}

fn default_pointer_gain() -> f64 {
    1.0
}

fn default_edge_hysteresis_px() -> u32 {
    8
}

fn default_takeover_threshold_px() -> u32 {
    12
}

impl InputConfig {
    /// The parsed emergency hotkey, if it parses.
    pub fn emergency(&self) -> Option<Hotkey> {
        Hotkey::parse(&self.hotkeys.emergency_release).ok()
    }

    fn validate(&self) -> Result<(), ConfigError> {
        self.hotkeys.validate()?;
        if !self.pointer_gain.is_finite() || self.pointer_gain <= 0.0 {
            return Err(ConfigError::invalid(
                "input.pointer_gain",
                self.pointer_gain,
                "a positive finite factor (default 1.0)",
            ));
        }
        Ok(())
    }
}

/// `[clipboard]`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClipboardConfig {
    /// Opt-in on both sides.
    #[serde(default)]
    pub enabled: bool,
    /// Skip content marked as a secret by password managers.
    #[serde(default = "crate::truthy")]
    pub skip_secret_marked: bool,
    /// Maximum transfer size; at most 1 MiB (the protocol limit).
    #[serde(default = "default_clipboard_max_bytes")]
    pub max_bytes: u64,
    /// Per-channel rate limit.
    #[serde(default = "default_clipboard_rate")]
    pub rate_bytes_per_s: u64,
    /// When an offered payload is requested.
    #[serde(default)]
    pub transfer: ClipboardTransfer,
}

impl Default for ClipboardConfig {
    fn default() -> Self {
        ClipboardConfig {
            enabled: false,
            skip_secret_marked: true,
            max_bytes: default_clipboard_max_bytes(),
            rate_bytes_per_s: default_clipboard_rate(),
            transfer: ClipboardTransfer::default(),
        }
    }
}

fn default_clipboard_max_bytes() -> u64 {
    MAX_CLIPBOARD_PAYLOAD_BYTES
}

fn default_clipboard_rate() -> u64 {
    262_144
}

impl ClipboardConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        if self.max_bytes > MAX_CLIPBOARD_PAYLOAD_BYTES {
            return Err(ConfigError::coded(
                ErrorCode::Clipboard,
                "clipboard.max_bytes",
                self.max_bytes,
                format!("0..={MAX_CLIPBOARD_PAYLOAD_BYTES} (1 MiB protocol limit)"),
            ));
        }
        if self.rate_bytes_per_s == 0 {
            return Err(ConfigError::invalid(
                "clipboard.rate_bytes_per_s",
                self.rate_bytes_per_s,
                ">= 1 (default 262144, 256 KiB/s per channel)",
            ));
        }
        Ok(())
    }
}

/// `[ui]`.
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UiConfig {
    /// GUI language.
    #[serde(default)]
    pub language: Language,
    /// Start on login; enabled only after a successful test.
    #[serde(default)]
    pub autostart: bool,
}

/// `[appearance]`: the small visual effects at linked edges. Read by the core and pushed to the
/// agent (the overlay runs without the GUI); the GUI-local `gui.toml` is not involved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AppearanceConfig {
    /// The ASCII strip that gathers toward the cursor at a linked edge, and its release on
    /// the machine the cursor arrives at.
    #[serde(default = "default_true")]
    pub edge_effects: bool,
    /// Hide the cursor on this machine while input is on another one (Windows: a small
    /// veil window tied to the agent process; Wayland: the portal already hides it).
    #[serde(default = "default_true")]
    pub hide_remote_cursor: bool,
}

impl Default for AppearanceConfig {
    fn default() -> Self {
        Self {
            edge_effects: true,
            hide_remote_cursor: true,
        }
    }
}

const fn default_true() -> bool {
    true
}

/// `[log]`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LogConfig {
    /// Minimum level; never logs keys or clipboard content.
    #[serde(default)]
    pub level: LogLevel,
    /// Rotation: maximum size of one file.
    #[serde(default = "default_max_file_mb")]
    pub max_file_mb: u32,
    /// Rotation: how many files to keep.
    #[serde(default = "default_max_files")]
    pub max_files: u32,
}

impl Default for LogConfig {
    fn default() -> Self {
        LogConfig {
            level: LogLevel::default(),
            max_file_mb: default_max_file_mb(),
            max_files: default_max_files(),
        }
    }
}

fn default_max_file_mb() -> u32 {
    5
}

fn default_max_files() -> u32 {
    5
}

impl LogConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        if self.max_file_mb == 0 {
            return Err(ConfigError::invalid(
                "log.max_file_mb",
                self.max_file_mb,
                ">= 1 (default 5)",
            ));
        }
        if self.max_files == 0 {
            return Err(ConfigError::invalid(
                "log.max_files",
                self.max_files,
                ">= 1 (default 5)",
            ));
        }
        Ok(())
    }
}

/// `[topology]` — the applied layout document plus the monotone floor.
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TopologyConfig {
    /// Applied revision; monotone, never goes backwards.
    #[serde(default)]
    pub revision: u64,
    /// Author of the applied revision.
    #[serde(default)]
    pub author: String,
    /// Monotone floor: the highest revision seen together with its author.
    #[serde(default)]
    pub max_seen_revision: u64,
    /// Author applied together with `max_seen_revision`.
    #[serde(default)]
    pub max_seen_author: String,
    /// Display snapshot for the GUI; the authoritative source is each node's
    /// agent, so this list is never turned into geometry displays.
    #[serde(default)]
    pub displays: Vec<DisplayConfig>,
    /// Transition graph of any shape.
    #[serde(default)]
    pub links: Vec<LinkConfig>,
    /// Nodes explicitly removed from the shared map; direct trust remains local.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub forgotten_nodes: Vec<String>,
}

impl TopologyConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        if self.revision > 0 && self.author.is_empty() {
            return Err(ConfigError::coded(
                ErrorCode::Topology,
                "topology.author",
                "<empty>",
                "the node_id of the applied revision's author when topology.revision > 0",
            ));
        }
        if self.max_seen_revision < self.revision {
            return Err(ConfigError::coded(
                ErrorCode::Topology,
                "topology.max_seen_revision",
                self.max_seen_revision,
                format!(
                    ">= topology.revision ({}) — the monotone floor never trails the applied \
                     revision",
                    self.revision
                ),
            ));
        }
        if self.max_seen_revision > 0 && self.max_seen_author.is_empty() {
            return Err(ConfigError::coded(
                ErrorCode::Topology,
                "topology.max_seen_author",
                "<empty>",
                "the author applied together with topology.max_seen_revision",
            ));
        }

        let mut seen: Vec<&str> = Vec::with_capacity(self.links.len());
        for link in &self.links {
            let key = "topology.links.id";
            if link.id.is_empty() {
                return Err(ConfigError::coded(
                    ErrorCode::Topology,
                    key,
                    "<empty>",
                    "a unique, non-empty link id",
                ));
            }
            if seen.contains(&link.id.as_str()) {
                return Err(ConfigError::coded(
                    ErrorCode::Topology,
                    key,
                    &link.id,
                    "a link id unique inside the document",
                ));
            }
            seen.push(&link.id);

            for (side, endpoint) in [("from", &link.from), ("to", &link.to)] {
                let key = match side {
                    "from" => "topology.links.from",
                    _ => "topology.links.to",
                };
                if endpoint.node.is_empty() || endpoint.display.is_empty() {
                    return Err(ConfigError::coded(
                        ErrorCode::Topology,
                        key,
                        format!(
                            "node = {:?}, display = {:?}",
                            endpoint.node, endpoint.display
                        ),
                        "a non-empty node reference (\"self\" or \"peer:<64 hex digits>\") and a \
                         non-empty display id",
                    ));
                }
                if !endpoint.is_auto()
                    && (!endpoint.start.is_finite()
                        || !endpoint.end.is_finite()
                        || endpoint.start >= endpoint.end)
                {
                    return Err(ConfigError::coded(
                        ErrorCode::Topology,
                        key,
                        format!("start = {}, end = {}", endpoint.start, endpoint.end),
                        "finite 0 <= start < end (distance along the edge, logical units)",
                    ));
                }
            }
            if link.from.node == link.to.node {
                return Err(ConfigError::coded(
                    ErrorCode::Topology,
                    "topology.links",
                    &link.id,
                    "a link between two different nodes",
                ));
            }
        }
        Ok(())
    }
}

/// One `[[topology.displays]]` snapshot entry.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DisplayConfig {
    /// `"self"` or `"peer:<64 hex digits>"`.
    pub node: String,
    /// Stable display id (EDID hash or connector name).
    pub id: String,
    /// Logical bounds `[x, y, width, height]` in the node's own logical space.
    pub logical: [f64; 4],
    /// Origin of the node logical space.
    #[serde(default)]
    pub primary: bool,
}

/// One `[[topology.links]]` entry: a transition between two display edges.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LinkConfig {
    /// Stable link key, unique inside the document.
    pub id: String,
    /// Where the pointer leaves when travelling forward.
    pub from: EndpointConfig,
    /// Where the pointer appears when travelling forward.
    pub to: EndpointConfig,
    /// `true` also makes the reverse direction an exit.
    #[serde(default)]
    pub bidirectional: bool,
}

/// One end of a [`LinkConfig`].
///
/// `display` is either a stable display id or the sentinel [`crate::AUTO_DISPLAY`] (`"auto"`),
/// which anchors the endpoint at the whole desktop edge of `node` with no monitor named. An
/// auto endpoint carries no meaningful segment: `start`/`end` may be omitted (they default to
/// `0`), and are ignored by the resolver.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointConfig {
    /// `"self"` or `"peer:<64 hex digits>"`.
    pub node: String,
    /// Display inside that node, or [`crate::AUTO_DISPLAY`] for the whole desktop.
    pub display: String,
    /// Which edge of the display (or of the desktop, for an auto anchor).
    pub edge: Edge,
    /// Distance along the edge where the segment starts; ignored for an auto anchor.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub start: f64,
    /// Distance along the edge where the segment ends; `> start`. Ignored for an auto anchor.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub end: f64,
}

impl EndpointConfig {
    /// `true` when this endpoint is anchored at the node's whole desktop rather than at one
    /// monitor.
    pub fn is_auto(&self) -> bool {
        self.display == crate::AUTO_DISPLAY
    }
}

/// Serde helper: a `0.0` segment bound is the default and is not written.
fn is_zero(value: &f64) -> bool {
    *value == 0.0
}
