use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL};

/// Raises the calling thread to above-normal priority.
///
/// Used by the input threads: being descheduled there delays a keystroke that has already been
/// captured, or a hook callback that Windows is timing.
pub fn raise_current_priority() {
    unsafe {
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL);
    }
}
