//! Revocable, per-input-link key derivation for optional features.
//!
//! Every authenticated runtime input link exports with the fixed label
//! `EXPORTER-bifrost-aux-v1` and explicit empty TLS context (`Some(&[])`),
//! identically on both TLS roles. The root never leaves transport and is exposed
//! for derivation only after validated Hello. Consumers gate on their own feature
//! capabilities; this generic seam adds no wire capability or payload.
//!
//! The exporter root stays in one heap allocation and is zeroized on revocation
//! or drop. Derived outputs are zeroized on drop too. ring's internal HKDF PRK
//! and HMAC copies are not guaranteed to be erased.

use std::sync::{Arc, Mutex, Weak};

use ring::hkdf;
use zeroize::Zeroizing;

use crate::{NodeId, Stream, TransportError};

const EXPORTER_LABEL: &[u8] = b"EXPORTER-bifrost-aux-v1";
const HKDF_SALT: &[u8] = b"bifrost-aux-hkdf-v1";

pub(crate) type ExporterRoot = Box<Zeroizing<[u8; 32]>>;

/// Registered, direction-specific uses of auxiliary key material.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuxPurpose {
    AudioForwardKey,
    AudioReverseKey,
    AudioForwardNonce,
    AudioReverseNonce,
}

impl AuxPurpose {
    fn label(self) -> &'static [u8] {
        match self {
            Self::AudioForwardKey => b"bifrost-audio-v1/forward-key",
            Self::AudioReverseKey => b"bifrost-audio-v1/reverse-key",
            Self::AudioForwardNonce => b"bifrost-audio-v1/forward-nonce",
            Self::AudioReverseNonce => b"bifrost-audio-v1/reverse-nonce",
        }
    }
}

/// A derivation failed without exposing any key material.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AuxiliaryDeriveError {
    #[error("the link is closed")]
    LinkClosed,
    #[error("auxiliary derivation context is shorter than 64 bytes")]
    ContextTooShort,
    #[error("auxiliary derivation context exceeds 256 bytes")]
    ContextTooLong,
}

/// Owned key material, erased on drop and only readable through explicit exposure.
///
/// Secret outputs cannot be formatted or implicitly duplicated.
///
/// ```compile_fail
/// fn require_debug<T: std::fmt::Debug>() {}
/// require_debug::<transport::DerivedKey>();
/// ```
///
/// ```compile_fail
/// fn require_clone<T: Clone>() {}
/// require_clone::<transport::DerivedKey>();
/// ```
///
/// ```compile_fail
/// fn require_copy<T: Copy>() {}
/// require_copy::<transport::DerivedKey>();
/// ```
pub struct DerivedKey(Zeroizing<[u8; 32]>);

impl DerivedKey {
    /// Explicitly borrow the secret bytes; any copies must be erased by the caller.
    pub fn expose_secret(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Identity and local lifetime of an authenticated input connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuxiliaryBinding {
    /// The authenticated remote node.
    pub peer: NodeId,
    /// The actual dialer-chosen connection id negotiated by Hello.
    pub connection_id: [u8; 16],
    /// The manager's unique local connection generation (`LinkConfig::key`).
    pub generation: [u8; 16],
}

struct BoundSecret {
    root: ExporterRoot,
    binding: AuxiliaryBinding,
}

enum Slot {
    Pending,
    Ready(BoundSecret),
    Closed,
}

/// Shared only inside transport; closing any link handle revokes every outstanding view.
pub(crate) struct AuxiliaryState(Mutex<Slot>);

impl std::fmt::Debug for AuxiliaryState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AuxiliaryState([REDACTED])")
    }
}

impl AuxiliaryState {
    pub(crate) fn new() -> Self {
        Self(Mutex::new(Slot::Pending))
    }

    pub(crate) fn establish(&self, root: ExporterRoot, binding: AuxiliaryBinding) {
        let mut slot = self.0.lock().unwrap_or_else(|error| error.into_inner());
        // close() may race a completed Hello; a revoked slot must never be resurrected.
        if matches!(*slot, Slot::Pending) {
            *slot = Slot::Ready(BoundSecret { root, binding });
        }
    }

    pub(crate) fn invalidate(&self) {
        *self.0.lock().unwrap_or_else(|error| error.into_inner()) = Slot::Closed;
    }

    pub(crate) fn handle(self: &Arc<Self>) -> Option<AuxiliarySecret> {
        let slot = self.0.lock().unwrap_or_else(|error| error.into_inner());
        let Slot::Ready(secret) = &*slot else {
            return None;
        };
        Some(AuxiliarySecret {
            state: Arc::downgrade(self),
            binding: secret.binding,
        })
    }
}

/// A non-Clone, revocable derivation view, not the exporter root.
///
/// A retained view stops deriving as soon as the parent closes or is replaced.
/// Already derived session keys belong to the consumer: discard them on parent
/// down/replacement and reject stale asynchronous results using [`Self::binding`].
pub struct AuxiliarySecret {
    state: Weak<AuxiliaryState>,
    binding: AuxiliaryBinding,
}

impl std::fmt::Debug for AuxiliarySecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuxiliarySecret")
            .field("binding", &self.binding)
            .field("secret", &"[REDACTED]")
            .finish()
    }
}

impl AuxiliarySecret {
    /// The peer, connection id and local generation to validate asynchronous completions.
    pub fn binding(&self) -> AuxiliaryBinding {
        self.binding
    }

    /// Derive a 32-byte key without exposing the root; fail after parent revocation.
    ///
    /// Context MUST include both node ids in canonical order and a fresh per-session
    /// nonce; encode its fields unambiguously and agree on their meaning at both
    /// peers. Context must be 64–256 bytes: the floor accommodates two 32-byte
    /// node ids, but consumers must validate their complete context schema.
    /// The local generation is intentionally not a KDF input (it differs by peer).
    ///
    /// HKDF-SHA256 extracts with salt `bifrost-aux-hkdf-v1`; expand info is
    /// `u64be(label.len) || label || u64be(context.len) || context`, using the
    /// registered purpose's fixed namespace label.
    pub fn derive(
        &self,
        purpose: AuxPurpose,
        context: &[u8],
    ) -> Result<DerivedKey, AuxiliaryDeriveError> {
        if context.len() < 64 {
            return Err(AuxiliaryDeriveError::ContextTooShort);
        }
        if context.len() > 256 {
            return Err(AuxiliaryDeriveError::ContextTooLong);
        }
        let state = self
            .state
            .upgrade()
            .ok_or(AuxiliaryDeriveError::LinkClosed)?;
        let slot = state.0.lock().unwrap_or_else(|error| error.into_inner());
        let Slot::Ready(secret) = &*slot else {
            return Err(AuxiliaryDeriveError::LinkClosed);
        };
        if secret.binding != self.binding {
            return Err(AuxiliaryDeriveError::LinkClosed);
        }
        Ok(derive_key(&secret.root, purpose, context))
    }
}

fn derive_key(root: &[u8; 32], purpose: AuxPurpose, context: &[u8]) -> DerivedKey {
    let purpose_label = purpose.label();
    let salt = hkdf::Salt::new(hkdf::HKDF_SHA256, HKDF_SALT);
    let prk = salt.extract(root);
    let label_len = (purpose_label.len() as u64).to_be_bytes();
    let context_len = (context.len() as u64).to_be_bytes();
    let info = [&label_len[..], purpose_label, &context_len[..], context];
    let mut output = DerivedKey(Zeroizing::new([0; 32]));
    prk.expand(&info, hkdf::HKDF_SHA256)
        .expect("32 bytes fit HKDF-SHA256")
        .fill(&mut *output.0)
        .expect("output matches the requested HKDF length");
    output
}

pub(crate) fn export(stream: &Stream) -> Result<ExporterRoot, TransportError> {
    let mut root = Box::new(Zeroizing::new([0; 32]));
    match stream {
        Stream::Client(stream) => {
            let connection = stream.get_ref().1;
            if connection.alpn_protocol() != Some(protocol::limits::ALPN_INPUT.as_bytes()) {
                return Err(TransportError::Protocol(
                    "auxiliary exporter requires input ALPN".into(),
                ));
            }
            connection.export_keying_material(&mut **root, EXPORTER_LABEL, Some(&[]))?;
        }
        Stream::Server(stream) => {
            let connection = stream.get_ref().1;
            if connection.alpn_protocol() != Some(protocol::limits::ALPN_INPUT.as_bytes()) {
                return Err(TransportError::Protocol(
                    "auxiliary exporter requires input ALPN".into(),
                ));
            }
            connection.export_keying_material(&mut **root, EXPORTER_LABEL, Some(&[]))?;
        }
    }
    Ok(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hkdf_sha256_matches_independent_known_answer() {
        // RFC 5869 extract/expand, independently computed with Python hmac/hashlib.
        let root = std::array::from_fn(|index| index as u8);
        let context: [u8; 64] = std::array::from_fn(|index| index as u8);
        assert_eq!(
            derive_key(&root, AuxPurpose::AudioForwardKey, &context).expose_secret(),
            &[
                19, 91, 5, 177, 65, 66, 133, 214, 83, 13, 138, 12, 15, 85, 0, 110, 188, 88, 123,
                42, 201, 198, 38, 216, 52, 91, 49, 147, 199, 192, 181, 225
            ]
        );
    }

    #[test]
    fn registered_labels_are_nonempty_and_unique() {
        let purposes = [
            AuxPurpose::AudioForwardKey,
            AuxPurpose::AudioReverseKey,
            AuxPurpose::AudioForwardNonce,
            AuxPurpose::AudioReverseNonce,
        ];
        for (index, purpose) in purposes.iter().enumerate() {
            assert!(!purpose.label().is_empty());
            for other in &purposes[..index] {
                assert_ne!(purpose.label(), other.label());
            }
        }
    }

    #[test]
    fn a_closed_binding_cannot_be_resurrected() {
        let state = Arc::new(AuxiliaryState::new());
        let binding = AuxiliaryBinding {
            peer: NodeId::new([1; 32]),
            connection_id: [2; 16],
            generation: [3; 16],
        };
        assert!(state.handle().is_none());
        state.invalidate();
        state.establish(Box::new(Zeroizing::new([4; 32])), binding);
        assert!(state.handle().is_none());
    }
}
