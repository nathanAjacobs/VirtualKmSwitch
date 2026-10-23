//! Launches a process as SYSTEM inside the interactive console session.
//!
//! This exists because of session 0 isolation: services run in session 0, which is non-interactive
//! and has no access to the interactive session's window station. A service therefore cannot call
//! `SendInput` at all, however privileged it is -- it has to place a helper *inside* the session.
//!
//! The token stays SYSTEM rather than becoming the logged-in user: only SYSTEM can reach the
//! Winlogon desktop, which is what makes driving a UAC prompt possible. So this duplicates the
//! caller's own SYSTEM token and merely retargets its session.
//!
//! Requires `SeTcbPrivilege`, i.e. the caller must already be LocalSystem.

use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_TIMEOUT};
use windows_sys::Win32::Security::{
    DuplicateTokenEx, SecurityIdentification, SetTokenInformation, TokenPrimary, TokenSessionId,
    TOKEN_ALL_ACCESS, TOKEN_DUPLICATE, TOKEN_QUERY,
};
use windows_sys::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;
use windows_sys::Win32::System::Threading::{
    CreateProcessAsUserW, GetCurrentProcess, OpenProcessToken, TerminateProcess, WaitForSingleObject,
    CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, PROCESS_INFORMATION, STARTUPINFOW,
};

use crate::{wide, Win32Error};

/// No interactive session is attached to the console.
pub const NO_SESSION: u32 = 0xFFFF_FFFF;

/// The session currently attached to the physical console, or [`NO_SESSION`].
pub fn active_console_session() -> u32 {
    unsafe { WTSGetActiveConsoleSessionId() }
}

/// A launched process, held by handle so it can be watched and stopped without a pid race.
pub struct ChildProcess {
    handle: OwnedHandle,
    pid: u32,
}

impl ChildProcess {
    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn is_alive(&self) -> bool {
        unsafe { WaitForSingleObject(self.handle.as_raw_handle() as HANDLE, 0) == WAIT_TIMEOUT }
    }

    /// Terminates the process and waits briefly for it to go.
    pub fn kill(&self) {
        unsafe {
            let handle = self.handle.as_raw_handle() as HANDLE;
            TerminateProcess(handle, 1);
            WaitForSingleObject(handle, 2000);
        }
    }
}

/// Starts `executable` as SYSTEM in `session_id`, attached to `desktop` (e.g. `Winsta0\Winlogon`).
pub fn launch_as_system_in_session(
    executable: &str,
    arguments: &str,
    session_id: u32,
    desktop: &str,
) -> Result<ChildProcess, Win32Error> {
    if session_id == NO_SESSION {
        return Err(Win32Error::with_code(0, "no interactive console session is attached"));
    }

    unsafe {
        let mut own: HANDLE = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_DUPLICATE | TOKEN_QUERY, &mut own) == 0 {
            return Err(Win32Error::last("OpenProcessToken failed"));
        }
        let own = OwnedHandle::from_raw_handle(own as _);

        // A primary token is required to launch a process; the original is not one.
        let mut primary: HANDLE = null_mut();
        if DuplicateTokenEx(
            own.as_raw_handle() as HANDLE,
            TOKEN_ALL_ACCESS,
            null(),
            SecurityIdentification,
            TokenPrimary,
            &mut primary,
        ) == 0
        {
            return Err(Win32Error::last("DuplicateTokenEx failed"));
        }
        let primary = OwnedHandle::from_raw_handle(primary as _);

        // The single step that crosses the session 0 boundary.
        if SetTokenInformation(
            primary.as_raw_handle() as HANDLE,
            TokenSessionId,
            (&session_id as *const u32).cast(),
            std::mem::size_of::<u32>() as u32,
        ) == 0
        {
            return Err(Win32Error::last(
                "SetTokenInformation(TokenSessionId) failed -- SeTcbPrivilege is required, so this only \
                 works when running as LocalSystem",
            ));
        }

        // The command line must be writable: CreateProcessAsUser may modify it in place.
        let mut command_line = wide(&format!("\"{executable}\" {arguments}"));
        let mut desktop_name = wide(desktop);

        let startup = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            // Placed on the named desktop from the start: a process may only inject on the desktop
            // it was created on, so there is no migrating it afterwards.
            lpDesktop: desktop_name.as_mut_ptr(),
            ..std::mem::zeroed()
        };

        let mut info: PROCESS_INFORMATION = std::mem::zeroed();

        if CreateProcessAsUserW(
            primary.as_raw_handle() as HANDLE,
            null(),
            command_line.as_mut_ptr(),
            null(),
            null(),
            0,
            CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
            null(),
            null(),
            &startup,
            &mut info,
        ) == 0
        {
            return Err(Win32Error::last("CreateProcessAsUser failed"));
        }

        CloseHandle(info.hThread);

        Ok(ChildProcess { handle: OwnedHandle::from_raw_handle(info.hProcess as _), pid: info.dwProcessId })
    }
}
