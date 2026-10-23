//! Security descriptors, token identity, and caller authorisation.

use std::ffi::c_void;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, BOOL, HANDLE};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SetNamedSecurityInfoW,
    SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    CheckTokenMembership, CreateWellKnownSid, GetSecurityDescriptorDacl, GetTokenInformation,
    IsWellKnownSid, RevertToSelf, TokenUser, WinBuiltinAdministratorsSid,
    WinLocalSystemSid, ACL, DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::System::Pipes::ImpersonateNamedPipeClient;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetCurrentThread, OpenProcessToken, OpenThreadToken};

use crate::{from_wide_ptr, wide, Win32Error};

/// SYSTEM alone, with inheritance blocked so no parent container can widen it.
pub const SYSTEM_ONLY_SDDL: &str = "D:P(A;;GA;;;SY)";

/// SYSTEM, Administrators, and the calling user, for running outside a service context.
///
/// The calling user must be included explicitly. Granting only SYSTEM and Administrators produces
/// an object its own creator cannot reopen, which fails at open time with a bare access-denied and
/// looks like a bug in whatever uses it rather than in the DACL.
pub fn current_user_sddl() -> Result<String, Win32Error> {
    Ok(format!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;{})", current_user_sid()?))
}

/// A `SECURITY_ATTRIBUTES` built from SDDL, owning its descriptor.
pub struct SecurityAttributes {
    descriptor: PSECURITY_DESCRIPTOR,
    attributes: SECURITY_ATTRIBUTES,
}

// The descriptor is an immutable heap block owned by this value.
unsafe impl Send for SecurityAttributes {}
unsafe impl Sync for SecurityAttributes {}

impl SecurityAttributes {
    pub fn from_sddl(sddl: &str) -> Result<Self, Win32Error> {
        let text = wide(sddl);
        let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();

        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                text.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                null_mut(),
            )
        };

        if ok == 0 {
            return Err(Win32Error::last(format!("could not parse security descriptor '{sddl}'")));
        }

        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };

        Ok(Self { descriptor, attributes })
    }

    pub fn as_ptr(&self) -> *const SECURITY_ATTRIBUTES {
        &self.attributes
    }

    /// For APIs that take the attributes as an untyped pointer.
    pub fn as_mut_void(&mut self) -> *mut c_void {
        (&mut self.attributes as *mut SECURITY_ATTRIBUTES).cast()
    }
}

impl Drop for SecurityAttributes {
    fn drop(&mut self) {
        unsafe { LocalFree(self.descriptor as _) };
    }
}

/// A token handle closed on drop.
struct Token(HANDLE);

impl Drop for Token {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

fn process_token() -> Result<Token, Win32Error> {
    let mut handle: HANDLE = null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut handle) } == 0 {
        return Err(Win32Error::last("OpenProcessToken failed"));
    }

    Ok(Token(handle))
}

/// Reads `TOKEN_USER` into an 8-byte-aligned buffer; the SID points inside it.
fn token_user(token: &Token) -> Result<Vec<u64>, Win32Error> {
    let mut needed = 0u32;
    unsafe { GetTokenInformation(token.0, TokenUser, null_mut(), 0, &mut needed) };

    let mut buffer = vec![0u64; (needed as usize).div_ceil(8).max(1)];
    let ok = unsafe {
        GetTokenInformation(token.0, TokenUser, buffer.as_mut_ptr().cast(), (buffer.len() * 8) as u32, &mut needed)
    };

    if ok == 0 {
        return Err(Win32Error::last("GetTokenInformation(TokenUser) failed"));
    }

    Ok(buffer)
}

fn user_sid_of(buffer: &[u64]) -> *mut c_void {
    unsafe { (*(buffer.as_ptr() as *const TOKEN_USER)).User.Sid }
}

/// The SID of the account this process runs as, in string form.
pub fn current_user_sid() -> Result<String, Win32Error> {
    let token = process_token()?;
    let buffer = token_user(&token)?;

    let mut text: *mut u16 = null_mut();
    if unsafe { ConvertSidToStringSidW(user_sid_of(&buffer), &mut text) } == 0 {
        return Err(Win32Error::last("ConvertSidToStringSid failed"));
    }

    let result = unsafe { from_wide_ptr(text) };
    unsafe { LocalFree(text as _) };
    Ok(result)
}

fn token_is_system(token: &Token) -> Result<bool, Win32Error> {
    let buffer = token_user(token)?;
    Ok(unsafe { IsWellKnownSid(user_sid_of(&buffer), WinLocalSystemSid) } != 0)
}

/// Whether this process runs as LocalSystem.
pub fn is_system() -> bool {
    process_token().and_then(|token| token_is_system(&token)).unwrap_or(false)
}

fn token_is_administrator(token: &Token) -> Result<bool, Win32Error> {
    let mut sid = [0u64; 12]; // SECURITY_MAX_SID_SIZE is 68 bytes
    let mut size = (sid.len() * 8) as u32;

    if unsafe { CreateWellKnownSid(WinBuiltinAdministratorsSid, null_mut(), sid.as_mut_ptr().cast(), &mut size) } == 0
    {
        return Err(Win32Error::last("CreateWellKnownSid(Administrators) failed"));
    }

    // CheckTokenMembership honours deny-only groups, so an administrator running unelevated under
    // UAC is correctly *not* treated as one.
    let mut member: BOOL = 0;
    if unsafe { CheckTokenMembership(token.0, sid.as_mut_ptr().cast(), &mut member) } == 0 {
        return Err(Win32Error::last("CheckTokenMembership failed"));
    }

    Ok(member != 0)
}

/// Whether the client on the far end of a named pipe is SYSTEM or an elevated administrator.
///
/// Decided by impersonating the connection and reading the token Windows attached to it, never from
/// anything the client sent: a client that claims to be an administrator is merely a client that
/// says so. The client needs to allow at least identification-level impersonation for this to work.
pub fn pipe_client_is_administrator(pipe: &impl AsRawHandle) -> Result<bool, Win32Error> {
    if unsafe { ImpersonateNamedPipeClient(pipe.as_raw_handle() as HANDLE) } == 0 {
        return Err(Win32Error::last("ImpersonateNamedPipeClient failed"));
    }

    let mut handle: HANDLE = null_mut();
    let opened = unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut handle) };
    let open_error = (opened == 0).then(|| Win32Error::last("OpenThreadToken failed"));

    // Reverted at once, before anything else runs on this thread. The token handle stays valid.
    if unsafe { RevertToSelf() } == 0 {
        // A thread left impersonating would carry on doing the service's work as the caller.
        // There is no safe way to continue from that.
        let error = Win32Error::last("RevertToSelf failed");
        log::error!("{error}; aborting rather than continuing under a client's identity");
        std::process::abort();
    }

    if let Some(error) = open_error {
        return Err(error);
    }

    let token = Token(handle);
    Ok(token_is_system(&token)? || token_is_administrator(&token)?)
}

/// Replaces a file or directory's DACL with the one described by `sddl`, blocking inheritance.
pub fn protect_path(path: &Path, sddl: &str) -> Result<(), Win32Error> {
    let attributes = SecurityAttributes::from_sddl(sddl)?;

    let mut present: BOOL = 0;
    let mut defaulted: BOOL = 0;
    let mut dacl: *mut ACL = null_mut();

    if unsafe { GetSecurityDescriptorDacl(attributes.descriptor, &mut present, &mut dacl, &mut defaulted) } == 0 {
        return Err(Win32Error::last("GetSecurityDescriptorDacl failed"));
    }

    let name = wide(&path.to_string_lossy());
    let status = unsafe {
        SetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            dacl,
            null(),
        )
    };

    if status != 0 {
        return Err(Win32Error::with_code(status, format!("could not secure '{}'", path.display())));
    }

    Ok(())
}
