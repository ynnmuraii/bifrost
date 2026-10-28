//! Deterministic two-channel model: independent HMAC keys model TLS exporter PRFs.
use identity::sas::{SasCode, SasContext, SasContribution};
use ring::hmac;

fn exporter(channel: &[u8], context: &[u8]) -> [u8; 4] {
    let key = hmac::Key::new(hmac::HMAC_SHA256, channel);
    let tag = hmac::sign(&key, context);
    tag.as_ref()[..4].try_into().expect("four bytes")
}

fn contribution(value: u8) -> SasContribution {
    SasContribution { attempt_id: [value; 16], nonce: [value; 32] }
}

fn context(channel: u8) -> SasContext {
    SasContext {
        opener: [1; 32], dialer: [2; 32], window_attempt: [3; 16], channel: [channel; 32],
    }
}

#[test]
fn preserved_legacy_model_allows_offline_context_grinding() {
    let target = SasCode::from_exporter_bytes(exporter(b"TLS channel A", &[0; 16]));
    let collision = (0u64..10_000_000).find(|candidate| {
        let mut attempt = [0; 16];
        attempt[..8].copy_from_slice(&candidate.to_be_bytes());
        SasCode::from_exporter_bytes(exporter(b"TLS channel B", &attempt)) == target
    });
    assert_eq!(collision, Some(146703), "preserve the old attack model");
}

#[test]
fn fixed_commitments_prevent_adaptive_two_channel_grinding() {
    let first = context(4);
    let second = context(5);
    let honest_opener = contribution(6);
    let honest_dialer = contribution(7);
    let attacker_opener = contribution(8);
    let attacker_dialer = contribution(9);
    // These must be sent before the corresponding honest nonces are revealed.
    let opener_commitment = second.commitment(true, &attacker_opener);
    let dialer_commitment = first.commitment(false, &attacker_dialer);
    let target = first.code(&honest_opener, &attacker_dialer);
    assert_ne!(target, second.code(&attacker_opener, &honest_dialer));
    assert!(first.verify(false, &dialer_commitment, &attacker_dialer));
    assert!(second.verify(true, &opener_commitment, &attacker_opener));
    // After learning both honest nonces the attacker can find another SAS, but
    // cannot open the commitment it already sent with that adaptive contribution.
    let collision = (0u64..10_000_000).find(|candidate| {
        let mut adaptive = contribution(8);
        adaptive.attempt_id[..8].copy_from_slice(&candidate.to_be_bytes());
        second.code(&adaptive, &honest_dialer) == target
    }).expect("the six-digit output itself is intentionally small");
    let mut adaptive = contribution(8);
    adaptive.attempt_id[..8].copy_from_slice(&collision.to_be_bytes());
    assert_eq!(second.code(&adaptive, &honest_dialer), target);
    assert!(!second.verify(true, &opener_commitment, &adaptive));
}

#[test]
fn commitments_bind_roles_identities_attempts_nonces_and_channel() {
    let original = context(4);
    let opening = contribution(5);
    let committed = original.commitment(true, &opening);
    assert!(original.verify(true, &committed, &opening));
    assert!(!original.verify(false, &committed, &opening));
    assert!(!original.verify(true, &committed, &contribution(6)));
    for field in 0..4 {
        let mut different = context(4);
        match field {
            0 => different.opener[0] ^= 1,
            1 => different.dialer[0] ^= 1,
            2 => different.window_attempt[0] ^= 1,
            _ => different.channel[0] ^= 1,
        }
        assert!(!different.verify(true, &committed, &opening));
    }
    let mut different = contribution(5);
    different.nonce[0] ^= 1;
    assert!(!original.verify(true, &committed, &different));
    let mut different = contribution(5);
    different.attempt_id[0] ^= 1;
    assert!(!original.verify(true, &committed, &different));
}
