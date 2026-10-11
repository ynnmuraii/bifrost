//! The 6-digit comparison code (SAS) derived from the TLS exporter.
//!
//! `docs/05-protocol.md`, «Сопряжение»: «Каждая сторона вычисляет 6-значный код
//! сравнения `TLS-Exporter("EXPORTER-lan-workspace-pair", attempt_id, 4 байта) mod 10⁶`;
//! пользователь сравнивает коды и подтверждает. ... MITM держит два разных TLS-сеанса,
//! поэтому коды не совпадут; вероятность подбора 10⁻⁶ на попытку при лимите 3 попытки
//! на 120 s». ADR-010 says the same thing and calls it a `numeric comparison` in the
//! spirit of Bluetooth SSP, not a PIN without PAKE.
//!
//! The exporter itself is RFC 8446 §7.5 (`export_keying_material`), so the code is a
//! function of the finished TLS session: the two peers can only agree on it when they
//! are in the *same* session. Nothing here is secret; the code is only useful while the
//! two users compare it out of band.

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
