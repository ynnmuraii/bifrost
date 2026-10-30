//! Revocable, per-input-link key derivation for optional features.
//!
//! Every authenticated runtime input link exports with the fixed label
//! `EXPORTER-bifrost-aux-v1` and explicit empty TLS context (`Some(&[])`),
//! identically on both TLS roles. The root never leaves transport and is exposed
//! for derivation only after validated Hello. Consumers gate on their own feature
//! capabilities; this generic seam adds no wire capability or payload.

use std::sync::{Arc, Mutex, Weak};

use ring::hkdf;
use zeroize::Zeroizing;

use crate::{NodeId, Stream, TransportError};

const EXPORTER_LABEL: &[u8] = b"EXPORTER-bifrost-aux-v1";
const HKDF_SALT: &[u8] = b"bifrost-aux-hkdf-v1";

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
    root: Zeroizing<[u8; 32]>,
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

    pub(crate) fn establish(&self, root: Zeroizing<[u8; 32]>, binding: AuxiliaryBinding) {
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
    /// Use a distinct purpose label for each feature and direction. Context MUST
    /// include both node ids in canonical order and a fresh per-session nonce;
    /// encode its fields unambiguously and agree on their meaning at both peers.
    /// The local generation is intentionally not a KDF input (it differs by peer).
    ///
    /// HKDF-SHA256 extracts with salt `bifrost-aux-hkdf-v1`; expand info is
    /// `u64be(label.len) || label || u64be(context.len) || context`. Treat the
    /// returned bytes as secret and erase them when the session ends.
    pub fn derive(&self, purpose_label: &[u8], context: &[u8]) -> Result<[u8; 32], TransportError> {
        let state = self.state.upgrade().ok_or(TransportError::LinkClosed)?;
        let slot = state.0.lock().unwrap_or_else(|error| error.into_inner());
        let Slot::Ready(secret) = &*slot else {
            return Err(TransportError::LinkClosed);
        };
        if secret.binding != self.binding {
            return Err(TransportError::LinkClosed);
        }
        Ok(derive_key(&secret.root, purpose_label, context))
    }
}

fn derive_key(root: &[u8; 32], purpose_label: &[u8], context: &[u8]) -> [u8; 32] {
    let salt = hkdf::Salt::new(hkdf::HKDF_SHA256, HKDF_SALT);
    let prk = salt.extract(root);
    let label_len = (purpose_label.len() as u64).to_be_bytes();
    let context_len = (context.len() as u64).to_be_bytes();
    let info = [&label_len[..], purpose_label, &context_len[..], context];
    let mut output = [0; 32];
    prk.expand(&info, hkdf::HKDF_SHA256)
        .expect("32 bytes fit HKDF-SHA256")
        .fill(&mut output)
        .expect("output matches the requested HKDF length");
    output
}

pub(crate) fn export(stream: &Stream) -> Result<Zeroizing<[u8; 32]>, TransportError> {
    let mut root = Zeroizing::new([0; 32]);
    match stream {
        Stream::Client(stream) => {
            let connection = stream.get_ref().1;
            if connection.alpn_protocol() != Some(protocol::limits::ALPN_INPUT.as_bytes()) {
                return Err(TransportError::Protocol(
                    "auxiliary exporter requires input ALPN".into(),
                ));
            }
            connection.export_keying_material(&mut *root, EXPORTER_LABEL, Some(&[]))?;
        }
        Stream::Server(stream) => {
            let connection = stream.get_ref().1;
            if connection.alpn_protocol() != Some(protocol::limits::ALPN_INPUT.as_bytes()) {
                return Err(TransportError::Protocol(
                    "auxiliary exporter requires input ALPN".into(),
                ));
            }
            connection.export_keying_material(&mut *root, EXPORTER_LABEL, Some(&[]))?;
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
        assert_eq!(
            derive_key(&root, b"feature/send", b"session/nonce"),
            [
                38, 38, 129, 236, 195, 143, 125, 132, 117, 200, 99, 190, 171, 158, 108, 90, 95,
                143, 134, 96, 137, 202, 35, 40, 176, 85, 214, 32, 7, 157, 173, 145
            ]
        );
        assert_ne!(
            derive_key(&root, b"a", b"bc"),
            derive_key(&root, b"ab", b"c")
        );
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
        state.establish(Zeroizing::new([4; 32]), binding);
        assert!(state.handle().is_none());
    }
}
