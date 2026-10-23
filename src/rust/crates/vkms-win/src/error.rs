use std::fmt;
use std::io;

/// A failed Win32 call: what was being attempted, and the error code it left behind.
///
/// The code is kept as a number rather than folded into an `io::ErrorKind`, because the cases worth
/// reacting to -- access denied while creating a `Global\` object, say -- are told apart by it.
#[derive(Debug, Clone)]
pub struct Win32Error {
    pub code: u32,
    pub context: String,
}

impl Win32Error {
    pub const ACCESS_DENIED: u32 = 5;

    /// Captures `GetLastError` for the call that just failed.
    pub fn last(context: impl Into<String>) -> Self {
        let code = io::Error::last_os_error().raw_os_error().unwrap_or(0) as u32;
        Self { code, context: context.into() }
    }

    pub fn with_code(code: u32, context: impl Into<String>) -> Self {
        Self { code, context: context.into() }
    }

    pub fn is_access_denied(&self) -> bool {
        self.code == Self::ACCESS_DENIED
    }
}

impl fmt::Display for Win32Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let detail = io::Error::from_raw_os_error(self.code as i32);
        write!(f, "{} ({detail})", self.context)
    }
}

impl std::error::Error for Win32Error {}

impl From<Win32Error> for io::Error {
    fn from(error: Win32Error) -> Self {
        let kind = io::Error::from_raw_os_error(error.code as i32).kind();
        io::Error::new(kind, error)
    }
}
