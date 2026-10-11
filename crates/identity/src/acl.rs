//! Windows directory/file ACLs: owner-only, and refuse to start if wider.
//!
//! `docs/03-architecture.md`: «Каталоги создаются с правами только для пользователя: **ACL
//! на текущий SID в Windows**, 0700 в Linux. Core отказывается стартовать, если права
//! шире, и выдаёт понятную ошибку». This module is the Windows half of
//! [`crate::atomic::create_private_dir`] / [`crate::atomic::is_wider_than_owner`], which on
//! Unix use the mode bits.
//!
//! # What the DACL contains
//!
//! One `ACCESS_ALLOWED_ACE` for the **current user SID** with `FILE_ALL_ACCESS`, on a
//! *protected* DACL (`PROTECTED_DACL_SECURITY_INFORMATION`), which drops any inherited
//! ACE — that is the mirror of `0700`/`0600`. For directories the ACE carries
//! `OBJECT_INHERIT | CONTAINER_INHERIT`, so files and subdirectories created inside
//! inherit exactly the same owner-only access without a second call.
//!
//! **SYSTEM is deliberately not added.** It would not narrow anything (SYSTEM can take
//! ownership of any object and read it regardless of the DACL), while the docs ask for
//! «ACL на текущий SID»; adding principals nobody needs would only make the file look
//! more open than the documented rule. Administrators are excluded for the same reason a
//! `0600` file excludes root's group: the user owns the key and can always widen it.
//!
//! # The "wider" check
//!
//! [`is_owner_only`] reads the object's DACL and requires every *allow* ACE to name the
//! current user SID (deny and audit ACEs do not grant anything, so they are ignored). Any
//! allow ACE for someone else makes the object "wider", which is what makes the caller
//! refuse to start.

use std::ffi::c_void;
use std::path::Path;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, LocalFree, ERROR_SUCCESS, HANDLE, HLOCAL};
use windows::Win32::Security::Authorization::{
    GetNamedSecurityInfoW, SetNamedSecurityInfoW, SE_FILE_OBJECT,
};
use windows::Win32::Security::{
    AclSizeInformation, AddAccessAllowedAceEx, EqualSid, GetAce, GetAclInformation, GetLengthSid,
    GetTokenInformation, InitializeAcl, TokenUser, ACCESS_ALLOWED_ACE, ACE_HEADER, ACL,
    ACL_REVISION, ACL_SIZE_INFORMATION, CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION,
    OBJECT_INHERIT_ACE, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    TOKEN_QUERY, TOKEN_USER,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// `ACCESS_ALLOWED_ACE_TYPE` (`Win32::System::SystemServices`); spelled out so this crate
/// does not need that feature for one constant.
const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;

/// `FILE_ALL_ACCESS`: `STANDARD_RIGHTS_REQUIRED | SYNCHRONIZE | 0x1FF`.
const FILE_ALL_ACCESS: u32 = 0x001F_01FF;

/// Why an ACL operation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AclError {
    /// Windows returned this error code (`WIN32_ERROR` or `HRESULT` value).
    Win32(u32),
}

impl std::fmt::Display for AclError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let AclError::Win32(code) = self;
        write!(f, "windows acl error {code:#010x}")
    }
}

impl std::error::Error for AclError {}

fn win32(error: windows::Win32::Foundation::WIN32_ERROR) -> AclError {
    AclError::Win32(error.0)
}

/// An owned copy of a SID, so it outlives the token information buffer it came from.
struct OwnedSid(Vec<u8>);

impl OwnedSid {
    const fn psid(&self) -> PSID {
        PSID(self.0.as_ptr().cast_mut().cast())
    }

    fn len(&self) -> u32 {
        self.0.len() as u32
    }
}

/// The SID of the user this process runs as.
fn current_user_sid() -> Result<OwnedSid, AclError> {
    let mut token = HANDLE::default();
    // SAFETY: the pseudo-handle from `GetCurrentProcess` is always valid for the process
    // token query, and `token` receives an owned handle closed below.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }
        .map_err(|error| AclError::Win32(error.code().0 as u32))?;

    let mut length = 0u32;
    // First call asks for the required buffer size; `ERROR_INSUFFICIENT_BUFFER` is the
    // expected answer and is deliberately ignored.
    let _ = unsafe { GetTokenInformation(token, TokenUser, None, 0, &mut length) };
    if length == 0 {
        let _ = unsafe { CloseHandle(token) };
        return Err(AclError::Win32(0));
    }
    let mut buffer = vec![0u8; length as usize];
    // SAFETY: `buffer` is exactly the size the first call asked for, and `length` is
    // rewritten with the amount actually written.
    unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            length,
            &mut length,
        )
    }
    .map_err(|error| AclError::Win32(error.code().0 as u32))?;
    let _ = unsafe { CloseHandle(token) };

    // SAFETY: `buffer` holds a `TOKEN_USER` (the class we asked for) whose `User.Sid`
    // points inside the same buffer, and the SID is copied out before `buffer` drops.
    let sid = unsafe { (*(buffer.as_ptr() as *const TOKEN_USER)).User.Sid };
    let sid_length = unsafe { GetLengthSid(sid) } as usize;
    if sid_length == 0 {
        return Err(AclError::Win32(0));
    }
    // SAFETY: `sid` points at `sid_length` readable bytes inside `buffer`.
    let bytes = unsafe { std::slice::from_raw_parts(sid.0 as *const u8, sid_length) }.to_vec();
    Ok(OwnedSid(bytes))
}

/// Replaces `path`'s DACL with a protected, owner-only one.
///
/// Applied to both the `lan-workspace` directory (with inheritance flags) and the key file
/// itself, so neither is readable by another account even if the parent directory was
/// permissive.
pub fn apply_owner_only(path: &Path) -> Result<(), AclError> {
    let sid = current_user_sid()?;
    // `ACL` + one ACE header/sid pair, minus the `SidStart` word that the ACE already
    // counts as part of its size.
    let acl_size = size_of::<ACL>() as u32
        + (size_of::<ACCESS_ALLOWED_ACE>() as u32 - size_of::<u32>() as u32)
        + sid.len();
    let mut acl_buffer = vec![0u8; acl_size as usize];
    let acl = acl_buffer.as_mut_ptr() as *mut ACL;
    // SAFETY: the buffer is `acl_size` bytes and is not moved while `acl` points into it.
    unsafe { InitializeAcl(acl, acl_size, ACL_REVISION) }
        .map_err(|error| AclError::Win32(error.code().0 as u32))?;
    // SAFETY: `acl` was just initialised with room for exactly this ACE, and `sid` is a
    // valid SID.
    unsafe {
        AddAccessAllowedAceEx(
            acl,
            ACL_REVISION,
            OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE,
            FILE_ALL_ACCESS,
            sid.psid(),
        )
    }
    .map_err(|error| AclError::Win32(error.code().0 as u32))?;

    let wide = wide_path(path);
    // SAFETY: `wide` is NUL-terminated and outlives the call; owner/group/SACL are left
    // untouched (`None`), so only the DACL changes, and a protected DACL drops inherited
    // ACEs.
    let result = unsafe {
        SetNamedSecurityInfoW(
            PCWSTR(wide.as_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(acl),
            None,
        )
    };
    if result != ERROR_SUCCESS {
        return Err(win32(result));
    }
    Ok(())
}

/// Whether every *allow* ACE on `path` names the current user.
///
/// `false` means the object is readable by someone else, which is the Windows mirror of
/// "the mode has group/other bits set".
pub fn is_owner_only(path: &Path) -> Result<bool, AclError> {
    let sid = current_user_sid()?;
    let wide = wide_path(path);
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // SAFETY: `wide` is NUL-terminated, `dacl` and `descriptor` receive borrowed pointers
    // into an `LocalAlloc` descriptor that is released below.
    let result = unsafe {
        GetNamedSecurityInfoW(
            PCWSTR(wide.as_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(&mut dacl),
            None,
            &mut descriptor,
        )
    };
    if result != ERROR_SUCCESS {
        return Err(win32(result));
    }
    if dacl.is_null() {
        // A null DACL grants everyone full access. Treat as wider than owner-only.
        // SAFETY: `descriptor` came from `GetNamedSecurityInfoW`.
        unsafe {
            LocalFree(Some(HLOCAL(descriptor.0)));
        }
        return Ok(false);
    }

    let owner_only = (|| -> Result<bool, AclError> {
        let mut information = ACL_SIZE_INFORMATION::default();
        // SAFETY: `dacl` is valid; `information` is the exact struct the class expects.
        unsafe {
            GetAclInformation(
                dacl,
                (&mut information as *mut ACL_SIZE_INFORMATION).cast::<c_void>(),
                size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            )
        }
        .map_err(|error| AclError::Win32(error.code().0 as u32))?;

        for index in 0..information.AceCount {
            let mut ace: *mut c_void = std::ptr::null_mut();
            // SAFETY: `index` is below the ACE count and `ace` receives a pointer inside
            // the DACL.
            unsafe { GetAce(dacl, index, &mut ace) }
                .map_err(|error| AclError::Win32(error.code().0 as u32))?;
            // SAFETY: every ACE starts with an `ACE_HEADER`.
            let header = unsafe { &*(ace as *const ACE_HEADER) };
            if header.AceType != ACCESS_ALLOWED_ACE_TYPE {
                // Deny/audit ACEs grant nothing, so they cannot widen access.
                continue;
            }
            // SAFETY: an `ACCESS_ALLOWED_ACE` stores its SID in the trailing `SidStart`
            // field; taking its address is the documented way to obtain that SID.
            let ace_sid = unsafe {
                PSID(
                    (&(*(ace as *const ACCESS_ALLOWED_ACE)).SidStart as *const u32)
                        .cast_mut()
                        .cast(),
                )
            };
            // SAFETY: both SIDs are valid; `EqualSid` only compares them.
            if unsafe { EqualSid(ace_sid, sid.psid()) }.is_err() {
                return Ok(false);
            }
        }
        Ok(true)
    })();

    // SAFETY: the descriptor was allocated by `GetNamedSecurityInfoW`.
    unsafe {
        LocalFree(Some(HLOCAL(descriptor.0)));
    }
    owner_only
}

fn wide_path(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    wide
}
