//! An invisible top-level window that holds the foreground while local input is being forwarded.
//!
//! Blocking in the low-level hook does not stop raw input: mouse motion and buttons still reach every
//! `WM_INPUT` registration, which is how capture reads them. A game that reads its mouse from raw
//! input, and acts on it whenever its window is active, therefore keeps responding locally while
//! this machine drives another. Taking the foreground away from it is the one lever that needs no
//! cooperation from the game, so this window does that and nothing else.
//!
//! It exists only while wanted -- created on show, destroyed on hide -- and is a layered tool window
//! with zero alpha: nothing drawn, no hit testing, no taskbar button, no Alt+Tab entry.

use std::cell::Cell;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, CreateWindowExW, DefWindowProcW, DestroyWindow, GetForegroundWindow, IsWindow, RegisterClassExW, SetForegroundWindow, SetLayeredWindowAttributes,
    ShowWindow, UnregisterClassW, LWA_ALPHA, SW_SHOWNA, WM_APP, WM_DESTROY, WNDCLASSEXW, WS_EX_LAYERED,
    WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT, WS_POPUP,
};

use crate::message_window::MessageWindow;
use crate::{wide, Win32Error};

/// Posted to make the window thread match the decoy to what is currently wanted.
const WM_APP_SYNC: u32 = WM_APP + 20;

pub struct FocusDecoy {
    wanted: Arc<AtomicBool>,
    window: MessageWindow,
}

impl FocusDecoy {
    /// Starts the thread that will own the decoy. No decoy exists until [`show`](Self::show).
    pub fn new() -> Result<Self, Win32Error> {
        let wanted = Arc::new(AtomicBool::new(false));
        let for_window = Arc::clone(&wanted);
        let decoy = Decoy::default();

        // A window belongs to the thread that created it and is destroyed there, so the decoy lives
        // on a thread of its own rather than on one of the input threads, whose budgets it would eat.
        let window = MessageWindow::new(
            "FocusDecoy",
            false,
            Box::new(move |_hwnd: HWND, message, _wparam, _lparam| match message {
                WM_APP_SYNC => {
                    decoy.sync(for_window.load(Ordering::Acquire));
                    Some(0)
                }

                WM_DESTROY => {
                    decoy.sync(false);
                    None
                }

                _ => None,
            }),
        )?;

        Ok(Self { wanted, window })
    }

    /// Creates the decoy and gives it the foreground. Non-blocking; safe from a hook callback.
    pub fn show(&self) {
        self.set_wanted(true);
    }

    /// Hands the foreground back and destroys the decoy. Non-blocking; safe from a hook callback, and
    /// a single load when there is nothing to hide.
    pub fn hide(&self) {
        self.set_wanted(false);
    }

    fn set_wanted(&self, wanted: bool) {
        if self.wanted.load(Ordering::Relaxed) == wanted || self.wanted.swap(wanted, Ordering::AcqRel) == wanted {
            return;
        }

        // The message carries no state: the window thread reads `wanted` when it gets there, so a
        // show and a hide racing from different threads still settle on whichever was last.
        self.window.post(WM_APP_SYNC, 0, 0);
    }
}

/// Owned by the window thread.
#[derive(Default)]
struct Decoy {
    /// The decoy while it exists; zero otherwise.
    hwnd: Cell<isize>,

    /// What held the foreground when the decoy took it, to hand back on release.
    previous: Cell<isize>,
}

impl Decoy {
    fn sync(&self, wanted: bool) {
        match (wanted, self.hwnd.get() != 0) {
            (true, false) => self.create(),
            (false, true) => self.destroy(),
            _ => {}
        }
    }

    fn create(&self) {
        let class_name = class_name();
        let title = wide("VirtualKmSwitch");

        unsafe {
            let instance = GetModuleHandleW(null());

            let class = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(DefWindowProcW),
                hInstance: instance,
                lpszClassName: class_name.as_ptr(),
                ..std::mem::zeroed()
            };

            if RegisterClassExW(&class) == 0 {
                log::warn!("{}", Win32Error::last("RegisterClassEx failed for the focus decoy"));
                return;
            }

            // Visible rather than hidden, so nothing that skips hidden windows when choosing what is
            // active can pass over it; zero alpha and WS_EX_TRANSPARENT make that visibility moot.
            // WS_EX_TOOLWINDOW keeps it off the taskbar and out of Alt+Tab.
            let hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_TRANSPARENT,
                class_name.as_ptr(),
                title.as_ptr(),
                WS_POPUP,
                0,
                0,
                1,
                1,
                null_mut(),
                null_mut(),
                instance,
                null(),
            );

            if hwnd.is_null() {
                log::warn!("{}", Win32Error::last("CreateWindowEx failed for the focus decoy"));
                UnregisterClassW(class_name.as_ptr(), instance);
                return;
            }

            SetLayeredWindowAttributes(hwnd, 0, 0, LWA_ALPHA);

            let previous = GetForegroundWindow();
            self.hwnd.set(hwnd as isize);
            self.previous.set(previous as isize);

            ShowWindow(hwnd, SW_SHOWNA);
            take_foreground(hwnd);
        }
    }

    fn destroy(&self) {
        let hwnd = self.hwnd.replace(0) as HWND;
        let previous = self.previous.replace(0) as HWND;

        unsafe {
            // Handed back only if the decoy still has it: if something else took the foreground in the
            // meantime, that is where it belongs now. Holding the foreground is what permits this.
            if GetForegroundWindow() == hwnd && !previous.is_null() && IsWindow(previous) != 0 {
                SetForegroundWindow(previous);
            }

            DestroyWindow(hwnd);
            UnregisterClassW(class_name().as_ptr(), GetModuleHandleW(null()));
        }
    }
}

/// A class name unique to this process and thread, as message windows use: registration is
/// process-wide. It must also differ from the class of the message window on this same thread, which
/// is `VirtualKmSwitch.FocusDecoy.{pid}.{tid}`.
fn class_name() -> Vec<u16> {
    wide(&format!("VirtualKmSwitch.FocusDecoyWindow.{}.{}", std::process::id(), unsafe { GetCurrentThreadId() }))
}

/// Moves the foreground to `hwnd`.
unsafe fn take_foreground(hwnd: HWND) {
    BringWindowToTop(hwnd);
    SetForegroundWindow(hwnd);
    SetFocus(hwnd);

    if GetForegroundWindow() != hwnd {
        // Not fatal: blocking still works. Only games reading raw input notice the difference.
        log::warn!("the focus decoy could not take the foreground; raw-input games may still see local mouse input");
    }
}
