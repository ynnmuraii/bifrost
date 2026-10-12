//! The source side: a node that copied something, and the offers it may make.
//!
//! ADR-008 and `docs/05-protocol.md` fix three rules this module owns:
//!
//! * **Fan-out, not a loop.** A local copy is offered to every peer (the caller's loop over
//!   N−1 links), but «полученный payload никогда не становится новым локальным offer» — a
//!   payload that arrived from a peer is applied locally and marked as *not* ours, so the
//!   next `local_copy` is the only thing that mints a new version.
//! * **Local copy wins.** «Локальное копирование побеждает»: a fresh local copy cancels an
//!   incoming transfer, so the source's version only ever moves forward.
//! * **The version is per origin.** Every node's counter is its own; the wire carries
//!   `(origin_node_id, origin_version)` so a peer can suppress an echo without knowing what
//!   the other nodes are doing.

use crate::echo::{Echo, EchoDecision};
use crate::policy::{Content, Decision, Marker, Policy};
use crate::{ClipboardError, MAX_BYTES, MIME};

/// One offer as the wire carries it (`ClipboardOffer`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    /// `random16`.
    pub offer_id: [u8; 16],
    /// The node that copied it.
    pub origin: [u8; 32],
    /// That node's version counter.
    pub version: u64,
    /// Always [`MIME`]: ADR-008 allows text only.
    pub mime: String,
    /// Payload size in bytes.
    pub size: u64,
}

impl Offer {
    /// Whether this offer is the documented MIME type and inside the limits.
    ///
    /// The two refusals are distinct on purpose: `CLIPBOARD_TOO_LARGE` is a size the peer
    /// could shrink, `CLIPBOARD_INVALID` is a payload this node will never accept.
    pub fn is_acceptable(&self) -> Result<(), ClipboardError> {
        if self.mime != MIME {
            return Err(ClipboardError::Invalid);
        }
        if self.size > MAX_BYTES {
            return Err(ClipboardError::TooLarge);
        }
        Ok(())
    }
}

/// Why a local copy did not become an offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The policy refused it (`DISABLED`, `TOO_LARGE`, `SECRET`).
    Policy(ClipboardError),
}

/// What the caller should do with an incoming offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Incoming {
    /// Ask for it (`ClipboardRequest`).
    Request,
    /// Ignore it: the same version again, or one older than we have seen.
    Ignore(EchoDecision),
}

/// The clipboard source of one node.
#[derive(Debug, Clone)]
pub struct Source {
    /// This node's identity, as `origin_node_id`.
    pub origin: [u8; 32],
    echo: Echo,
    version: u64,
    /// Whether the currently applied content came from a peer. A payload that came from the
    /// wire is never offered onward, which is the property that stops the fan-out loop.
    applied_from_peer: bool,
}

impl Source {
    /// A node that has copied nothing yet.
    pub fn new(origin: [u8; 32]) -> Self {
        Source {
            origin,
            echo: Echo::new(),
            version: 0,
            applied_from_peer: false,
        }
    }

    /// The version counter, for diagnostics.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Whether the applied content came from a peer.
    pub fn applied_from_peer(&self) -> bool {
        self.applied_from_peer
    }

    /// A local copy: the only path that mints a new version and may be offered.
    ///
    /// `offer_id` is supplied by the caller (production mints `random16`), so the decision
    /// logic stays deterministic in tests.
    pub fn local_copy(
        &mut self,
        content: &Content,
        markers: &[Marker],
        policy: &Policy,
        offer_id: [u8; 16],
    ) -> Result<Offer, Refusal> {
        let mut content = content.clone();
        content.markers.extend_from_slice(markers);
        match policy.decide(&content) {
            Decision::Send => {}
            Decision::Refuse(reason) => return Err(Refusal::Policy(reason)),
        }
        self.version += 1;
        // A local copy is ours, and it cancels any incoming transfer (the caller cancels the
        // sink; the version bump is what makes the old offer stale everywhere).
        self.applied_from_peer = false;
        Ok(Offer {
            offer_id,
            origin: self.origin,
            version: self.version,
            mime: MIME.to_owned(),
            size: content.bytes,
        })
    }

    /// A payload that arrived from a peer and was applied locally.
    ///
    /// It is recorded in the echo set — so the same version coming back over another link is
    /// a duplicate — and marked as not ours, so the fan-out stops here.
    pub fn apply_from_peer(&mut self, offer: &Offer) {
        self.echo.accept(&offer.origin, offer.version);
        self.applied_from_peer = true;
    }

    /// Judges an incoming offer: may this node ask for it?
    pub fn incoming(&self, offer: &Offer, policy: &Policy) -> Result<Incoming, ClipboardError> {
        if !policy.enabled {
            return Err(ClipboardError::Disabled);
        }
        offer.is_acceptable()?;
        if offer.size > policy.max_bytes {
            return Err(ClipboardError::TooLarge);
        }
        match self.echo.judge(&offer.origin, offer.version) {
            EchoDecision::Accept => Ok(Incoming::Request),
            other => Ok(Incoming::Ignore(other)),
        }
    }

    /// Records an accepted offer, so the next judgement of the same version is a duplicate.
    pub fn accept(&mut self, offer: &Offer) {
        self.echo.accept(&offer.origin, offer.version);
    }

    /// The echo history, for diagnostics and for the harness.
    pub fn echo(&self) -> &Echo {
        &self.echo
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Content;

    fn node(byte: u8) -> [u8; 32] {
        [byte; 32]
    }

    fn offer_id(byte: u8) -> [u8; 16] {
        [byte; 16]
    }

    fn enabled() -> Policy {
        Policy {
            enabled: true,
            skip_secret_marked: true,
            max_bytes: MAX_BYTES,
        }
    }

    #[test]
    fn a_local_copy_mints_a_new_version_and_offers_it() {
        let mut source = Source::new(node(1));
        let first = source
            .local_copy(&Content::text(5), &[], &enabled(), offer_id(1))
            .expect("offered");
        assert_eq!(first.version, 1);
        assert_eq!(first.origin, node(1));
        assert_eq!(first.mime, MIME);
        let second = source
            .local_copy(&Content::text(6), &[], &enabled(), offer_id(2))
            .expect("offered");
        assert_eq!(second.version, 2);
        assert!(!source.applied_from_peer());
    }

    #[test]
    fn a_disabled_or_secret_copy_makes_no_offer_at_all() {
        let mut source = Source::new(node(1));
        assert_eq!(
            source.local_copy(
                &Content::text(5),
                &[],
                &Policy {
                    enabled: false,
                    ..enabled()
                },
                offer_id(1)
            ),
            Err(Refusal::Policy(ClipboardError::Disabled))
        );
        assert_eq!(
            source.local_copy(
                &Content::text(5),
                &[Marker::KdeSecret],
                &enabled(),
                offer_id(1)
            ),
            Err(Refusal::Policy(ClipboardError::Secret))
        );
        assert_eq!(
            source.local_copy(&Content::text(MAX_BYTES + 1), &[], &enabled(), offer_id(1)),
            Err(Refusal::Policy(ClipboardError::TooLarge))
        );
        // Nothing was minted, so a peer would never see a version from this node.
        assert_eq!(source.version(), 0);
    }

    #[test]
    fn a_payload_from_a_peer_is_never_offered_onward() {
        // The fan-out loop, closed: B applies A's payload and does *not* mint a version of
        // its own, so A and C see nothing new from B.
        let mut a = Source::new(node(1));
        let offer = a
            .local_copy(&Content::text(3), &[], &enabled(), offer_id(1))
            .expect("offered");
        let mut b = Source::new(node(2));
        assert_eq!(b.incoming(&offer, &enabled()), Ok(Incoming::Request));
        b.accept(&offer);
        b.apply_from_peer(&offer);
        assert!(b.applied_from_peer());
        assert_eq!(b.version(), 0, "a received payload must not mint a version");
        // A re-offer of the same version, arriving at B over another link, is a duplicate.
        assert_eq!(
            b.incoming(&offer, &enabled()),
            Ok(Incoming::Ignore(EchoDecision::Duplicate))
        );
        // C sees the original offer once and then nothing from B.
        let mut c = Source::new(node(3));
        assert_eq!(c.incoming(&offer, &enabled()), Ok(Incoming::Request));
        c.accept(&offer);
        assert_eq!(
            c.incoming(&offer, &enabled()),
            Ok(Incoming::Ignore(EchoDecision::Duplicate))
        );
    }

    #[test]
    fn a_local_copy_beats_an_incoming_transfer() {
        let mut b = Source::new(node(2));
        let from_a = Offer {
            offer_id: offer_id(9),
            origin: node(1),
            version: 4,
            mime: MIME.to_owned(),
            size: 10,
        };
        assert_eq!(b.incoming(&from_a, &enabled()), Ok(Incoming::Request));
        // The user copies something locally while the transfer is in flight: the local copy
        // wins, and the incoming version is now stale for this node's own history.
        let local = b
            .local_copy(&Content::text(2), &[], &enabled(), offer_id(5))
            .expect("offered");
        assert_eq!(local.version, 1);
        assert!(!b.applied_from_peer());
        assert_eq!(
            b.incoming(&from_a, &enabled()),
            Ok(Incoming::Request),
            "the peer's own version line is independent of ours"
        );
        b.accept(&from_a);
        assert_eq!(
            b.incoming(&from_a, &enabled()),
            Ok(Incoming::Ignore(EchoDecision::Duplicate))
        );
    }

    #[test]
    fn an_offer_outside_the_limits_is_refused() {
        let source = Source::new(node(1));
        let too_big = Offer {
            offer_id: offer_id(1),
            origin: node(9),
            version: 1,
            mime: MIME.to_owned(),
            size: MAX_BYTES + 1,
        };
        assert_eq!(
            source.incoming(&too_big, &enabled()),
            Err(ClipboardError::TooLarge)
        );
        let wrong_mime = Offer {
            mime: "image/png".to_owned(),
            size: 10,
            ..too_big
        };
        assert_eq!(
            source.incoming(&wrong_mime, &enabled()),
            Err(ClipboardError::Invalid)
        );
        let disabled = Policy {
            enabled: false,
            ..enabled()
        };
        let fine = Offer {
            size: 10,
            ..wrong_mime
        };
        assert_eq!(
            source.incoming(&fine, &disabled),
            Err(ClipboardError::Disabled)
        );
    }
}
