//! Turns a low-level keyboard hook event into a wire [`InputEvent`].
//!
//! Keyboard is captured from the hook rather than raw input because blocking a key in the hook also
//! suppresses its `WM_INPUT` -- measured, not assumed. Nothing is lost by this: the hook carries the
//! scancode, the extended flag, and the virtual key code.

use crate::event::{InputEvent, InputEventFlags, InputEventKind};

// Low-level keyboard hook messages.
pub const WM_KEYDOWN: u32 = 0x0100;
pub const WM_KEYUP: u32 = 0x0101;
pub const WM_SYSKEYDOWN: u32 = 0x0104;
pub const WM_SYSKEYUP: u32 = 0x0105;

/// `KBDLLHOOKSTRUCT.flags`: the scancode carried an E0 prefix.
pub const LLKHF_EXTENDED: u32 = 0x01;

const VK_CANCEL: u16 = 0x03;
const VK_PAUSE: u16 = 0x13;

const SCAN_CODE_LEFT_SHIFT: u16 = 0x2A;
const SCAN_CODE_RIGHT_SHIFT: u16 = 0x36;
const SCAN_CODE_OVERRUN: u16 = 0xFF;

pub fn is_keyboard_message(message: u32) -> bool {
    matches!(message, WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP)
}

pub fn is_key_up_message(message: u32) -> bool {
    matches!(message, WM_KEYUP | WM_SYSKEYUP)
}

/// Translates one hook event, or returns `None` if it must not be forwarded.
pub fn try_translate(
    message: u32,
    virtual_key: u16,
    scan_code: u16,
    hook_flags: u32,
    timestamp: i64,
) -> Option<InputEvent> {
    if !is_keyboard_message(message) {
        return None;
    }

    // Keyboard overrun sentinel: garbage, not a key.
    if scan_code == SCAN_CODE_OVERRUN || virtual_key == SCAN_CODE_OVERRUN {
        return None;
    }

    let is_up = is_key_up_message(message);
    let extended = hook_flags & LLKHF_EXTENDED != 0;

    // "Fake shift": with Shift or NumLock active the driver brackets the grey navigation keys with
    // synthetic shift events. A real LShift/RShift never carries the extended flag, so this test is
    // exact. Forwarding them would leave shift stuck or double-toggled on the target, which
    // synthesises its own from its own modifier state.
    if extended && (scan_code == SCAN_CODE_LEFT_SHIFT || scan_code == SCAN_CODE_RIGHT_SHIFT) {
        return None;
    }

    // Pause and Ctrl+Break cannot be replayed from their scancodes: Pause emits the E1 1D 45
    // sequence and reports no release at all. They are sent as virtual keys instead, and the target
    // synthesises the missing key-up.
    //
    // This is also where the NumLock collision resolves: both report scancode 0x45, but the hook's
    // virtual key tells them apart, so no prefix state machine is needed.
    if virtual_key == VK_PAUSE || virtual_key == VK_CANCEL {
        let mut flags = InputEventFlags::VIRTUAL_KEY;
        if is_up {
            flags |= InputEventFlags::UP;
        }

        return Some(InputEvent::new(timestamp, InputEventKind::KEY, flags, virtual_key, 0, 0));
    }

    let mut flags = InputEventFlags::NONE;
    if is_up {
        flags |= InputEventFlags::UP;
    }

    if extended {
        flags |= InputEventFlags::EXTENDED;
    }

    Some(InputEvent::key(timestamp, scan_code, flags))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_transition_direction_is_tagged_from_the_message() {
        for (message, expect_up) in
            [(WM_KEYDOWN, false), (WM_SYSKEYDOWN, false), (WM_KEYUP, true), (WM_SYSKEYUP, true)]
        {
            let event = try_translate(message, 0x41, 0x1E, 0, 100).unwrap();
            assert_eq!(expect_up, event.is_up());
            assert_eq!(0x1E, event.code);
            assert_eq!(100, event.timestamp);
        }
    }

    #[test]
    fn extended_keys_are_flagged_rather_than_prefixed() {
        // Right Ctrl: same scancode as left, distinguished only by the E0 prefix. The prefix must
        // travel as a flag, never packed into the scancode itself.
        let event = try_translate(WM_KEYDOWN, 0xA3, 0x1D, LLKHF_EXTENDED, 1).unwrap();
        assert_eq!(0x1D, event.code);
        assert!(event.flags.contains(InputEventFlags::EXTENDED));
    }

    #[test]
    fn fake_shift_is_dropped() {
        // The driver brackets grey navigation keys with synthetic shifts when Shift or NumLock is
        // active. Forwarding them leaves shift stuck or double-toggled on the target.
        for scan in [0x2A, 0x36] {
            assert!(try_translate(WM_KEYDOWN, 0xA0, scan, LLKHF_EXTENDED, 1).is_none());
        }
    }

    #[test]
    fn real_shift_is_kept() {
        // A genuine shift never carries the extended flag, which is what makes the filter exact.
        for scan in [0x2A, 0x36] {
            assert_eq!(scan, try_translate(WM_KEYDOWN, 0xA0, scan, 0, 1).unwrap().code);
        }
    }

    #[test]
    fn pause_is_sent_as_virtual_key_because_its_scancode_cannot_be_replayed() {
        let event = try_translate(WM_KEYDOWN, 0x13, 0x45, 0, 1).unwrap();
        assert!(event.flags.contains(InputEventFlags::VIRTUAL_KEY));
        assert_eq!(0x13, event.code);
    }

    #[test]
    fn ctrl_break_is_sent_as_virtual_key() {
        let event = try_translate(WM_KEYDOWN, 0x03, 0x46, LLKHF_EXTENDED, 1).unwrap();
        assert!(event.flags.contains(InputEventFlags::VIRTUAL_KEY));
        assert_eq!(0x03, event.code);
    }

    #[test]
    fn num_lock_is_not_confused_with_pause() {
        // Both report scancode 0x45. Keying on the scancode alone would emit a spurious NumLock
        // toggle on every Pause press; the virtual key is what separates them.
        let num_lock = try_translate(WM_KEYDOWN, 0x90, 0x45, 0, 1).unwrap();
        assert!(!num_lock.flags.contains(InputEventFlags::VIRTUAL_KEY));
        assert_eq!(0x45, num_lock.code);

        let pause = try_translate(WM_KEYDOWN, 0x13, 0x45, 0, 1).unwrap();
        assert!(pause.flags.contains(InputEventFlags::VIRTUAL_KEY));
    }

    #[test]
    fn keyboard_overrun_sentinel_is_dropped() {
        assert!(try_translate(WM_KEYDOWN, 0xFF, 0xFF, 0, 1).is_none());
    }

    #[test]
    fn non_keyboard_messages_are_ignored() {
        assert!(try_translate(0x0200 /* WM_MOUSEMOVE */, 0, 0, 0, 1).is_none());
    }
}
