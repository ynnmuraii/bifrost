//! Bifrost identity: node keys, pinned mTLS, the SAS pairing ceremony and the
//! trust store (KVM-030).
//!
//! # What a node identity is
//!
//! `node_id = SHA-256(SPKI DER)` of a self-signed Ed25519 key pair
//! (`docs/03-architecture.md`, ERD `NODE_IDENTITY`: «bytes32 node_id PK "SHA256 of SPKI
//! DER"»). A name, an address and a discovery record are never identity: ADR-007 states
//! «mDNS/IP — маршрутизация, не идентичность», and `docs/05-protocol.md` repeats «Название
//! устройства/адрес не являются идентичностью».
//!
//! # The four pieces
//!
//! | module | responsibility |
//! |---|---|
//! | [`key`] | load/create/persist the private key, build the self-signed certificate |
//! | [`verify`] | the pin: a custom rustls verifier that accepts exactly the pinned `node_id` |
//! | [`pairing`] | the bounded ceremony state machine (sans-IO, no clock, no socket) |
//! | [`drive`] | tokio + tokio-rustls: sockets, `PairingEnvelope` framing, exporter |
//! | [`trust`] | `TRUSTED_PEER` records, revoke, atomic writes |
//!
//! # Constants, quoted from the docs
//!
//! | constant | value | source |
//! |---|---|---|
//! | [`sas::EXPORTER_LABEL`] | `"EXPORTER-lan-workspace-pair"` | `docs/05-protocol.md`: «6-значный код сравнения `TLS-Exporter("EXPORTER-lan-workspace-pair", attempt_id, 4 байта) mod 10⁶`» |
//! | [`sas::SAS_MODULUS`] | `10⁶` | same line |
//! | [`pairing::WINDOW_MS`] | `120 s` | «Пользователь открывает окно сопряжения на узле-сервере на 120 s» |
//! | [`pairing::MAX_PENDING_PEERS`] | `3` | «не более 3 ожидающих peers» |
//! | [`pairing::MAX_ATTEMPTS`] | `3` | «при лимите 3 попытки на 120 s» |
//! | [`protocol::limits::ALPN_PAIR`] | `"lan-workspace/pair/1"` | `docs/05-protocol.md`, header |
//! | [`protocol::limits::MAX_FRAME_PAIRING`] | `8192` | «для pairing — отдельный `PairingEnvelope` с лимитом 8192 bytes» |
//!
//! # Invariants
//!
//! 1. **Unknown peers are only allowed on the pairing channel.** [`config`] refuses to
//!    build a [`verify::TrustPolicy::Pairing`] configuration for any ALPN other than
//!    `lan-workspace/pair/1`, so an unpaired node cannot reach the input or clipboard
//!    channels; the input verifier refuses it during the handshake, before any `Hello`.
//! 2. **A revoked peer cannot reconnect.** [`trust::TrustStore::pinned`] excludes revoked
//!    records, so a revoke takes effect at the next handshake by construction
//!    («TRUSTED_PEER с `revoked_at` не может открыть сеанс»).
//! 3. **The comparison code is a function of the TLS session.** It is derived from the
//!    RFC 8446 §7.5 exporter with `attempt_id` as the context, so a MITM that terminates
//!    two sessions cannot make the two codes agree («MITM держит два разных TLS-сеанса,
//!    поэтому коды не совпадут»).
//! 4. **`PairHello.node_id` must equal the SPKI hash** of the certificate the handshake
//!    just proved possession of; a mismatch rejects the ceremony (`pairing`).
//! 5. **A ceremony writes trust only after both sides approved** — the machine emits
//!    [`pairing::PairingOutput::Commit`] once, and never on a rejected or expired path
//!    («Rejected --> [*]: audit only»).
//! 6. **Re-pairing always creates a new record** ([`trust::TrustStore::trust`] replaces
//!    the whole entry), so a peer that changed its key cannot inherit the old
//!    `paired_at` or silently un-revoke.
//!
//! # Deliberate scope
//!
//! * TLS 1.3 only, `ring` backend (no cmake/nasm on Windows) — see [`config`].
//! * Ed25519 keys, because the docs leave the algorithm open and Ed25519 has the smallest
//!   surface in both `ring` and `rcgen`; the SPKI-derived `node_id` is what is pinned, so
//!   the choice stays local as long as both peers can parse each other's certificate.
//! * **Gaps, stated rather than hidden**: the Windows key is a PKCS#8 file under
//!   `%APPDATA%` with the profile ACL, *not* the documented `identity.dpapi` (DPAPI
//!   wrapping is a follow-up); Linux uses the documented `0600` file fallback instead of
//!   the Secret Service. Both are in [`key`]'s module docs.
//! * This crate does not implement the input/control link itself (framing, `Hello`,
//!   `seq`, reconnects, queues). `crates/protocol` owns those rules and
//!   `crates/transport` will own the link; here the pairing channel is the covered path.
//!
//! # Threat model (from `docs/07-quality-security.md`)
//!
//! «MITM/подмена адреса» is answered by mTLS possession + the SPKI pin + the 6-digit code
//! the users compare; «malicious peer без trust» never completes a handshake on a runtime
//! channel; «private identity keys» never leave [`key`] and are never logged
//! ([`key::IdentityPaths`] exposes paths, never bytes).

// The crate is unsafe-free except for two Windows-only modules that must call Win32 to
// protect the private key and to set the owner-only DACL: `#[allow(unsafe_code)]` is granted
// on exactly `acl` and `keystore`, where every block carries its own SAFETY comment. A
// `forbid` cannot be relaxed by an inner attribute, hence `deny` here.
#![deny(unsafe_code)]

#[cfg(windows)]
#[allow(unsafe_code)]
mod acl;
pub mod atomic;
pub mod config;
pub mod drive;
pub mod key;
// `#[allow(unsafe_code)]`: only the `#[cfg(windows)]` half of this module calls Win32
// (`CryptProtectData`/`CryptUnprotectData`); the Linux half is a plain copy.
#[allow(unsafe_code)]
pub mod keystore;
pub mod pairing;
pub mod sas;
pub mod spki;
pub mod trust;
pub mod verify;

pub use config::{
    client_config, clipboard_client_config, clipboard_server_config, pairing_client_config,
    pairing_server_config, ring_provider, server_config, server_name, trusted_client_config,
    trusted_server_config, LINK_ALPN,
};
pub use drive::{
    accept_one, accept_stream, bind, dial, open_window, PairError, PairedPeer, PairingDriver,
};
pub use key::{
    key_file_of, random16, trust_file_at, IdentityError, IdentityLoad, IdentityPaths, NodeIdentity,
};
pub use keystore::{KeystoreError, DPAPI_ENTROPY};
pub use pairing::{
    AttemptId, PairingFsm, PairingInput, PairingOutcome, PairingOutput, PairingRole, PairingState,
    PairingWindow, RejectionReason, MAX_ATTEMPTS, MAX_PENDING_PEERS, WINDOW_MS,
};
pub use sas::{SasCode, EXPORTER_BYTES, EXPORTER_LABEL, SAS_DIGITS, SAS_MODULUS};
pub use spki::{fingerprint, fingerprint_grouped, node_id_from_certificate, spki_der, SpkiError};
pub use trust::{now_unix_ms, TrustStore, TrustedPeer};
pub use verify::{
    peer_node_id, pin_error, PinError, PinnedClientVerifier, PinnedServerVerifier, TrustPolicy,
};
