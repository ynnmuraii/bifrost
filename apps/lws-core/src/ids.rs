//! Production identifiers for the focus FSM.
//!
//! `focus-router` ships [`fr::SequentialIds`] «for tests and for replaying a recorded
//! session; not for production (it is not random and repeats after `2^64` calls)». A lease
//! id that repeats is not a cosmetic problem: a receiver remembers the ids it burned and
//! answers a reused one with `ERROR_CODE_LEASE_MISMATCH` (the E2 smoke hit exactly that when
//! a restarted core replayed `SequentialIds` from 1), and a repeated nonce would let a
//! replay of an old `LeaseRenew` extend a live lease.
//!
//! `docs/05-protocol.md` asks for «`lease_id` — random16», so both ids and nonces come from
//! the same system entropy source the identity layer uses for keys
//! ([`identity::random16`], ring's `SystemRandom`).

use focus_router as fr;

/// The focus router this core runs.
///
/// Naming the type keeps production structurally unable to fall back to
/// `FocusRouter<SequentialIds>`: the struct's default type parameter *is* the replay source,
/// so a bare `FocusRouter` in a signature is a production bug waiting to happen.
pub type Router = fr::FocusRouter<RandomIds>;

/// Random `lease_id`/`nonce` source: the only [`fr::Identifiers`] production may use.
#[derive(Debug, Clone, Copy, Default)]
pub struct RandomIds;

impl RandomIds {
    /// Checks the system entropy source once, at startup.
    ///
    /// [`fr::Identifiers`] cannot report a failure, and inventing an id would be worse than
    /// stopping (a repeated id breaks the peer's burned-lease set), so the daemon refuses
    /// to start when the source is already broken instead of discovering it mid-lease.
    pub fn new() -> Result<Self, String> {
        identity::random16()
            .map(|_| Self)
            .map_err(|error| format!("the system entropy source is unavailable: {error}"))
    }
}

impl fr::Identifiers for RandomIds {
    fn lease_id(&mut self) -> fr::LeaseId {
        random16()
    }

    fn nonce(&mut self) -> fr::Nonce {
        random16()
    }
}

/// One 16-byte draw, or a stop.
///
/// `SystemRandom::fill` failing after a successful startup probe means the platform entropy
/// source went away; every alternative (a counter, a constant, a zeroed id) is the contract
/// violation this module exists to prevent, so the process stops with the reason.
fn random16() -> [u8; 16] {
    identity::random16().expect("the system entropy source failed while minting an id")
}

#[cfg(test)]
mod tests {
    use super::*;
    use fr::Identifiers;

    #[test]
    fn ids_and_nonces_are_random_and_never_repeat() {
        let mut ids = RandomIds::new().expect("entropy");
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..1_000 {
            let lease_id = ids.lease_id();
            let nonce = ids.nonce();
            assert_ne!(lease_id, [0u8; 16], "a zeroed id is not random");
            assert_ne!(nonce, [0u8; 16], "a zeroed nonce is not random");
            assert!(seen.insert(lease_id), "a lease id repeated");
            assert!(seen.insert(nonce), "a nonce repeated");
        }
        // `SequentialIds` puts a counter in the first eight bytes and leaves the rest zero;
        // a production id must not have that shape, or a restarted node would replay it.
        assert!(
            ids.lease_id()[8..].iter().any(|byte| *byte != 0),
            "the id looks like the sequential test source"
        );
    }

    #[test]
    fn the_identifiers_are_not_shared_between_instances() {
        let mut first = RandomIds::new().expect("entropy");
        let mut second = RandomIds::new().expect("entropy");
        assert_ne!(first.lease_id(), second.lease_id());
        assert_ne!(first.nonce(), second.nonce());
    }
}
