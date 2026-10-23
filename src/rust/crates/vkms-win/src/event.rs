//! A named auto-reset event, the wakeup half of a shared-memory ring.

use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr::null;
use std::time::Duration;

use windows_sys::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::System::Threading::WaitForMultipleObjects;
use windows_sys::Win32::System::Threading::{
    CreateEventW, OpenEventW, SetEvent, WaitForSingleObject, EVENT_MODIFY_STATE, SYNCHRONIZATION_SYNCHRONIZE,
};

use crate::security::SecurityAttributes;
use crate::{wide, Win32Error};

pub struct NamedEvent {
    handle: OwnedHandle,
}

impl NamedEvent {
    /// Creates an auto-reset event, initially unsignalled.
    pub fn create(name: &str, sddl: Option<&str>) -> Result<Self, Win32Error> {
        let attributes = sddl.map(SecurityAttributes::from_sddl).transpose()?;
        let wide_name = wide(name);

        let handle = unsafe {
            CreateEventW(
                attributes.as_ref().map_or(null(), SecurityAttributes::as_ptr),
                0,
                0,
                wide_name.as_ptr(),
            )
        };

        if handle.is_null() {
            return Err(Win32Error::last(format!("CreateEvent failed for '{name}'")));
        }

        Ok(Self { handle: unsafe { OwnedHandle::from_raw_handle(handle as _) } })
    }

    pub fn open(name: &str) -> Result<Self, Win32Error> {
        let wide_name = wide(name);
        let handle = unsafe { OpenEventW(EVENT_MODIFY_STATE | SYNCHRONIZATION_SYNCHRONIZE, 0, wide_name.as_ptr()) };

        if handle.is_null() {
            return Err(Win32Error::last(format!("OpenEvent failed for '{name}'")));
        }

        Ok(Self { handle: unsafe { OwnedHandle::from_raw_handle(handle as _) } })
    }

    pub fn set(&self) {
        unsafe { SetEvent(self.raw()) };
    }

    /// Blocks until signalled or the timeout expires. Returns whether it was signalled.
    pub fn wait(&self, timeout: Duration) -> bool {
        let millis = timeout.as_millis().min(u32::MAX as u128 - 1) as u32;
        unsafe { WaitForSingleObject(self.raw(), millis) == WAIT_OBJECT_0 }
    }

    /// Blocks until any of `events` is signalled or the timeout expires. Returns the index of the
    /// one that was, consuming its signal. At most 64 events, which is the Win32 limit.
    pub fn wait_any(events: &[&NamedEvent], timeout: Duration) -> Option<usize> {
        if events.is_empty() {
            std::thread::sleep(timeout);
            return None;
        }

        let handles: Vec<HANDLE> = events.iter().take(64).map(|event| event.raw()).collect();
        let millis = timeout.as_millis().min(u32::MAX as u128 - 1) as u32;
        let result = unsafe { WaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), 0, millis) };

        let index = result.wrapping_sub(WAIT_OBJECT_0) as usize;
        (index < handles.len()).then_some(index)
    }

    fn raw(&self) -> HANDLE {
        self.handle.as_raw_handle() as HANDLE
    }
}
