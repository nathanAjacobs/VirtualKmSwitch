use std::path::Path;
use std::ptr::null;

use windows_sys::Win32::Storage::FileSystem::{ReplaceFileW, REPLACEFILE_IGNORE_MERGE_ERRORS};

use crate::{wide, Win32Error};

/// Atomically swaps `replacement` into `replaced`, moving the previous `replaced` to `backup`.
///
/// `ReplaceFile` rather than two renames: a crash between two renames leaves no primary at all,
/// whereas this either happened or did not.
pub fn replace_file(replaced: &Path, replacement: &Path, backup: &Path) -> Result<(), Win32Error> {
    let replaced_name = wide(&replaced.to_string_lossy());
    let replacement_name = wide(&replacement.to_string_lossy());
    let backup_name = wide(&backup.to_string_lossy());

    let ok = unsafe {
        ReplaceFileW(
            replaced_name.as_ptr(),
            replacement_name.as_ptr(),
            backup_name.as_ptr(),
            REPLACEFILE_IGNORE_MERGE_ERRORS,
            null(),
            null(),
        )
    };

    if ok == 0 {
        return Err(Win32Error::last(format!("ReplaceFile failed for '{}'", replaced.display())));
    }

    Ok(())
}
