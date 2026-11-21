//! Every native call Virtual KM Switch makes, behind safe wrappers.
//!
//! This is the only crate that touches raw handles, and the only one whose mistakes can crash the
//! process rather than return an error, so it is kept small and each `unsafe` block says what makes
//! it sound.
//!
//! Several of the traps the C# version had to code around simply do not exist here. Hook and window
//! procedures are plain `extern "system"` functions with static lifetime, so there is no delegate
//! for a garbage collector to move or collect. Handles are closed by `Drop` on the owning value
//! rather than by a finalizer on an arbitrary thread. What remains is thread affinity -- hooks,
//! windows and desktops all belong to a thread -- and that is handled by giving each its own.

#![cfg(windows)]

mod error;

pub mod cursor;
pub mod desktop;
pub mod dpapi;
pub mod event;
pub mod files;
pub mod focus_decoy;
pub mod hook;
pub mod injector;
pub mod logging;
pub mod message_window;
pub mod pointer;
pub mod raw_input;
pub mod security;
pub mod session;
pub mod shared_memory;
pub mod thread;

pub use error::Win32Error;

/// Tag stamped into `dwExtraInfo` on everything we inject, so our own hooks and raw input can tell
/// our synthetic events from physical ones. Without this the mesh forms an input feedback loop.
///
/// Raw input echoes this back in `RAWMOUSE.ulExtraInformation` / `RAWKEYBOARD.ExtraInformation`,
/// which is the *only* sound way to recognise our own injection there. A null `hDevice` looks like
/// it should work and does not: physical mice legitimately report `hDevice == 0` on some hardware,
/// so filtering on it silently discards real movement.
pub const INJECTED_SIGNATURE: u32 = 0x564B_4D53; // "VKMS"

/// A NUL-terminated UTF-16 copy of `text`, for passing to a `W` function.
pub(crate) fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Reads a NUL-terminated UTF-16 string.
///
/// # Safety
/// `ptr` must point to a NUL-terminated UTF-16 string.
pub(crate) unsafe fn from_wide_ptr(ptr: *const u16) -> String {
    if ptr.is_null() {
        return String::new();
    }

    let mut length = 0;
    while *ptr.add(length) != 0 {
        length += 1;
    }

    String::from_utf16_lossy(std::slice::from_raw_parts(ptr, length))
}
