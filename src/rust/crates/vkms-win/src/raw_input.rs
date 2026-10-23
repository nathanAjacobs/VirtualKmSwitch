//! Reads physical mouse and keyboard via the Raw Input API on a dedicated message-only window.
//!
//! Raw input is the mouse capture mechanism because it is the only one reporting true relative
//! deltas, before pointer ballistics. Low-level hooks only *block* mouse input.

use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT};
use windows_sys::Win32::UI::Input::{
    GetRawInputBuffer, GetRawInputData, RegisterRawInputDevices, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE,
    RAWINPUTHEADER, RAWKEYBOARD, RAWMOUSE, RIDEV_INPUTSINK, RID_INPUT, RIM_TYPEKEYBOARD, RIM_TYPEMOUSE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::WM_INPUT;

use vkms_core::{clock, InputEvent, InputEventFlags};

use crate::message_window::MessageWindow;
use crate::{Win32Error, INJECTED_SIGNATURE};

/// Receives decoded input events, on the raw-input thread. Must not block.
pub type RawInputSink = Box<dyn Fn(&InputEvent) + Send + Sync>;

const HID_USAGE_PAGE_GENERIC: u16 = 0x01;
const HID_USAGE_GENERIC_MOUSE: u16 = 0x02;
const HID_USAGE_GENERIC_KEYBOARD: u16 = 0x06;

// RAWMOUSE.usFlags
const MOUSE_MOVE_ABSOLUTE: u16 = 0x01;

// RAWMOUSE.usButtonFlags
const RI_MOUSE_LEFT_BUTTON_DOWN: u16 = 0x0001;
const RI_MOUSE_LEFT_BUTTON_UP: u16 = 0x0002;
const RI_MOUSE_RIGHT_BUTTON_DOWN: u16 = 0x0004;
const RI_MOUSE_RIGHT_BUTTON_UP: u16 = 0x0008;
const RI_MOUSE_MIDDLE_BUTTON_DOWN: u16 = 0x0010;
const RI_MOUSE_MIDDLE_BUTTON_UP: u16 = 0x0020;
const RI_MOUSE_BUTTON_4_DOWN: u16 = 0x0040;
const RI_MOUSE_BUTTON_4_UP: u16 = 0x0080;
const RI_MOUSE_BUTTON_5_DOWN: u16 = 0x0100;
const RI_MOUSE_BUTTON_5_UP: u16 = 0x0200;
const RI_MOUSE_WHEEL: u16 = 0x0400;
const RI_MOUSE_HWHEEL: u16 = 0x0800;

// RAWKEYBOARD.Flags: bare #defines in winuser.h with no metadata grouping.
const RI_KEY_BREAK: u16 = 0x0001;
const RI_KEY_E0: u16 = 0x0002;
const RI_KEY_E1: u16 = 0x0004;

/// Keyboard overrun sentinel; such events are garbage and must be dropped.
const KEYBOARD_OVERRUN_MAKE_CODE: u16 = 0x00FF;
const SCANCODE_LSHIFT: u16 = 0x002A;
const SCANCODE_RSHIFT: u16 = 0x0036;

/// Counters, readable from any thread.
#[derive(Default)]
pub struct RawInputStats {
    /// Physical events decoded and forwarded to the sink.
    pub events: AtomicI64,

    /// Events discarded as our own injection. Rising means loopback suppression works.
    pub injected_suppressed: AtomicI64,

    /// Every entry seen, before filtering. Distinguishes "raw input is not arriving" from "it
    /// arrives but decodes to nothing" -- very different faults that look identical from outside.
    pub raw_samples: AtomicI64,

    /// Samples with no source device handle. Diagnostic only: this is deliberately *not* used to
    /// identify injection, because real mice report it too.
    pub null_device_samples: AtomicI64,

    /// Samples reporting absolute position. Non-zero means a tablet, RDP, or a virtual machine.
    pub absolute_samples: AtomicI64,
}

pub struct RawInputReader {
    stats: Arc<RawInputStats>,
    _window: MessageWindow,
}

/// State touched only by the raw-input thread.
struct ReaderState {
    sink: RawInputSink,
    stats: Arc<RawInputStats>,

    /// Reused across every drain so the hot path allocates nothing; grown only when
    /// `GetRawInputBuffer` reports it is too small for the next entry. `u64` for 8-byte alignment.
    drain_buffer: RefCell<Vec<u64>>,

    // Devices reporting absolute position are differenced into deltas.
    last_absolute: Cell<(i32, i32)>,
    have_absolute_origin: Cell<bool>,
}

impl RawInputReader {
    pub fn new(sink: RawInputSink) -> Result<Self, Win32Error> {
        let stats = Arc::new(RawInputStats::default());

        let state = ReaderState {
            sink,
            stats: Arc::clone(&stats),
            drain_buffer: RefCell::new(vec![0u64; 64 * std::mem::size_of::<RAWINPUT>() / 8]),
            last_absolute: Cell::new((0, 0)),
            have_absolute_origin: Cell::new(false),
        };

        let window = MessageWindow::new(
            "RawInput",
            true,
            Box::new(move |_hwnd: HWND, message, _wparam, lparam| state.handle_message(message, lparam)),
        )?;

        register(window.handle())?;
        Ok(Self { stats, _window: window })
    }

    pub fn stats(&self) -> &RawInputStats {
        &self.stats
    }
}

fn register(target: HWND) -> Result<(), Win32Error> {
    // RIDEV_INPUTSINK is the whole point: it keeps input flowing to us without foreground focus,
    // which is always the case here.
    //
    // Registration is per process and last-call-wins, so it happens exactly once per process --
    // which is a reason the controller UI, whose framework might register too, never captures.
    let devices = [
        RAWINPUTDEVICE {
            usUsagePage: HID_USAGE_PAGE_GENERIC,
            usUsage: HID_USAGE_GENERIC_MOUSE,
            dwFlags: RIDEV_INPUTSINK,
            hwndTarget: target,
        },
        RAWINPUTDEVICE {
            usUsagePage: HID_USAGE_PAGE_GENERIC,
            usUsage: HID_USAGE_GENERIC_KEYBOARD,
            dwFlags: RIDEV_INPUTSINK,
            hwndTarget: target,
        },
    ];

    let registered = unsafe {
        RegisterRawInputDevices(devices.as_ptr(), devices.len() as u32, std::mem::size_of::<RAWINPUTDEVICE>() as u32)
    };

    if registered == 0 {
        return Err(Win32Error::last("RegisterRawInputDevices failed"));
    }

    Ok(())
}

impl ReaderState {
    fn handle_message(&self, message: u32, lparam: LPARAM) -> Option<LRESULT> {
        if message != WM_INPUT {
            return None;
        }

        self.read_current_event(lparam);

        // GetMessage already dequeued the current WM_INPUT, so GetRawInputBuffer sees only what
        // arrived after it. One event per message cannot keep up with a modern high-polling-rate
        // mouse, which is why a buffered drain follows every single event.
        self.drain_buffered_events();

        // WM_INPUT still needs DefWindowProc for cleanup.
        None
    }

    fn read_current_event(&self, lparam: LPARAM) {
        let mut raw: RAWINPUT = unsafe { std::mem::zeroed() };
        let mut size = std::mem::size_of::<RAWINPUT>() as u32;

        let written = unsafe {
            GetRawInputData(
                lparam as HRAWINPUT,
                RID_INPUT,
                (&mut raw as *mut RAWINPUT).cast(),
                &mut size,
                std::mem::size_of::<RAWINPUTHEADER>() as u32,
            )
        };

        if written != u32::MAX && written != 0 {
            self.decode(&raw);
        }
    }

    fn drain_buffered_events(&self) {
        let mut buffer = self.drain_buffer.borrow_mut();

        loop {
            let mut size = (buffer.len() * 8) as u32;
            let count = unsafe {
                GetRawInputBuffer(
                    buffer.as_mut_ptr().cast::<RAWINPUT>(),
                    &mut size,
                    std::mem::size_of::<RAWINPUTHEADER>() as u32,
                )
            };

            if count == 0 {
                return; // drained
            }

            if count == u32::MAX {
                // Too small for the next entry. Grow and retry, but refuse to grow without bound.
                if buffer.len() * 8 >= 4096 * std::mem::size_of::<RAWINPUT>() {
                    return;
                }

                let doubled = buffer.len() * 2;
                buffer.resize(doubled, 0);
                continue;
            }

            let base = buffer.as_ptr().cast::<u8>();
            let end = unsafe { base.add(buffer.len() * 8) };
            let mut current = base;

            for _ in 0..count {
                if current >= end {
                    break;
                }

                // SAFETY: GetRawInputBuffer wrote `count` packed entries starting at the base.
                let raw = unsafe { &*current.cast::<RAWINPUT>() };
                self.decode(raw);

                // Entries are variable-length and packed on 8-byte boundaries (NEXTRAWINPUTBLOCK),
                // so advance by the reported size, rounded up, rather than by sizeof(RAWINPUT).
                let advance = (raw.header.dwSize as usize + 7) & !7;
                if advance == 0 {
                    break;
                }

                current = unsafe { current.add(advance) };
            }

            // More may have arrived while decoding.
        }
    }

    fn decode(&self, raw: &RAWINPUT) {
        self.stats.raw_samples.fetch_add(1, Ordering::Relaxed);

        if raw.header.hDevice.is_null() {
            self.stats.null_device_samples.fetch_add(1, Ordering::Relaxed);
        }

        let timestamp = clock::ticks();

        // SAFETY: the union member read matches the type the header declares.
        unsafe {
            if raw.header.dwType == RIM_TYPEMOUSE {
                self.decode_mouse(&raw.data.mouse, timestamp);
            } else if raw.header.dwType == RIM_TYPEKEYBOARD {
                self.decode_keyboard(&raw.data.keyboard, timestamp);
            }
        }
    }

    fn decode_mouse(&self, mouse: &RAWMOUSE, timestamp: i64) {
        // Our own injection, echoed back. Suppressing it is what stops a peer that is both driving
        // another machine and being driven from looping input round the mesh.
        if mouse.ulExtraInformation == INJECTED_SIGNATURE {
            self.stats.injected_suppressed.fetch_add(1, Ordering::Relaxed);
            return;
        }

        let mut dx = mouse.lLastX;
        let mut dy = mouse.lLastY;

        // Tablets, RDP sessions and most virtual machines report absolute positions in a 0..65535
        // space rather than deltas. Treating those as deltas teleports the cursor, so they are
        // differenced here instead.
        if mouse.usFlags & MOUSE_MOVE_ABSOLUTE != 0 {
            self.stats.absolute_samples.fetch_add(1, Ordering::Relaxed);

            let (last_x, last_y) = self.last_absolute.get();
            self.last_absolute.set((dx, dy));

            if self.have_absolute_origin.replace(true) {
                dx -= last_x;
                dy -= last_y;
            } else {
                // The first absolute sample only establishes the origin.
                dx = 0;
                dy = 0;
            }
        }

        if dx != 0 || dy != 0 {
            self.emit(InputEvent::mouse_move(timestamp, dx, dy));
        }

        // SAFETY: both union members are plain integers.
        let (button_flags, button_data) =
            unsafe { (mouse.Anonymous.Anonymous.usButtonFlags, mouse.Anonymous.Anonymous.usButtonData) };

        if button_flags == 0 {
            return;
        }

        self.emit_button(button_flags, RI_MOUSE_LEFT_BUTTON_DOWN, RI_MOUSE_LEFT_BUTTON_UP, 0, timestamp);
        self.emit_button(button_flags, RI_MOUSE_RIGHT_BUTTON_DOWN, RI_MOUSE_RIGHT_BUTTON_UP, 1, timestamp);
        self.emit_button(button_flags, RI_MOUSE_MIDDLE_BUTTON_DOWN, RI_MOUSE_MIDDLE_BUTTON_UP, 2, timestamp);
        self.emit_button(button_flags, RI_MOUSE_BUTTON_4_DOWN, RI_MOUSE_BUTTON_4_UP, 3, timestamp);
        self.emit_button(button_flags, RI_MOUSE_BUTTON_5_DOWN, RI_MOUSE_BUTTON_5_UP, 4, timestamp);

        if button_flags & (RI_MOUSE_WHEEL | RI_MOUSE_HWHEEL) != 0 {
            // usButtonData carries the wheel delta as a signed short in an unsigned field.
            let delta = button_data as i16 as i32;
            self.emit(InputEvent::mouse_wheel(timestamp, delta, button_flags & RI_MOUSE_HWHEEL != 0));
        }
    }

    fn emit_button(&self, flags: u16, down_bit: u16, up_bit: u16, button: u16, timestamp: i64) {
        if flags & down_bit != 0 {
            self.emit(InputEvent::mouse_button(timestamp, button, false));
        }

        if flags & up_bit != 0 {
            self.emit(InputEvent::mouse_button(timestamp, button, true));
        }
    }

    fn decode_keyboard(&self, keyboard: &RAWKEYBOARD, timestamp: i64) {
        if keyboard.ExtraInformation == INJECTED_SIGNATURE {
            self.stats.injected_suppressed.fetch_add(1, Ordering::Relaxed);
            return;
        }

        let make_code = keyboard.MakeCode;
        let flags = keyboard.Flags;

        if make_code == KEYBOARD_OVERRUN_MAKE_CODE {
            return;
        }

        let e0 = flags & RI_KEY_E0 != 0;
        let e1 = flags & RI_KEY_E1 != 0;

        // "Fake shift": real LShift/RShift never carry E0, so this test is exact.
        if e0 && (make_code == SCANCODE_LSHIFT || make_code == SCANCODE_RSHIFT) {
            return;
        }

        let mut event_flags = InputEventFlags::NONE;
        if flags & RI_KEY_BREAK != 0 {
            event_flags |= InputEventFlags::UP;
        }
        if e0 {
            event_flags |= InputEventFlags::EXTENDED;
        }
        if e1 {
            event_flags |= InputEventFlags::E1;
        }

        self.emit(InputEvent::key(timestamp, make_code, event_flags));
    }

    fn emit(&self, event: InputEvent) {
        self.stats.events.fetch_add(1, Ordering::Relaxed);
        (self.sink)(&event);
    }
}
