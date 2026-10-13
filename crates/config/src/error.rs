//! Typed configuration errors.
//!
//! `docs/03-architecture.md`, «Валидация при загрузке», attaches a `CONFIG_*`
//! code to each rule; [`ErrorCode`] carries exactly those strings so the GUI can
//! show the code next to the message. [`ConfigError::Invalid`] lists the key,
//! the offending value and what was allowed.

use core::fmt;
use std::io;
use std::path::PathBuf;

/// The `CONFIG_*` / `INVALID_TOPOLOGY` codes of the load-validation table in
/// `docs/03-architecture.md`.
///
/// Some codes are declared by the documents but can only be raised by the
/// environment layer, never by this loader; each variant says so.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    /// `network.listen_address`: a concrete private LAN address; `0.0.0.0` needs
    /// `allow_any_interface = true`.
    Listen,
    /// `network.max_members`: `1..4` (`N_max = 4`).
    Members,
    /// `workspace.claim_hotkey` / `input.emergency_hotkey`: modifier count,
    /// distinctness, and the Linux VT rule.
    Hotkey,
    /// `workspace.owner_claim_serial` / `owner_node_id`: the pair must be
    /// consistent.
    Owner,
    /// `timing.*`: each timeout must stay inside its documented range.
    Timing,
    /// `topology.*`: the integrity rules of the layout document.
    Topology,
    /// `clipboard.max_bytes`: at most 1 MiB.
    Clipboard,
    /// `platform.backend`: a manually selected backend must match the session.
    /// Declared by the table but raised by the platform layer, not by this
    /// loader — the loader only rejects an unknown backend *value* at parse time.
    Backend,
    /// `input.takeover_detection = "evdev"` requires membership in the `input`
    /// group. Declared by the table but raised by the platform layer, not by
    /// this loader.
    Takeover,
}

impl ErrorCode {
    /// The code as written in the documents.
    pub const fn as_str(self) -> &'static str {
        match self {
            ErrorCode::Listen => "CONFIG_LISTEN",
            ErrorCode::Members => "CONFIG_MEMBERS",
            ErrorCode::Hotkey => "CONFIG_HOTKEY",
            ErrorCode::Owner => "CONFIG_OWNER",
            ErrorCode::Timing => "CONFIG_TIMING",
            ErrorCode::Topology => "INVALID_TOPOLOGY",
            ErrorCode::Clipboard => "CONFIG_CLIPBOARD",
            ErrorCode::Backend => "CONFIG_BACKEND",
            ErrorCode::Takeover => "CONFIG_TAKEOVER",
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Everything that can go wrong while locating, reading, parsing, validating or
/// writing the configuration.
#[derive(Debug)]
pub enum ConfigError {
    /// Another writer held the configuration lock for longer than the wait allows.
    Locked {
        /// The lock file that could not be taken.
        path: PathBuf,
    },
    /// The file could not be read or written.
    Io {
        /// File the operation addressed.
        path: PathBuf,
        /// Underlying OS error.
        source: io::Error,
    },
    /// The text is not valid TOML, has an unknown key, a wrong type, or an
    /// unknown enum variant. The message lists the allowed values.
    Parse {
        /// `toml` error with the offending key and expected shape.
        source: toml::de::Error,
    },
    /// The configuration could not be rendered back to TOML (only reachable for
    /// non-finite floats).
    Serialize {
        /// `toml` serialization error.
        source: toml::ser::Error,
    },
    /// `config_version` is absent.
    MissingVersion,
    /// `config_version` is present but not [`SUPPORTED_CONFIG_VERSION`].
    ///
    /// [`SUPPORTED_CONFIG_VERSION`]: crate::SUPPORTED_CONFIG_VERSION
    UnsupportedVersion {
        /// The value found in the file, as written.
        found: String,
    },
    /// A documented validation rule failed.
    Invalid {
        /// `CONFIG_*` code from the validation table, when the rule has one.
        code: Option<ErrorCode>,
        /// Dotted key path, e.g. `timing.lease_ttl_ms`.
        key: &'static str,
        /// The offending value, rendered.
        value: String,
        /// The allowed range or rule.
        expected: String,
    },
    /// A documented per-OS path could not be resolved (missing environment
    /// variables such as `APPDATA` or `HOME`).
    Paths {
        /// What was missing and what it was needed for.
        detail: String,
    },
}

impl ConfigError {
    /// A rule violation without a documented code.
    pub fn invalid(
        key: &'static str,
        value: impl fmt::Display,
        expected: impl Into<String>,
    ) -> Self {
        ConfigError::Invalid {
            code: None,
            key,
            value: value.to_string(),
            expected: expected.into(),
        }
    }

    /// A rule violation carrying its `CONFIG_*` code.
    pub fn coded(
        code: ErrorCode,
        key: &'static str,
        value: impl fmt::Display,
        expected: impl Into<String>,
    ) -> Self {
        ConfigError::Invalid {
            code: Some(code),
            key,
            value: value.to_string(),
            expected: expected.into(),
        }
    }

    /// The documented error code for this failure, when there is one.
    pub const fn code(&self) -> Option<ErrorCode> {
        match self {
            ConfigError::Invalid { code, .. } => *code,
            _ => None,
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Locked { path } => write!(
                f,
                "{}: another writer holds the configuration lock ({})",
                path.display(),
                crate::lock::ACQUIRE_TIMEOUT.as_secs()
            ),
            ConfigError::Io { path, source } => {
                write!(f, "{}: {source}", path.display())
            }
            ConfigError::Parse { source } => write!(f, "invalid config: {source}"),
            ConfigError::Serialize { source } => write!(f, "cannot serialize config: {source}"),
            ConfigError::MissingVersion => write!(
                f,
                "config_version is missing; expected {} (docs/03-architecture.md, \
                 «Валидация при загрузке»)",
                crate::SUPPORTED_CONFIG_VERSION
            ),
            ConfigError::UnsupportedVersion { found } => write!(
                f,
                "config_version = {found} is not supported; only {} is (a v1 file needs the \
                 v1->v2 migration, which is not implemented here)",
                crate::SUPPORTED_CONFIG_VERSION
            ),
            ConfigError::Invalid {
                code,
                key,
                value,
                expected,
            } => {
                if let Some(code) = code {
                    write!(f, "{code} ")?;
                }
                write!(f, "{key} = {value}: expected {expected}")
            }
            ConfigError::Paths { detail } => write!(f, "cannot resolve config paths: {detail}"),
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConfigError::Io { source, .. } => Some(source),
            ConfigError::Parse { source } => Some(source),
            ConfigError::Serialize { source } => Some(source),
            _ => None,
        }
    }
}
