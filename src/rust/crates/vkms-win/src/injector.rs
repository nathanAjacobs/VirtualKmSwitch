//! Replays input on this machine via `SendInput`.
//!
//! Mouse motion is injected as *relative* movement, which is what games and relative-motion
//! applications expect. Windows applies pointer ballistics to relative motion, so callers pre-shape
//! deltas with `MouseCalibration` rather than having this quietly rewrite them.
//!
//! Keyboard is injected by *scancode*, not virtual key, so machines with different keyboard layouts
//! still receive the key that was physically pressed.

use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_MOVE_NOCOALESCE,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP,
    MOUSEINPUT, MOUSE_EVENT_FLAGS,
};

use vkms_core::{InputEvent, InputEventFlags, InputEventKind};

use crate::INJECTED_SIGNATURE;

// Buttons are indexed as they are on the wire: 0=left, 1=right, 2=middle, 3=X1, 4=X2.
const BUTTON_DOWN: [MOUSE_EVENT_FLAGS; 5] =
    [MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XDOWN];

const BUTTON_UP: [MOUSE_EVENT_FLAGS; 5] =
    [MOUSEEVENTF_LEFTUP, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_XUP, MOUSEEVENTF_XUP];

const XBUTTON1: i32 = 0x0001;
const XBUTTON2: i32 = 0x0002;

#[derive(Default)]
pub struct InputInjector {
    /// Reused so that injecting a batch allocates nothing in steady state.
    batch: Vec<INPUT>,
}

impl InputInjector {
    pub fn new() -> Self {
        Self { batch: Vec::with_capacity(64) }
    }

    /// Injects a batch of events in a single `SendInput` call. Each call is a syscall, so batching
    /// is where the win is. Returns how many records the system accepted.
    pub fn inject(&mut self, events: &[InputEvent]) -> u32 {
        self.batch.clear();
        self.batch.extend(events.iter().filter_map(encode));
        send(&self.batch)
    }

    /// Injects a single event.
    pub fn inject_one(event: &InputEvent) -> u32 {
        match encode(event) {
            Some(input) => send(std::slice::from_ref(&input)),
            None => 0,
        }
    }

    /// Releases a key by virtual key, locally. Used when control moves to another machine while a
    /// modifier is down here, since its key-up will be swallowed while engaged.
    pub fn release_virtual_key(virtual_key: u16) -> u32 {
        send(&[key_input(virtual_key, 0, KEYEVENTF_KEYUP)])
    }
}

fn send(inputs: &[INPUT]) -> u32 {
    if inputs.is_empty() {
        return 0;
    }

    unsafe { SendInput(inputs.len() as u32, inputs.as_ptr(), std::mem::size_of::<INPUT>() as i32) }
}

fn encode(event: &InputEvent) -> Option<INPUT> {
    match event.kind {
        InputEventKind::MOUSE_MOVE => {
            Some(mouse_input(MOUSEEVENTF_MOVE | MOUSEEVENTF_MOVE_NOCOALESCE, event.x, event.y, 0))
        }

        InputEventKind::MOUSE_BUTTON => {
            let index = event.code as usize;
            if index >= BUTTON_DOWN.len() {
                return None;
            }

            let flags = if event.is_up() { BUTTON_UP[index] } else { BUTTON_DOWN[index] };

            // X buttons share one flag and distinguish themselves through mouseData.
            let data = match index {
                3 => XBUTTON1,
                4 => XBUTTON2,
                _ => 0,
            };

            Some(mouse_input(flags, 0, 0, data))
        }

        InputEventKind::MOUSE_WHEEL => {
            let flags = if event.flags.contains(InputEventFlags::HORIZONTAL_WHEEL) {
                MOUSEEVENTF_HWHEEL
            } else {
                MOUSEEVENTF_WHEEL
            };

            Some(mouse_input(flags, 0, 0, event.x))
        }

        InputEventKind::KEY => Some(encode_key(event)),

        _ => None,
    }
}

fn mouse_input(flags: MOUSE_EVENT_FLAGS, dx: i32, dy: i32, data: i32) -> INPUT {
    let mut input: INPUT = unsafe { std::mem::zeroed() };
    input.r#type = INPUT_MOUSE;
    input.Anonymous.mi = MOUSEINPUT {
        dx,
        dy,
        mouseData: data as _,
        dwFlags: flags,
        time: 0,
        // Tags the event as ours so our own hooks and raw input can tell it from physical input.
        dwExtraInfo: INJECTED_SIGNATURE as usize,
    };
    input
}

fn key_input(virtual_key: u16, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    let mut input: INPUT = unsafe { std::mem::zeroed() };
    input.r#type = INPUT_KEYBOARD;
    input.Anonymous.ki = KEYBDINPUT {
        wVk: virtual_key,
        wScan: scan,
        dwFlags: flags,
        time: 0,
        dwExtraInfo: INJECTED_SIGNATURE as usize,
    };
    input
}

fn encode_key(event: &InputEvent) -> INPUT {
    // Pause/Break cannot be reproduced from its scancode sequence: it emits e1 1d 45 and never
    // reports a release. Such keys are marked at capture and replayed by virtual key instead.
    if event.flags.contains(InputEventFlags::VIRTUAL_KEY) {
        let flags = if event.is_up() { KEYEVENTF_KEYUP } else { 0 };
        return key_input(event.code, 0, flags);
    }

    let mut flags = KEYEVENTF_SCANCODE;

    // The E0 prefix is expressed through EXTENDEDKEY; it is never packed into wScan.
    if event.flags.contains(InputEventFlags::EXTENDED) {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }

    if event.is_up() {
        flags |= KEYEVENTF_KEYUP;
    }

    key_input(0, event.code, flags)
}
