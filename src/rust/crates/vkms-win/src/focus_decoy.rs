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
//!
//! **It lives in a helper process, never in the one that owns the hooks.** While a window belonging
//! to a process holds the foreground, Windows does not call that process's low-level keyboard hook,
//! and keys go straight past it. A decoy inside the agent therefore blinded the hook to every hotkey,
//! panic included, for exactly as long as it was blocking. Measured: with the hooking process's own
//! window in front no chord was ever seen, and Ctrl+Alt+Shift+Esc reached the desktop as Alt+Esc.
//!
//! The host is a copy of the same executable started with [`HOST_FLAG`]. The parent sends it one
//! byte per change over its standard input, and closing that input -- deliberately, or by exiting --
//! makes the host hand the foreground back, destroy the decoy and exit.

use std::cell::{Cell, RefCell};
use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, BringWindowToTop, CreateWindowExW, DefWindowProcW, DestroyWindow,
    GetForegroundWindow, IsWindow, RegisterClassExW, SetForegroundWindow, SetLayeredWindowAttributes,
    ShowWindow, UnregisterClassW, LWA_ALPHA, SW_SHOWNA, WM_APP, WM_DESTROY, WNDCLASSEXW, WS_EX_LAYERED,
    WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT, WS_POPUP,
};

use vkms_core::{clock, InputEvent};

use crate::injector::InputInjector;
use crate::message_window::MessageWindow;
use crate::{wide, Win32Error};

/// The argument that starts a process as a decoy host. Any executable that creates a [`FocusDecoy`]
/// must check for it before doing anything else and hand over to [`run_host`].
pub const HOST_FLAG: &str = "--focus-decoy";

/// Posted to make a window thread act on a change: to the relay in the parent, or to the decoy's
/// thread in the host, where `wparam` carries the state.
const WM_APP_SYNC: u32 = WM_APP + 20;

const SHOW: u8 = b'1';
const HIDE: u8 = b'0';

pub struct FocusDecoy {
    wanted: Arc<AtomicBool>,
    relay: MessageWindow,
}

impl FocusDecoy {
    /// Starts the host process, and the thread that talks to it. No decoy exists until
    /// [`show`](Self::show).
    pub fn new() -> Result<Self, Win32Error> {
        let wanted = Arc::new(AtomicBool::new(false));
        let for_relay = Arc::clone(&wanted);

        // Started now rather than on the first show, so engaging never waits for a process launch
        // while the game is still reading the mouse.
        let host = HostProcess::default();
        host.ensure_running();

        // Writing to a pipe is I/O, which show and hide may not do: they are called from the hook
        // and raw-input threads. So they post here, and this thread writes.
        let relay = MessageWindow::new(
            "FocusDecoyRelay",
            false,
            Box::new(move |_hwnd: HWND, message, _wparam, _lparam| match message {
                WM_APP_SYNC => {
                    host.send(for_relay.load(Ordering::Acquire));
                    Some(0)
                }

                WM_DESTROY => {
                    host.stop();
                    None
                }

                _ => None,
            }),
        )?;

        Ok(Self { wanted, relay })
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

        // The message carries no state: the relay reads `wanted` when it gets there, so a show and a
        // hide racing from different threads still settle on whichever was last.
        self.relay.post(WM_APP_SYNC, 0, 0);
    }
}

/// The parent's handle on the host. Owned by the relay thread.
#[derive(Default)]
struct HostProcess {
    child: RefCell<Option<Child>>,
}

impl HostProcess {
    fn send(&self, wanted: bool) {
        // A host that has died is replaced once, rather than leaving every later engagement without
        // a decoy.
        for _ in 0..2 {
            self.ensure_running();

            let mut child = self.child.borrow_mut();
            let Some(host) = child.as_mut() else {
                return;
            };

            if wanted {
                // The host is a separate process with no right of its own to take the foreground, so
                // this process lends it whatever right it has.
                unsafe { AllowSetForegroundWindow(host.id()) };
            }

            let byte = if wanted { SHOW } else { HIDE };
            let written = host.stdin.as_mut().is_some_and(|stdin| stdin.write_all(&[byte]).and_then(|()| stdin.flush()).is_ok());
            if written {
                return;
            }

            log::warn!("the focus decoy host has stopped; restarting it");
            *child = None;
        }
    }

    fn ensure_running(&self) {
        if self.child.borrow().is_some() {
            return;
        }

        match spawn_host() {
            Ok(host) => *self.child.borrow_mut() = Some(host),
            Err(error) => log::warn!("could not start the focus decoy host: {error}"),
        }
    }

    /// Closing the host's input is its signal to hand the foreground back, destroy the decoy and exit.
    fn stop(&self) {
        if let Some(mut host) = self.child.borrow_mut().take() {
            drop(host.stdin.take());
        }
    }
}

/// This executable again, with this process's arguments, so the host can name its log after the same
/// desktop.
fn spawn_host() -> std::io::Result<Child> {
    Command::new(std::env::current_exe()?)
        .arg(HOST_FLAG)
        .args(std::env::args_os().skip(1))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
}

/// The host process's whole life: shows and hides the decoy as the parent asks, and returns once the
/// parent closes standard input, with the foreground handed back and the decoy destroyed.
pub fn run_host() -> Result<(), Win32Error> {
    let decoy = Decoy::default();

    // A window belongs to the thread that created it and is destroyed there, so the decoy gets a
    // thread with a pump of its own while this one blocks reading the parent.
    let window = MessageWindow::new(
        "FocusDecoy",
        false,
        Box::new(move |_hwnd: HWND, message, wparam, _lparam| match message {
            WM_APP_SYNC => {
                decoy.sync(wparam != 0);
                Some(0)
            }

            WM_DESTROY => {
                decoy.sync(false);
                None
            }

            _ => None,
        }),
    )?;

    let mut input = std::io::stdin().lock();
    let mut byte = [0u8; 1];

    // End of input and a read error mean the same thing: the parent is gone.
    while let Ok(1) = input.read(&mut byte) {
        match byte[0] {
            SHOW => window.post(WM_APP_SYNC, 1, 0),
            HIDE => window.post(WM_APP_SYNC, 0, 0),
            _ => continue,
        };
    }

    // Dropping the window destroys it on its own thread, and WM_DESTROY takes the decoy down first.
    drop(window);
    Ok(())
}

/// The decoy itself, owned by the host process's window thread.
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
///
/// While the user is active, Windows refuses `SetForegroundWindow` to a process that is neither in
/// the foreground nor the source of the last input event. The right the parent lends with
/// `AllowSetForegroundWindow` only exists when the parent itself has it, which it does only some of
/// the time -- when it has just injected releases for modifiers still held from the chord. When the
/// first attempt is refused, injecting an event makes this process the source of the last input:
/// a zero-length move, so the cursor stays put, tagged as ours so the hook and capture ignore it.
unsafe fn take_foreground(hwnd: HWND) {
    if !try_take_foreground(hwnd) {
        InputInjector::inject_one(&InputEvent::mouse_move(clock::ticks(), 0, 0));

        if try_take_foreground(hwnd) {
            log::info!("the focus decoy took the foreground after a zero-length move");
        }
    }

    if GetForegroundWindow() != hwnd {
        // Not fatal: blocking still works. Only games reading raw input notice the difference.
        log::warn!("the focus decoy could not take the foreground; raw-input games may still see local mouse input");
    }
}

unsafe fn try_take_foreground(hwnd: HWND) -> bool {
    BringWindowToTop(hwnd);
    SetForegroundWindow(hwnd);
    SetFocus(hwnd);

    GetForegroundWindow() == hwnd
}
