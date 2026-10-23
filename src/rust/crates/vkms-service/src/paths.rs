//! Where the service keeps its state.

use std::io;
use std::path::{Path, PathBuf};

/// `%ProgramData%\VirtualKmSwitch`. Machine-scoped: what is stored here describes the computer and the
/// keyboard attached to it, not whoever is signed in.
pub fn data_dir() -> PathBuf {
    let base = std::env::var_os("ProgramData").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    base.join("VirtualKmSwitch")
}

/// SYSTEM and Administrators may change anything; users may read, so they can look at logs.
///
/// Explicit rather than inherited. ProgramData lets ordinary users create files in new
/// subdirectories, and a user who could drop a `peers.json` here before the service first wrote one
/// would decide whose keystrokes this machine accepts.
const DATA_DIR_SDDL: &str = "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FRFX;;;BU)";

/// Creates the directory and, when running as SYSTEM, locks its DACL down.
pub fn ensure_data_dir(path: &Path) -> io::Result<()> {
    std::fs::create_dir_all(path)?;

    // Only as SYSTEM: a developer running --console must not lock themselves out of the directory
    // their own debug build writes to.
    if vkms_win::security::is_system() {
        vkms_win::security::protect_path(path, DATA_DIR_SDDL)?;
    }

    Ok(())
}
