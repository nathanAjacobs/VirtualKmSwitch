//! Low-level mouse and keyboard hooks, used to swallow local input while this machine is driving
//! another, and to read the keys that are swallowed.
//!
//! The callbacks are on a hard latency budget. Windows delivers them by *sending a message* to the
//! installing thread, and a callback overrunning `LowLevelHooksTimeout` gets the hook *silently
//! removed with no notification*. That failure is invisible and catastrophic: the user keeps typing
//! believing they drive a remote machine while every keystroke lands locally. So these procedures
//! read a flag, compare integers, and return. No allocation, no blocking, no I/O.

use std::cell::Cell;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{GetLastError, HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, SetWindowsHookExW, UnhookWindowsHookEx, HHOOK, KBDLLHOOKSTRUCT, LLKHF_INJECTED,
    LLMHF_INJECTED, MSLLHOOKSTRUCT, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_APP, WM_DESTROY, WM_MOUSEMOVE,
};

use crate::message_window::MessageWindow;
use crate::{Win32Error, INJECTED_SIGNATURE};

/// Categories of input the hook can independently swallow.
pub mod block {
    pub const NONE: u32 = 0;
    pub const MOUSE_MOVE: u32 = 1 << 0;
    pub const MOUSE_BUTTONS: u32 = 1 << 1;
    pub const KEYBOARD: u32 = 1 << 2;
    pub const ALL: u32 = MOUSE_MOVE | MOUSE_BUTTONS | KEYBOARD;
}

/// What a hook observed, projected out of the native structures.
#[derive(Clone, Copy, Debug)]
pub struct HookInputInfo {
    /// The `WM_*` message that triggered the hook.
    pub message: u32,

    /// Scancode for keyboard events; zero for mouse.
    pub code: u16,

    /// Raw hook flags (`LLKHF_*` / `LLMHF_*`).
    pub flags: u32,

    /// Cursor position the event would have produced, in screen coordinates.
    pub x: i32,
    pub y: i32,

    /// Wheel delta or X-button index for mouse; the virtual key for keyboard.
    pub data: i32,
}

/// Observes a hooked event and decides whether to swallow it, independently of the block mask.
///
/// Returning `true` is how a hotkey chord is consumed while otherwise idle: the switch key must
/// never reach the local machine, even when nothing is being blocked. Runs inside the hook's
/// latency budget.
pub type HookObserver = Box<dyn Fn(&HookInputInfo) -> bool + Send + Sync>;

const WM_APP_INSTALL: u32 = WM_APP + 10;
const WM_APP_REINSTALL: u32 = WM_APP + 11;
const WM_APP_UNHOOK_SILENTLY: u32 = WM_APP + 12;

struct HookShared {
    observer: Option<HookObserver>,
    block_mask: AtomicU32,
    blocked: AtomicI64,
    injected_seen: AtomicI64,
    reinstalls: AtomicI64,
    installed: AtomicBool,
    install_error: AtomicU32,
    disposed: AtomicBool,
}

thread_local! {
    /// Hook callbacks arrive on the installing thread, so a thread-local resolves the instance.
    /// Valid while the window handler holds its `Arc`, and cleared on `WM_DESTROY`.
    static CURRENT: Cell<*const HookShared> = const { Cell::new(null()) };

    /// Handles owned by the hook thread, and released only there.
    static HOOKS: Cell<(HHOOK, HHOOK)> = const { Cell::new((null_mut(), null_mut())) };
}

pub struct LowLevelHook {
    shared: Arc<HookShared>,
    window: Option<MessageWindow>,
}

impl LowLevelHook {
    /// Installs both hooks on a dedicated thread.
    ///
    /// `observer` receives every hooked event that is not our own injection, blocked or not. This
    /// is how keys are captured, since blocking them removes them from raw input.
    pub fn new(observer: Option<HookObserver>) -> Result<Self, Win32Error> {
        let shared = Arc::new(HookShared {
            observer,
            block_mask: AtomicU32::new(block::NONE),
            blocked: AtomicI64::new(0),
            injected_seen: AtomicI64::new(0),
            reinstalls: AtomicI64::new(0),
            installed: AtomicBool::new(false),
            install_error: AtomicU32::new(0),
            disposed: AtomicBool::new(false),
        });

        let for_window = Arc::clone(&shared);

        // Its own window and thread, separate from raw input: a thread draining WM_INPUT at
        // kilohertz rates would queue hook callbacks behind it and eventually overrun the timeout.
        let window = MessageWindow::new(
            "Hooks",
            true,
            Box::new(move |_hwnd: HWND, message, _wparam, _lparam| {
                handle_message(&for_window, message)
            }),
        )?;

        // Hooks must be installed from the thread that pumps for them.
        window.post(WM_APP_INSTALL, 0, 0);

        let deadline = Instant::now() + Duration::from_secs(5);
        while !shared.installed.load(Ordering::Acquire) && Instant::now() < deadline {
            let error = shared.install_error.load(Ordering::Acquire);
            if error != 0 {
                return Err(Win32Error::with_code(error, "SetWindowsHookEx failed"));
            }

            std::thread::sleep(Duration::from_millis(1));
        }

        if !shared.installed.load(Ordering::Acquire) {
            return Err(Win32Error::with_code(0, "timed out installing low-level hooks"));
        }

        Ok(Self { shared, window: Some(window) })
    }

    /// Which categories of input are currently swallowed; see [`block`].
    pub fn block_mask(&self) -> u32 {
        self.shared.block_mask.load(Ordering::Relaxed)
    }

    pub fn set_block_mask(&self, mask: u32) {
        self.shared.block_mask.store(mask, Ordering::Relaxed);
    }

    /// Events swallowed so far.
    pub fn blocked_count(&self) -> i64 {
        self.shared.blocked.load(Ordering::Relaxed)
    }

    /// Whether both hooks are installed, as far as this process knows.
    pub fn is_installed(&self) -> bool {
        self.shared.installed.load(Ordering::Acquire)
    }

    /// Our own injected events observed by the hook.
    ///
    /// Windows offers no way to ask whether a hook is still installed -- one that overruns
    /// `LowLevelHooksTimeout` is removed silently -- so a watchdog injects a harmless event and
    /// watches this counter to prove the hook still runs.
    pub fn injected_seen_count(&self) -> i64 {
        self.shared.injected_seen.load(Ordering::Acquire)
    }

    /// How many times the hooks have been reinstalled after being found dead.
    pub fn reinstall_count(&self) -> i64 {
        self.shared.reinstalls.load(Ordering::Relaxed)
    }

    /// Removes the hooks the way Windows does when a callback overruns its timeout: without telling
    /// anyone, so this object still believes they are installed.
    ///
    /// For verification harnesses, which need to show that the watchdog notices.
    pub fn unhook_silently(&self) {
        if let Some(window) = &self.window {
            window.post(WM_APP_UNHOOK_SILENTLY, 0, 0);
        }
    }

    /// Tears the hooks down and installs them again, on the hook thread.
    pub fn reinstall(&self) {
        if let Some(window) = &self.window {
            window.post(WM_APP_REINSTALL, 0, 0);
        }
    }
}

impl Drop for LowLevelHook {
    fn drop(&mut self) {
        // Unblocked first: tearing down while blocked would leave the machine deaf for as long as
        // it takes the hooks to come out.
        self.shared.block_mask.store(block::NONE, Ordering::Relaxed);
        self.shared.disposed.store(true, Ordering::Release);

        // Destroying the window unhooks on the hook thread (see WM_DESTROY below).
        self.window.take();
        self.shared.installed.store(false, Ordering::Release);
    }
}

fn handle_message(shared: &Arc<HookShared>, message: u32) -> Option<LRESULT> {
    match message {
        WM_APP_INSTALL => {
            CURRENT.with(|current| current.set(Arc::as_ptr(shared)));
            install(shared);
            Some(0)
        }

        WM_APP_REINSTALL => {
            CURRENT.with(|current| current.set(Arc::as_ptr(shared)));

            // Unhook before rehooking: leaving the old handles installed would put two hooks in the
            // chain, doubling every callback.
            unhook();
            install(shared);
            shared.reinstalls.fetch_add(1, Ordering::Relaxed);
            Some(0)
        }

        WM_APP_UNHOOK_SILENTLY => {
            unhook();
            Some(0)
        }

        WM_DESTROY => {
            unhook();
            CURRENT.with(|current| current.set(null()));
            None
        }

        _ => None,
    }
}

fn install(shared: &HookShared) {
    // hmod is null: low-level hooks need no module handle, since they run in this process.
    let (mouse, keyboard) = unsafe {
        (
            SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_procedure), null_mut(), 0),
            SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_procedure), null_mut(), 0),
        )
    };

    if mouse.is_null() || keyboard.is_null() {
        let error = unsafe { GetLastError() }.max(1);
        unsafe {
            if !mouse.is_null() {
                UnhookWindowsHookEx(mouse);
            }
            if !keyboard.is_null() {
                UnhookWindowsHookEx(keyboard);
            }
        }

        shared.install_error.store(error, Ordering::Release);
        return;
    }

    HOOKS.with(|hooks| hooks.set((mouse, keyboard)));
    shared.installed.store(true, Ordering::Release);
}

fn unhook() {
    let (mouse, keyboard) = HOOKS.with(|hooks| hooks.replace((null_mut(), null_mut())));

    unsafe {
        if !mouse.is_null() {
            UnhookWindowsHookEx(mouse);
        }
        if !keyboard.is_null() {
            UnhookWindowsHookEx(keyboard);
        }
    }
}

fn current() -> Option<&'static HookShared> {
    let pointer = CURRENT.with(Cell::get);

    // SAFETY: set from an Arc the hook thread's window handler keeps alive, and cleared before that
    // handler is dropped. Callbacks only ever run on that same thread.
    unsafe { pointer.as_ref() }.filter(|shared| !shared.disposed.load(Ordering::Relaxed))
}

unsafe extern "system" fn mouse_procedure(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        if let (Some(shared), Some(data)) = (current(), (lparam as *const MSLLHOOKSTRUCT).as_ref()) {
            let message = wparam as u32;

            // Our own injected events pass through untouched. Blocking them would break driving a
            // peer that is itself driving another, which per-peer targeting allows.
            let ours = data.flags & LLMHF_INJECTED != 0 && data.dwExtraInfo == INJECTED_SIGNATURE as usize;

            if ours {
                shared.injected_seen.fetch_add(1, Ordering::Release);
            } else {
                let needed = if message == WM_MOUSEMOVE { block::MOUSE_MOVE } else { block::MOUSE_BUTTONS };

                let swallowed_by_observer = shared.observer.as_ref().is_some_and(|observer| {
                    observer(&HookInputInfo {
                        message,
                        code: 0,
                        flags: data.flags,
                        x: data.pt.x,
                        y: data.pt.y,
                        data: (data.mouseData >> 16) as u16 as i16 as i32,
                    })
                });

                if swallowed_by_observer || shared.block_mask.load(Ordering::Relaxed) & needed != 0 {
                    shared.blocked.fetch_add(1, Ordering::Relaxed);

                    // Non-zero suppresses the event system-wide, physical cursor movement included.
                    return 1;
                }
            }
        }
    }

    CallNextHookEx(null_mut(), code, wparam, lparam)
}

unsafe extern "system" fn keyboard_procedure(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        if let (Some(shared), Some(data)) = (current(), (lparam as *const KBDLLHOOKSTRUCT).as_ref()) {
            let ours = data.flags & LLKHF_INJECTED != 0 && data.dwExtraInfo == INJECTED_SIGNATURE as usize;

            if ours {
                shared.injected_seen.fetch_add(1, Ordering::Release);
            } else {
                let swallowed_by_observer = shared.observer.as_ref().is_some_and(|observer| {
                    observer(&HookInputInfo {
                        message: wparam as u32,
                        code: data.scanCode as u16,
                        flags: data.flags,
                        x: 0,
                        y: 0,
                        data: data.vkCode as i32,
                    })
                });

                if swallowed_by_observer || shared.block_mask.load(Ordering::Relaxed) & block::KEYBOARD != 0 {
                    shared.blocked.fetch_add(1, Ordering::Relaxed);
                    return 1;
                }
            }
        }
    }

    CallNextHookEx(null_mut(), code, wparam, lparam)
}
