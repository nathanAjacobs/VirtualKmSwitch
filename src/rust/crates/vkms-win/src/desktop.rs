//! Which desktop currently owns input.
//!
//! Desktops are isolation boundaries: `SendInput` reaches only the desktop its calling *process* was
//! created on (measured -- `SetThreadDesktop` succeeds and injection is then still denied). A UAC
//! prompt runs on the separate "Winlogon" desktop, so each agent needs to know whether its own
//! desktop is the one showing.

use windows_sys::Win32::System::StationsAndDesktops::{
    CloseDesktop, GetUserObjectInformationW, OpenInputDesktop, DESKTOP_READOBJECTS, UOI_NAME,
};

/// The name of the desktop receiving input, e.g. "Default" or "Winlogon", without attaching to it.
///
/// Returns "?" when it cannot be read, which happens transiently while the system switches
/// desktops, and permanently for a caller that lacks access to the secure desktop.
pub fn input_desktop_name() -> String {
    unsafe {
        let desktop = OpenInputDesktop(0, 0, DESKTOP_READOBJECTS);
        if desktop.is_null() {
            return "?".into();
        }

        let mut buffer = [0u16; 128];
        let mut needed = 0u32;
        let ok = GetUserObjectInformationW(
            desktop,
            UOI_NAME,
            buffer.as_mut_ptr().cast(),
            (buffer.len() * 2) as u32,
            &mut needed,
        );

        CloseDesktop(desktop);

        if ok == 0 {
            return "?".into();
        }

        let length = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
        String::from_utf16_lossy(&buffer[..length])
    }
}
