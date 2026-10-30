use audio_model::{jitter::Profile, keys::*};
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
#[test]
fn fixed_width_context_known_answer() {
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
    assert_eq!(
        hex(&format.canonical_bytes()),
        "010000bb80000201e00001770000010104b0"
    );
    // SHA256 of the preimage above, computed independently of this dependency-free crate.
    let format_sha256 = [
        0xf1, 0x47, 0x25, 0x51, 0x24, 0x0d, 0x99, 0xe6, 0x1f, 0xf3, 0xe4, 0x6d, 0x94, 0x6c, 0x21,
        0xdb, 0xb1, 0x79, 0xf4, 0xa8, 0x46, 0xba, 0x5c, 0x58, 0x3a, 0xdd, 0x09, 0x13, 0x6b, 0xb5,
        0x9a, 0xee,
    ];
    let mut context = SessionContext {
        parent_connection_id: [1; 16],
        group_id: [2; 16],
        session_id: [3; 16],
        listener_nonce: [4; 32],
        source_nonce: [5; 32],
        nodes: [[7; 32], [6; 32]],
        kind: StreamKind::System,
        format_sha256,
    };
    let encoded = context.encode();
    assert_eq!(
        hex(&encoded),
        concat!(
            "626966726f73742d617564696f2d7631",
            "01010101010101010101010101010101",
            "02020202020202020202020202020202",
            "03030303030303030303030303030303",
            "0404040404040404040404040404040404040404040404040404040404040404",
            "0505050505050505050505050505050505050505050505050505050505050505",
            "0606060606060606060606060606060606060606060606060606060606060606",
            "0707070707070707070707070707070707070707070707070707070707070707",
            "01",
            "f1472551240d99e61ff3e46d946c21dbb179f4a846ba5c583add09136bb59aee"
        )
    );
    context.nodes.swap(0, 1);
    assert_eq!(encoded, context.encode());
    context.listener_nonce[0] ^= 1;
    assert_ne!(encoded, context.encode());
}
