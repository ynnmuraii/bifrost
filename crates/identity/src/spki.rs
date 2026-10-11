//! `node_id` derivation: `SHA-256` of the certificate's `SubjectPublicKeyInfo` DER.
//!
//! `docs/03-architecture.md`, ERD `NODE_IDENTITY`: «bytes32 node_id PK "SHA256 of SPKI
//! DER"», and `docs/05-protocol.md`, «Сопряжение»: «Peer `node_id` — производный
//! SHA-256 SPKI, bytes32». The identity of a node is therefore its public key, and
//! never a name or an address: «Название устройства/адрес не являются идентичностью».
//!
//! The SPKI is extracted from the *presented* certificate, so a peer cannot claim a
//! `node_id` that does not belong to the key it just proved possession of
//! (`docs/05-protocol.md`: «Обе стороны проверяют, что `PairHello.node_id` совпадает с
//! SHA-256 реально предъявленного SPKI»).

use geometry::NodeId;
use ring::digest::{digest, SHA256};
use x509_cert::der::{Decode, Encode};

/// Why a certificate could not be turned into a node identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpkiError {
    /// The DER does not parse as an X.509 certificate.
    MalformedCertificate,
    /// The `SubjectPublicKeyInfo` could not be re-encoded.
    MalformedSpki,
}

impl std::fmt::Display for SpkiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpkiError::MalformedCertificate => f.write_str("certificate is not valid DER X.509"),
            SpkiError::MalformedSpki => f.write_str("certificate SPKI could not be re-encoded"),
        }
    }
}

impl std::error::Error for SpkiError {}

/// The DER bytes of the certificate's `SubjectPublicKeyInfo`.
///
/// Re-encoded from the parsed structure: DER is canonical, so for a well-formed
/// certificate this is byte-identical to the original field. A non-canonical encoding
/// can only make the re-encoding differ, which makes a pin comparison fail closed.
pub fn spki_der(certificate_der: &[u8]) -> Result<Vec<u8>, SpkiError> {
    let certificate = x509_cert::Certificate::from_der(certificate_der)
        .map_err(|_| SpkiError::MalformedCertificate)?;
    certificate
        .tbs_certificate()
        .subject_public_key_info()
        .to_der()
        .map_err(|_| SpkiError::MalformedSpki)
}

/// `SHA-256(SPKI DER)` — the `node_id` of the key that signed `certificate_der`.
pub fn node_id_from_certificate(certificate_der: &[u8]) -> Result<NodeId, SpkiError> {
    let spki = spki_der(certificate_der)?;
    Ok(node_id_from_spki_der(&spki))
}

/// `SHA-256` over SPKI DER bytes.
pub fn node_id_from_spki_der(spki_der: &[u8]) -> NodeId {
    let digest = digest(&SHA256, spki_der);
    let mut bytes = [0u8; 32];
    bytes.copy_from_slice(digest.as_ref());
    NodeId::new(bytes)
}

/// Lower-case hex of the 32 identity bytes, the "fingerprint" shown in the GUI
/// («Полный отпечаток доступен в „Подробнее“»).
pub fn fingerprint(node_id: &NodeId) -> String {
    let mut out = String::with_capacity(64);
    for byte in node_id.as_bytes() {
        out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('?'));
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('?'));
    }
    out
}

/// The same fingerprint grouped in pairs of bytes, for the "Подробнее" view.
pub fn fingerprint_grouped(node_id: &NodeId) -> String {
    node_id
        .as_bytes()
        .chunks(2)
        .map(|pair| {
            let mut group = String::with_capacity(4);
            for byte in pair {
                group.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('?'));
                group.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('?'));
            }
            group
        })
        .collect::<Vec<_>>()
        .join(":")
}
