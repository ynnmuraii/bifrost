//! How the private key is protected **at rest**.
//!
//! `docs/03-architecture.md`, «Файлы и пути»:
//!
//! | | path |
//! |---|---|
//! | Windows | `%APPDATA%\lan-workspace\identity.dpapi` (DPAPI CurrentUser) |
//! | Linux | Secret Service; otherwise `$XDG_DATA_HOME/lan-workspace/identity.key` (0600) |
//!
//! * **Windows** seals the PKCS#8 key with DPAPI (`CryptProtectData`) under the *current
//!   user*: the blob can only be unsealed by the same user profile on the same machine
//!   (or by a domain profile with the user's credentials), so a copied file is useless.
//!   `CRYPTPROTECT_UI_FORBIDDEN` is always set: a headless agent must never pop a UI, it
//!   must fail.
//! * **Linux** stores the PKCS#8 bytes as-is, because the documented protection there is
//!   the file mode (`0600`) plus the `0700` directory —
//!   [`crate::atomic::create_private_dir`] and [`crate::atomic::is_wider_than_owner`]
//!   refuse anything wider. The Secret Service path is not implemented (documented gap).
//!
//! # Entropy
//!
//! DPAPI takes optional "entropy" that is mixed into the key derivation, so a blob can
//! only be unsealed by a caller that presents the same bytes. It is **not** a secret (it
//! is a compile-time constant in this crate, and the DPAPI master key still comes from
//! the user profile); its value is that it binds the blob to this application, so another
//! program running as the same user cannot casually unseal our key by calling
//! `CryptUnprotectData` on the file it found. [`DPAPI_ENTROPY`] is that constant, and it
//! must never change for as long as keys exist in the field — changing it makes every
//! existing `identity.dpapi` unreadable.

/// Application-specific DPAPI entropy. **Never change this value**: it is part of the
/// on-disk format of `identity.dpapi`.
pub const DPAPI_ENTROPY: &[u8] = b"lan-workspace/identity/v1";

/// Why sealing or unsealing failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeystoreError {
    /// The platform call failed with this OS error code.
    ///
    /// On Windows this is a `Win32`/`HRESULT` value: `0x8007000D`-style for "wrong user or
    /// wrong entropy", `0x80070057` for a malformed blob.
    Os(u32),
    /// The platform returned an empty blob, which can never be a valid key.
    Empty,
}

impl std::fmt::Display for KeystoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeystoreError::Os(code) => {
                write!(f, "key protection failed with OS error {code:#010x}")
            }
            KeystoreError::Empty => f.write_str("key protection returned an empty blob"),
        }
    }
}

impl std::error::Error for KeystoreError {}

/// Protects `plaintext` for storage.
#[cfg(windows)]
pub fn seal(plaintext: &[u8]) -> Result<Vec<u8>, KeystoreError> {
    use windows::core::PCWSTR;
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    let input = blob(plaintext)?;
    let entropy = blob(DPAPI_ENTROPY)?;
    let mut output = CRYPT_INTEGER_BLOB::default();
    // SAFETY: both input blobs point at live slices that outlive the call, no description
    // string and no prompt structure are passed (so nothing else is allocated or shown),
    // and DPAPI writes an owned `LocalAlloc` blob into `output`, which `take_output`
    // copies and then releases with `LocalFree`.
    unsafe {
        CryptProtectData(
            &input,
            PCWSTR::null(),
            Some(&entropy),
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    }
    .map_err(|error| KeystoreError::Os(error.code().0 as u32))?;
    take_output(&output)
}

/// Unprotects a blob produced by [`seal`].
#[cfg(windows)]
pub fn unseal(ciphertext: &[u8]) -> Result<Vec<u8>, KeystoreError> {
    use windows::Win32::Security::Cryptography::{
        CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    let input = blob(ciphertext)?;
    let entropy = blob(DPAPI_ENTROPY)?;
    let mut output = CRYPT_INTEGER_BLOB::default();
    // SAFETY: as in `seal`; `ppszdatadescr` is `None`, so DPAPI allocates only `output`,
    // and `take_output` releases it with `LocalFree`.
    unsafe {
        CryptUnprotectData(
            &input,
            None,
            Some(&entropy),
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    }
    .map_err(|error| KeystoreError::Os(error.code().0 as u32))?;
    take_output(&output)
}

#[cfg(windows)]
fn blob(
    bytes: &[u8],
) -> Result<windows::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB, KeystoreError> {
    let length = u32::try_from(bytes.len()).map_err(|_| KeystoreError::Empty)?;
    Ok(windows::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB {
        cbData: length,
        // `pbData` is `*mut u8` in the bindings, but DPAPI only reads the input blob.
        pbData: bytes.as_ptr().cast_mut(),
    })
}

/// Copies the DPAPI-owned blob and releases it with `LocalFree`.
#[cfg(windows)]
fn take_output(
    output: &windows::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB,
) -> Result<Vec<u8>, KeystoreError> {
    use windows::Win32::Foundation::{LocalFree, HLOCAL};

    if output.pbData.is_null() || output.cbData == 0 {
        return Err(KeystoreError::Empty);
    }
    // SAFETY: DPAPI guarantees `cbData` readable bytes at `pbData`; the slice is copied
    // before the buffer is released, so nothing points into freed memory afterwards.
    let bytes =
        unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) }.to_vec();
    // SAFETY: the buffer came from `LocalAlloc` inside DPAPI, and `LocalFree` is its
    // documented release; the returned handle is a status we do not need.
    unsafe {
        LocalFree(Some(HLOCAL(output.pbData.cast())));
    }
    Ok(bytes)
}

/// On Linux the documented protection is the file mode (`0600`) and the directory
/// (`0700`), both enforced by [`crate::atomic`], so the bytes are stored as they are.
#[cfg(unix)]
pub fn seal(plaintext: &[u8]) -> Result<Vec<u8>, KeystoreError> {
    Ok(plaintext.to_vec())
}

/// Mirror of [`seal`] for Linux (see its docs).
#[cfg(unix)]
pub fn unseal(ciphertext: &[u8]) -> Result<Vec<u8>, KeystoreError> {
    Ok(ciphertext.to_vec())
}
