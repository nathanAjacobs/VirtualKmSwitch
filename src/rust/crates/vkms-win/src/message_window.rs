//! A message-only window (`HWND_MESSAGE`) owning a dedicated thread and its own message pump.
//!
//! Two things in this codebase need a thread with a pump: raw-input delivery, and low-level hooks --
//! which are invoked by *sending a message* to the installing thread. They get separate instances
//! deliberately: a thread draining `WM_INPUT` at kilohertz rates cannot also service hook callbacks
//! without risking the `LowLevelHooksTimeout`, and exceeding it makes Windows silently unhook us with
//! no notification at all.

use std::cell::RefCell;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{mpsc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, PostMessageW,
    PostQuitMessage, RegisterClassExW, TranslateMessage, UnregisterClassW, HWND_MESSAGE, MSG,
    WM_APP, WM_DESTROY, WNDCLASSEXW,
};

use crate::{thread, wide, Win32Error};

/// Handles one window message on the window's own thread. Return `Some(result)` to consume it, or
/// `None` for default processing.
///
/// `Fn` rather than `FnMut`: window procedures re-enter (a modal loop, or `DefWindowProc` sending a
/// message of its own), and a mutable borrow held across that would panic. State lives in `Cell`s
/// or atomics instead.
pub type MessageHandler = Box<dyn Fn(HWND, u32, WPARAM, LPARAM) -> Option<LRESULT> + Send>;

/// Posted to ask the window to destroy itself on its owning thread.
const WM_APP_CLOSE: u32 = WM_APP + 1;

thread_local! {
    /// The window procedure always runs on the thread that owns the window, so thread-local storage
    /// resolves the handler with no lookup on a path that carries every `WM_INPUT`.
    static HANDLER: RefCell<Option<MessageHandler>> = const { RefCell::new(None) };
}

pub struct MessageWindow {
    hwnd: AtomicIsize,
    thread: Option<JoinHandle<()>>,
    exited: Mutex<mpsc::Receiver<()>>,
}

impl MessageWindow {
    /// Creates the window on a new thread and starts pumping.
    ///
    /// `above_normal` raises the thread's priority, which the input threads want: being descheduled
    /// there delays a keystroke, or worse, a hook callback against its timeout.
    pub fn new(name: &str, above_normal: bool, handler: MessageHandler) -> Result<Self, Win32Error> {
        let (ready_tx, ready_rx) = mpsc::channel::<Result<isize, Win32Error>>();
        let (exit_tx, exit_rx) = mpsc::channel::<()>();
        let thread_name = format!("VirtualKmSwitch.{name}");
        let window_name = name.to_owned();

        let thread = std::thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                if above_normal {
                    thread::raise_current_priority();
                }

                run_window_thread(&window_name, handler, &ready_tx);
                let _ = exit_tx.send(());
            })
            .map_err(|error| Win32Error::with_code(0, format!("could not start the {name} thread: {error}")))?;

        // Surface creation failures to the caller rather than leaving a dead thread behind.
        let hwnd = match ready_rx.recv() {
            Ok(Ok(hwnd)) => hwnd,
            Ok(Err(error)) => {
                let _ = thread.join();
                return Err(error);
            }
            Err(_) => return Err(Win32Error::with_code(0, format!("the {name} window thread died"))),
        };

        Ok(Self { hwnd: AtomicIsize::new(hwnd), thread: Some(thread), exited: Mutex::new(exit_rx) })
    }

    /// The window handle, valid until the window is dropped.
    pub fn handle(&self) -> HWND {
        self.hwnd.load(Ordering::Acquire) as HWND
    }

    /// Posts a message to the window's own thread. Safe from any thread.
    pub fn post(&self, message: u32, wparam: WPARAM, lparam: LPARAM) -> bool {
        let hwnd = self.handle();
        !hwnd.is_null() && unsafe { PostMessageW(hwnd, message, wparam, lparam) } != 0
    }
}

impl Drop for MessageWindow {
    fn drop(&mut self) {
        // DestroyWindow is thread-affine, so the request is posted rather than made from here.
        self.post(WM_APP_CLOSE, 0, 0);

        // Waited for, but not forever: if the thread is wedged the process is exiting anyway, and
        // hanging shutdown would only make that worse.
        let exited = self.exited.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).recv_timeout(Duration::from_secs(5));
        if exited.is_ok() {
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }

        self.hwnd.store(0, Ordering::Release);
    }
}

fn run_window_thread(name: &str, handler: MessageHandler, ready: &mpsc::Sender<Result<isize, Win32Error>>) {
    HANDLER.with(|slot| *slot.borrow_mut() = Some(handler));

    // A unique class name per instance: registration is process-wide, and two windows sharing a
    // name would collide across restarts within one process.
    let class_name = wide(&format!(
        "VirtualKmSwitch.{name}.{}.{}",
        std::process::id(),
        unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() }
    ));
    let window_name = wide(name);

    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());

        let class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(window_procedure),
            hInstance: instance,
            lpszClassName: class_name.as_ptr(),
            ..std::mem::zeroed()
        };

        if RegisterClassExW(&class) == 0 {
            let _ = ready.send(Err(Win32Error::last(format!("RegisterClassEx failed for the {name} window"))));
            HANDLER.with(|slot| *slot.borrow_mut() = None);
            return;
        }

        // HWND_MESSAGE as the parent makes the window message-only: no display, no z-order, no
        // enumeration by other applications, but a real queue and a real HWND, which is all that
        // raw-input registration and hooks require.
        let hwnd = CreateWindowExW(
            0,
            class_name.as_ptr(),
            window_name.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            null_mut(),
            instance,
            std::ptr::null(),
        );

        if hwnd.is_null() {
            let _ = ready.send(Err(Win32Error::last(format!("CreateWindowEx failed for the {name} window"))));
        } else {
            let _ = ready.send(Ok(hwnd as isize));
            pump_messages();
        }

        UnregisterClassW(class_name.as_ptr(), instance);
    }

    HANDLER.with(|slot| *slot.borrow_mut() = None);
}

unsafe fn pump_messages() {
    let mut message: MSG = std::mem::zeroed();

    // GetMessage returns -1 on error, which must not be treated as a message.
    while GetMessageW(&mut message, null_mut(), 0, 0) > 0 {
        TranslateMessage(&message);
        DispatchMessageW(&message);
    }
}

unsafe extern "system" fn window_procedure(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if message == WM_APP_CLOSE {
        DestroyWindow(hwnd);
        return 0;
    }

    // Shared borrow only, so a re-entrant call made while the handler runs is fine.
    let handled = HANDLER.with(|slot| match slot.try_borrow() {
        Ok(handler) => handler.as_ref().and_then(|handler| handler(hwnd, message, wparam, lparam)),
        Err(_) => None,
    });

    // The handler sees WM_DESTROY first so that anything bound to this thread -- hooks, above all --
    // can be released here, on the thread that owns it.
    if message == WM_DESTROY {
        PostQuitMessage(0);
        return 0;
    }

    match handled {
        Some(result) => result,
        None => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}
