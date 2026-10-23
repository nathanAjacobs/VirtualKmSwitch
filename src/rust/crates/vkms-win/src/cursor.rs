//! Reads and moves the cursor. Used to measure what injected motion actually produced.

use windows_sys::Win32::Foundation::POINT;
use windows_sys::Win32::UI::WindowsAndMessaging::{GetCursorPos, SetCursorPos};

pub fn position() -> Option<(i32, i32)> {
    let mut point = POINT { x: 0, y: 0 };
    (unsafe { GetCursorPos(&mut point) } != 0).then_some((point.x, point.y))
}

pub fn set_position(x: i32, y: i32) -> bool {
    unsafe { SetCursorPos(x, y) != 0 }
}
