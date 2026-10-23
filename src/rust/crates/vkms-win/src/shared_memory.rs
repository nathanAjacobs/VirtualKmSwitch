//! A named shared-memory region with an explicit security descriptor.
//!
//! The region carries the input stream between the service and a session agent, and anything able
//! to write to it can inject keystrokes and mouse movement into the machine. It is an injection
//! primitive, so it gets an explicit DACL rather than whatever the creating token defaults to.

use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr::null;

use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_READ, FILE_MAP_WRITE,
    MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
};

use crate::security::SecurityAttributes;
use crate::{wide, Win32Error};

pub struct SharedMemoryRegion {
    _mapping: OwnedHandle,
    view: *mut u8,
    len: usize,
    name: String,
}

// The view is plain memory; synchronising access to it is the user's responsibility, exactly as
// it is for the other process mapping the same pages.
unsafe impl Send for SharedMemoryRegion {}
unsafe impl Sync for SharedMemoryRegion {}

impl SharedMemoryRegion {
    /// Creates a region backed by the page file.
    ///
    /// Prefix `name` with `Global\` to cross session boundaries, which the service (session 0) and
    /// the agent (the console session) must do. Creating in that namespace needs
    /// `SeCreateGlobalPrivilege`, which LocalSystem holds.
    pub fn create(name: &str, len: usize, sddl: Option<&str>) -> Result<Self, Win32Error> {
        let attributes = sddl.map(SecurityAttributes::from_sddl).transpose()?;
        let wide_name = wide(name);

        let mapping = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                attributes.as_ref().map_or(null(), SecurityAttributes::as_ptr),
                PAGE_READWRITE,
                0,
                len as u32,
                wide_name.as_ptr(),
            )
        };

        if mapping.is_null() {
            return Err(Win32Error::last(format!("CreateFileMapping failed for '{name}'")));
        }

        Self::map(unsafe { OwnedHandle::from_raw_handle(mapping as _) }, len, name)
    }

    /// Opens a region created by another process.
    pub fn open(name: &str, len: usize) -> Result<Self, Win32Error> {
        let wide_name = wide(name);
        let mapping = unsafe { OpenFileMappingW(FILE_MAP_READ | FILE_MAP_WRITE, 0, wide_name.as_ptr()) };

        if mapping.is_null() {
            return Err(Win32Error::last(format!("OpenFileMapping failed for '{name}'")));
        }

        Self::map(unsafe { OwnedHandle::from_raw_handle(mapping as _) }, len, name)
    }

    fn map(mapping: OwnedHandle, len: usize, name: &str) -> Result<Self, Win32Error> {
        let view = unsafe {
            MapViewOfFile(mapping.as_raw_handle() as HANDLE, FILE_MAP_READ | FILE_MAP_WRITE, 0, 0, len)
        };

        if view.Value.is_null() {
            return Err(Win32Error::last(format!("MapViewOfFile failed for '{name}'")));
        }

        Ok(Self { _mapping: mapping, view: view.Value.cast(), len, name: name.to_owned() })
    }

    /// Base of the mapped view, valid until the region is dropped.
    pub fn as_ptr(&self) -> *mut u8 {
        self.view
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The full object name, including any namespace prefix.
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl Drop for SharedMemoryRegion {
    fn drop(&mut self) {
        unsafe { UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.view.cast() }) };
    }
}
