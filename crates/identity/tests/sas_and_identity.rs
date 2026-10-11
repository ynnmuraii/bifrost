//! The comparison code and the `node_id` derivation: the two pure identity functions.

use identity::spki::{node_id_from_spki_der, spki_der};
use identity::{
    fingerprint, fingerprint_grouped, node_id_from_certificate, random16, NodeIdentity, SasCode,
    SpkiError, EXPORTER_BYTES, EXPORTER_LABEL, SAS_DIGITS, SAS_MODULUS,
};

#[test]
fn the_sas_constants_are_the_documented_ones() {
    // docs/05-protocol.md: «6-значный код сравнения
    // TLS-Exporter("EXPORTER-lan-workspace-pair", attempt_id, 4 байта) mod 10⁶».
    assert_eq!(EXPORTER_LABEL, b"EXPORTER-lan-workspace-pair");
    assert_eq!(EXPORTER_BYTES, 4);
    assert_eq!(SAS_DIGITS, 6);
    assert_eq!(SAS_MODULUS, 1_000_000);
}

#[test]
fn the_sas_is_the_big_endian_value_modulo_a_million() {
    // The byte order is the one resolved ambiguity; big-endian is what `from_be_bytes`
    // means, so `[0, 0, 1, 0]` is 256 and not 65536.
    assert_eq!(SasCode::from_exporter_bytes([0, 0, 1, 0]).value(), 256);
    assert_eq!(SasCode::from_exporter_bytes([0, 1, 0, 0]).value(), 65_536);
    assert_eq!(
        SasCode::from_exporter_bytes([1, 0, 0, 0]).value(),
        16_777_216 % 1_000_000
    );
}

#[test]
fn the_modulus_wraps_at_exactly_one_million() {
    // 999_999 is the largest code; 1_000_000 wraps to 0. This is the boundary the
    // «mod 10⁶» wording creates.
    let last = SasCode::from_exporter_bytes([0x00, 0x0f, 0x42, 0x3f]); // 999_999
    assert_eq!(last.value(), 999_999);
    assert_eq!(last.text(), "999999");

    let wrap = SasCode::from_exporter_bytes([0x00, 0x0f, 0x42, 0x40]); // 1_000_000
    assert_eq!(wrap.value(), 0);
    assert_eq!(wrap.text(), "000000");

    // A full-width value still reduces into six digits.
    let saturated = SasCode::from_exporter_bytes([0xff, 0xff, 0xff, 0xff]);
    assert_eq!(saturated.value(), 4_294_967_295 % 1_000_000);
    assert_eq!(saturated.text().len(), 6);
}

#[test]
fn codes_are_zero_padded_to_six_digits() {
    let small = SasCode::from_exporter_bytes([0, 0, 0, 42]);
    assert_eq!(small.value(), 42);
    assert_eq!(small.text(), "000042");
    assert_eq!(small.digits(), *b"000042");
    assert_eq!(small.to_string(), "000042", "Display must match text()");

    let zero = SasCode::from_exporter_bytes([0, 0, 0, 0]);
    assert_eq!(zero.text(), "000000");
    assert_eq!(zero.digits(), *b"000000");
}

#[test]
fn every_code_is_six_ascii_digits_and_agrees_with_its_value() {
    for raw in 0u32..4_000 {
        let bytes = raw.to_be_bytes();
        let code = SasCode::from_exporter_bytes(bytes);
        let text = code.text();

        assert_eq!(text.len(), SAS_DIGITS);
        assert!(text.bytes().all(|byte| byte.is_ascii_digit()), "{text}");
        assert_eq!(
            text,
            String::from_utf8(code.digits().to_vec()).expect("ascii")
        );
        assert_eq!(text.parse::<u32>().expect("numeric"), code.value());
        assert!(code.value() < SAS_MODULUS);
        // Deterministic: the same exporter bytes always give the same code.
        assert_eq!(SasCode::from_exporter_bytes(bytes), code);
    }
}

#[test]
fn node_id_is_the_hash_of_the_certificate_spki() {
    let identity = NodeIdentity::generate().expect("generate");
    let certificate = identity.certificate();

    let spki = spki_der(certificate.as_ref()).expect("the certificate has an SPKI");
    assert_eq!(spki.first(), Some(&0x30), "SPKI DER is a SEQUENCE");
    assert_eq!(node_id_from_spki_der(&spki), identity.node_id());
    assert_eq!(
        node_id_from_certificate(certificate.as_ref()).expect("derives"),
        identity.node_id()
    );

    // The identity survives a PKCS#8 round trip, which is what makes the pin stable
    // across restarts even though the certificate is regenerated every start.
    let reloaded = NodeIdentity::from_pkcs8(&identity.pkcs8_der()).expect("reload from pkcs8");
    assert_eq!(reloaded.node_id(), identity.node_id());
    assert_eq!(reloaded.pkcs8_der(), identity.pkcs8_der());
}

#[test]
fn two_generated_identities_are_different() {
    let first = NodeIdentity::generate().expect("generate");
    let second = NodeIdentity::generate().expect("generate");

    assert_ne!(first.node_id(), second.node_id());
    assert_ne!(first.pkcs8_der(), second.pkcs8_der());
    assert_ne!(first.certificate().as_ref(), second.certificate().as_ref());
}

#[test]
fn fingerprints_are_lower_case_hex_and_the_grouped_form_stays_equivalent() {
    let identity = NodeIdentity::generate().expect("generate");
    let plain = identity.fingerprint();
    assert_eq!(plain, fingerprint(&identity.node_id()));
    assert_eq!(plain.len(), 64);
    assert!(plain
        .bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)));
    assert_eq!(
        plain,
        identity
            .node_id()
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );

    let grouped = identity.fingerprint_grouped();
    assert_eq!(grouped, fingerprint_grouped(&identity.node_id()));
    assert_eq!(grouped.replace(':', ""), plain);
    assert_eq!(grouped.split(':').count(), 16);
    assert!(grouped.split(':').all(|group| group.len() == 4));
}

#[test]
fn malformed_certificates_are_refused() {
    assert_eq!(spki_der(b""), Err(SpkiError::MalformedCertificate));
    assert_eq!(
        spki_der(b"this is not a certificate"),
        Err(SpkiError::MalformedCertificate)
    );
    assert_eq!(
        node_id_from_certificate(b"\x30\x00"),
        Err(SpkiError::MalformedCertificate)
    );
}

#[test]
fn random16_is_sixteen_bytes_from_the_os_entropy_source() {
    let first = random16().expect("entropy");
    let second = random16().expect("entropy");
    assert_eq!(first.len(), 16);
    assert_eq!(second.len(), 16);
    assert_ne!(first, second);
}
