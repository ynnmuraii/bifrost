//! The node identity: an Ed25519 key pair, its self-signed certificate and `node_id`.
//!
//! # Where the key lives (`docs/03-architecture.md`, «Файлы и пути»)
//!
//! | | path |
//! |---|---|
//! | Windows | `%APPDATA%\lan-workspace\identity.dpapi` (DPAPI CurrentUser) |
//! | Linux | Secret Service; otherwise `$XDG_DATA_HOME/lan-workspace/identity.key` (0600) |
//!
//! «Каталоги создаются с правами только для пользователя: ACL на текущий SID в Windows,
//! 0700 в Linux. Core отказывается стартовать, если права шире, и выдаёт понятную
//! ошибку» — so [`NodeIdentity::load_or_create`] enforces 0700/0600 on Linux and refuses
//! a key file that a group or another user could read.
//!
//! **Two documented gaps** (both are follow-ups, not silent substitutions):
//!
//! * Windows stores the PKCS#8 key as `identity.key` under `%APPDATA%\lan-workspace`
//!   with the profile ACL. The docs' `identity.dpapi` needs `CryptProtectData`
//!   (DPAPI CurrentUser) wrapping, which is not implemented here; naming an unprotected
//!   file `.dpapi` would be a lie.
//! * Linux uses the documented 0600-file fallback. The Secret Service path (D-Bus) is
//!   not implemented.
//!
//! # Why Ed25519
//!
//! The docs only require «self-signed keys» and a `SPKI`-derived `node_id`, so the
//! algorithm is our choice. Ed25519 is the smallest surface available in both `ring` and
//! `rcgen`: one fixed scheme, no curve parameters to validate, no RSA key-size matrix,
//! and the public key is 32 bytes, which keeps the certificate and the pin trivial.

use std::path::{Path, PathBuf};

use rcgen::{CertificateParams, KeyPair, PKCS_ED25519};
use ring::rand::SecureRandom;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

use geometry::NodeId;

use crate::atomic;
use crate::spki::{fingerprint, fingerprint_grouped, node_id_from_certificate, SpkiError};

/// Failures while loading or creating the node identity.
#[derive(Debug)]
pub enum IdentityError {
    /// Filesystem failure.
    Io(std::io::Error),
    /// `rcgen` refused to generate or parse the key/certificate.
    Key(String),
    /// The certificate could not be turned into a `node_id`.
    Spki(SpkiError),
    /// `rustls` refused a configuration or a handshake input.
    Tls(String),
    /// The key file is readable by someone other than its owner.
    ///
    /// `docs/03-architecture.md`: «Core отказывается стартовать, если права шире».
    InsecurePermissions {
        /// The offending file.
        path: PathBuf,
    },
}

impl std::fmt::Display for IdentityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IdentityError::Io(error) => write!(f, "identity file error: {error}"),
            IdentityError::Key(detail) => write!(f, "identity key error: {detail}"),
            IdentityError::Spki(error) => write!(f, "identity certificate error: {error}"),
            IdentityError::Tls(detail) => write!(f, "tls configuration error: {detail}"),
            IdentityError::InsecurePermissions { path } => write!(
                f,
                "{} is readable by other users; expected 0600 for the identity key",
                path.display()
            ),
        }
    }
}

impl std::error::Error for IdentityError {}

impl From<std::io::Error> for IdentityError {
    fn from(error: std::io::Error) -> Self {
        IdentityError::Io(error)
    }
}

impl From<SpkiError> for IdentityError {
    fn from(error: SpkiError) -> Self {
        IdentityError::Spki(error)
    }
}

/// Where this node keeps its identity and its trust store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityPaths {
    /// The directory holding the private key.
    pub data_dir: PathBuf,
    /// The private key file.
    pub key_file: PathBuf,
    /// The trust store file.
    pub trust_file: PathBuf,
}

impl IdentityPaths {
    /// The per-OS paths from `docs/03-architecture.md`, «Файлы и пути».
    pub fn system() -> Result<Self, IdentityError> {
        #[cfg(unix)]
        {
            let home = std::env::var_os("HOME").map(PathBuf::from);
            let data_root = std::env::var_os("XDG_DATA_HOME")
                .map(PathBuf::from)
                .or_else(|| home.as_ref().map(|home| home.join(".local/share")))
                .ok_or_else(|| {
                    IdentityError::Key("neither XDG_DATA_HOME nor HOME is set".into())
                })?;
            let config_root = std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .or_else(|| home.as_ref().map(|home| home.join(".config")))
                .ok_or_else(|| {
                    IdentityError::Key("neither XDG_CONFIG_HOME nor HOME is set".into())
                })?;
            let data_dir = data_root.join("lan-workspace");
            Ok(Self {
                key_file: data_dir.join("identity.key"),
                trust_file: config_root.join("lan-workspace").join("trust.toml"),
                data_dir,
            })
        }
        #[cfg(windows)]
        {
            let appdata = std::env::var_os("APPDATA")
                .map(PathBuf::from)
                .ok_or_else(|| IdentityError::Key("%APPDATA% is not set".into()))?;
            let dir = appdata.join("lan-workspace");
            Ok(Self {
                key_file: dir.join("identity.key"),
                trust_file: dir.join("trust.toml"),
                data_dir: dir,
            })
        }
    }

    /// Paths rooted at an arbitrary directory: used by the pairing demo and by tests, so
    /// neither ever touches the user's real identity.
    pub fn rooted_at(directory: impl Into<PathBuf>) -> Self {
        let data_dir = directory.into();
        Self {
            key_file: data_dir.join("identity.key"),
            trust_file: data_dir.join("trust.toml"),
            data_dir,
        }
    }
}

/// Whether the identity was loaded from disk or created now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityLoad {
    /// The key file was read.
    Loaded,
    /// A new key was generated and stored.
    Created,
}

/// This node's key pair, certificate and identity.
///
/// Not `Clone`: `rcgen::KeyPair` does not implement it in 0.14, and a second copy of a
/// private key is not something this crate wants to encourage anyway.
#[derive(Debug)]
pub struct NodeIdentity {
    key_pair: KeyPair,
    certificate: CertificateDer<'static>,
    node_id: NodeId,
}

impl NodeIdentity {
    /// Generates a brand-new Ed25519 identity (does not touch the filesystem).
    pub fn generate() -> Result<Self, IdentityError> {
        let key_pair = KeyPair::generate_for(&PKCS_ED25519)
            .map_err(|error| IdentityError::Key(error.to_string()))?;
        Self::from_key_pair(key_pair)
    }

    /// Rebuilds an identity from a persisted PKCS#8 private key.
    pub fn from_pkcs8(pkcs8: &[u8]) -> Result<Self, IdentityError> {
        let key_pair = KeyPair::from_pkcs8_der_and_sign_algo(
            &PrivatePkcs8KeyDer::from(pkcs8.to_vec()),
            &PKCS_ED25519,
        )
        .map_err(|error| IdentityError::Key(error.to_string()))?;
        Self::from_key_pair(key_pair)
    }

    fn from_key_pair(key_pair: KeyPair) -> Result<Self, IdentityError> {
        // Self-signed, no CA, no SANs: the certificate is only a carrier for the public
        // key that the peer pins (`node_id = SHA-256(SPKI DER)`), and x509 path
        // validation is never performed — the custom verifier checks the pin instead.
        let params = CertificateParams::new(Vec::<String>::new())
            .map_err(|error| IdentityError::Key(error.to_string()))?;
        let certificate = params
            .self_signed(&key_pair)
            .map_err(|error| IdentityError::Key(error.to_string()))?
            .der()
            .clone();
        let node_id = node_id_from_certificate(certificate.as_ref())?;
        Ok(Self {
            key_pair,
            certificate,
            node_id,
        })
    }

    /// Loads the key from `paths`, generating and storing a fresh one when absent.
    ///
    /// Regenerating the certificate on every start is deliberate: `node_id` is derived
    /// from the *public key* (the SPKI), so peers pin an identity that survives, while
    /// the self-signed certificate bytes (serial, validity) never need to be persisted.
    pub fn load_or_create(paths: &IdentityPaths) -> Result<(Self, IdentityLoad), IdentityError> {
        if let Some(bytes) = atomic::read(&paths.key_file)? {
            if atomic::is_wider_than_owner(&paths.key_file)? {
                return Err(IdentityError::InsecurePermissions {
                    path: paths.key_file.clone(),
                });
            }
            return Ok((Self::from_pkcs8(&bytes)?, IdentityLoad::Loaded));
        }
        let identity = Self::generate()?;
        atomic::create_private_dir(&paths.data_dir)?;
        atomic::write_atomic(&paths.key_file, &identity.pkcs8_der())?;
        Ok((identity, IdentityLoad::Created))
    }

    /// The PKCS#8 DER private key, exactly what is persisted.
    pub fn pkcs8_der(&self) -> Vec<u8> {
        self.key_pair.serialize_der()
    }

    /// `node_id = SHA-256(SPKI DER)`.
    pub fn node_id(&self) -> NodeId {
        self.node_id
    }

    /// The self-signed certificate presented to peers.
    pub fn certificate(&self) -> CertificateDer<'static> {
        self.certificate.clone()
    }

    /// The private key in the shape `rustls` wants.
    ///
    /// Re-derived from the persisted PKCS#8 bytes (exactly what is written to
    /// [`IdentityPaths::key_file`]), so the key a peer proves possession of is the same
    /// key that was stored.
    pub fn private_key(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.pkcs8_der()))
    }

    /// `SHA-256(SPKI DER)` as lower-case hex, for the «Два fingerprints» display.
    pub fn fingerprint(&self) -> String {
        fingerprint(&self.node_id)
    }

    /// The same fingerprint in `aa:bb:...` groups for the "Подробнее" view (ADR-010).
    pub fn fingerprint_grouped(&self) -> String {
        fingerprint_grouped(&self.node_id)
    }
}

/// A random 16-byte value from the OS entropy source (`attempt_id`, `connection_id`).
pub fn random16() -> Result<[u8; 16], IdentityError> {
    let mut bytes = [0u8; 16];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| IdentityError::Key("the system entropy source failed".into()))?;
    Ok(bytes)
}

/// Paths rooted at `directory`, for callers that only need the trust file.
pub fn trust_file_at(directory: impl Into<PathBuf>) -> PathBuf {
    IdentityPaths::rooted_at(directory).trust_file
}

/// The key file of `paths`, exposed for diagnostics that must never print its content.
pub fn key_file_of(paths: &IdentityPaths) -> &Path {
    &paths.key_file
}
