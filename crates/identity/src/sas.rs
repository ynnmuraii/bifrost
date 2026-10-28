//! Six-digit comparison codes. Version 1 uses SHA-256 commit/reveal over two fresh
//! contributions, both certificate identities and a fixed-context TLS exporter.
//! The legacy exporter-only derivation remains solely for explicitly legacy peers;
//! it does not provide version 1's resistance to offline context grinding.

/// The exporter label. Quoted from `docs/05-protocol.md`: `"EXPORTER-lan-workspace-pair"`.
pub const EXPORTER_LABEL: &[u8] = b"EXPORTER-lan-workspace-pair";

/// How many exporter bytes feed the code: «4 байта».
pub const EXPORTER_BYTES: usize = 4;

/// Modulus for the code: «mod 10⁶», i.e. exactly six digits including leading zeros.
pub const SAS_MODULUS: u32 = 1_000_000;

/// Number of digits the user compares.
pub const SAS_DIGITS: usize = 6;

/// A 6-digit comparison code, always printable with leading zeros.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SasCode(u32);

impl SasCode {
    /// Builds a code from the four exporter bytes.
    ///
    /// **Resolved ambiguity**: the docs say "4 байта" but not the byte order. This uses
    /// big-endian (network order), the canonical reading of a 4-byte exporter value; the
    /// choice is invisible to interoperability because both peers run this same function
    /// on the exporter output of the same session.
    pub const fn from_exporter_bytes(bytes: [u8; EXPORTER_BYTES]) -> Self {
        Self(u32::from_be_bytes(bytes) % SAS_MODULUS)
    }

    /// The numeric value (`0..=999_999`).
    pub const fn value(self) -> u32 {
        self.0
    }

    /// `"000000"`-style text the user compares.
    pub fn text(self) -> String {
        format!("{:0width$}", self.0, width = SAS_DIGITS)
    }

    /// The text as a fixed-size array, for wire/display paths that avoid allocation.
    pub fn digits(self) -> [u8; SAS_DIGITS] {
        let mut out = [b'0'; SAS_DIGITS];
        let mut value = self.0;
        for slot in out.iter_mut().rev() {
            *slot = b'0' + (value % 10) as u8;
            value /= 10;
        }
        out
    }
}

impl std::fmt::Display for SasCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let digits = self.digits();
        for digit in digits {
            f.write_str(std::str::from_utf8(&[digit]).unwrap_or("?"))?;
        }
        Ok(())
    }
}

/// Explicitly negotiated commitment ceremony version (zero means legacy).
pub const COMMITMENT_VERSION: u32 = 1;
/// Domain-separated fixed-context channel binding for the commitment ceremony.
pub const COMMITMENT_EXPORTER_LABEL: &[u8] = b"EXPORTER-bifrost-sas-commit-v1";

/// A fresh contribution kept private until both commitments have been received.
/// Deliberately not `Debug`: neither nonce nor transcript belongs in diagnostics.
pub struct SasContribution {
    /// Independent per-connection attempt identifier, not the window identifier.
    pub attempt_id: [u8; 16],
    /// 256 unpredictable bits, revealed only after commitments are fixed.
    pub nonce: [u8; 32],
}

impl SasContribution {
    /// Generates a fresh contribution using the OS entropy source.
    pub fn fresh() -> Result<Self, crate::key::IdentityError> {
        let mut nonce = [0; 32];
        nonce[..16].copy_from_slice(&crate::key::random16()?);
        nonce[16..].copy_from_slice(&crate::key::random16()?);
        Ok(Self {
            attempt_id: crate::key::random16()?,
            nonce,
        })
    }
}

/// Fixed-width, role-ordered transcript shared by both ends of one TLS channel.
/// Identities are certificate hashes, never names or unverified Hello fields.
pub struct SasContext {
    /// TLS server / pairing-window opener.
    pub opener: [u8; 32],
    /// TLS client / dialer.
    pub dialer: [u8; 32],
    /// The window identifier carried in every envelope.
    pub window_attempt: [u8; 16],
    /// 32-byte TLS 1.3 exporter with the versioned fixed label and empty context.
    pub channel: [u8; 32],
}

impl SasContext {
    fn hash(&self, domain: &[u8]) -> ring::digest::Context {
        let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
        hash.update(domain);
        hash.update(&self.opener);
        hash.update(&self.dialer);
        hash.update(&self.window_attempt);
        hash.update(&self.channel);
        hash
    }

    /// Commits to a contribution, its role, both identities and this exact channel.
    pub fn commitment(&self, opener: bool, contribution: &SasContribution) -> [u8; 32] {
        let mut hash = self.hash(b"bifrost-sas-commit-v1\0");
        hash.update(&[u8::from(opener)]);
        hash.update(&contribution.attempt_id);
        hash.update(&contribution.nonce);
        hash.finish().as_ref().try_into().expect("SHA-256 size")
    }

    /// Verifies a public reveal against the previously received commitment.
    pub fn verify(
        &self,
        opener: bool,
        commitment: &[u8; 32],
        contribution: &SasContribution,
    ) -> bool {
        self.commitment(opener, contribution) == *commitment
    }

    /// Derives the comparison code only after both reveals were verified.
    pub fn code(&self, opener: &SasContribution, dialer: &SasContribution) -> SasCode {
        let mut hash = self.hash(b"bifrost-sas-code-v1\0");
        hash.update(&opener.attempt_id);
        hash.update(&opener.nonce);
        hash.update(&dialer.attempt_id);
        hash.update(&dialer.nonce);
        let digest = hash.finish();
        SasCode::from_exporter_bytes(digest.as_ref()[..4].try_into().expect("four bytes"))
    }
}
