//! Independent SHA256/HMAC known answers; transport can reproduce without media dependencies.
use audio_model::{jitter::Profile, keys::*};
use ring::hkdf;
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
#[test]
fn fixed_width_context_and_auxiliary_hkdf_vectors() {
    let format = Format {
        codec: Codec::Opus,
        sample_rate: 48_000,
        channels: 2,
        frame_samples: 480,
        bitrate: 96_000,
        fec: false,
        aead: Aead::ChaCha20Poly1305,
        profile: Profile::Ethernet,
        max_datagram: 1200,
    };
    let hash = ring::digest::digest(&ring::digest::SHA256, &format.canonical_bytes());
    assert_eq!(
        hex(hash.as_ref()),
        "f1472551240d99e61ff3e46d946c21dbb179f4a846ba5c583add09136bb59aee"
    );
    let context = SessionContext {
        parent_connection_id: [1; 16],
        group_id: [2; 16],
        session_id: [3; 16],
        listener_nonce: [4; 32],
        source_nonce: [5; 32],
        nodes: [[7; 32], [6; 32]],
        kind: StreamKind::System,
        format_sha256: hash.as_ref().try_into().unwrap(),
    };
    let encoded = context.encode();
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, b"bifrost-aux-hkdf-v1").extract(&[11; 32]);
    for (label, expected) in [
        (
            FORWARD_KEY_LABEL,
            "fe0b5ee19055671520bec140b61ab475590faf447dfd408efdd12e1b5ed8c38d",
        ),
        (
            REVERSE_KEY_LABEL,
            "dec68355b4f0d5814e60d845ac72e5ac0c78ee7cde784f4f9330bbc9ec62f72c",
        ),
        (
            FORWARD_NONCE_LABEL,
            "32d95a00cb4e4fff9bd758800aaa4ec2d7b0958e13000b4f1e577d9a549b33bf",
        ),
        (
            REVERSE_NONCE_LABEL,
            "480e1cd1dae53fa33b512d589a61101523acb8acfb8c108e57f7ebbae03d5e5f",
        ),
    ] {
        let label_len = (label.len() as u64).to_be_bytes();
        let context_len = (encoded.len() as u64).to_be_bytes();
        let parts = [&label_len[..], label, &context_len[..], &encoded[..]];
        let mut key = [0; 32];
        prk.expand(&parts, hkdf::HKDF_SHA256)
            .unwrap()
            .fill(&mut key)
            .unwrap();
        assert_eq!(hex(&key), expected);
    }
}
