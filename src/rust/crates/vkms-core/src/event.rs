//! The one value that travels the whole input path.

use std::ops::{BitAnd, BitOr, BitOrAssign};

use crate::hotkeys::HotkeyActionKind;

/// What kind of physical input an [`InputEvent`] carries.
///
/// A transparent byte rather than a Rust `enum`, deliberately. Events are copied byte-for-byte out
/// of shared memory and off the network, and an `enum` holding a discriminant nobody declared is
/// undefined behaviour rather than merely an unknown value. Any byte is a valid kind here; code
/// that meets one it does not recognise ignores it.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default, Debug)]
pub struct InputEventKind(pub u8);

impl InputEventKind {
    pub const NONE: Self = Self(0);
    pub const MOUSE_MOVE: Self = Self(1);
    pub const MOUSE_BUTTON: Self = Self(2);
    pub const MOUSE_WHEEL: Self = Self(3);
    pub const KEY: Self = Self(4);

    /// Release every key and button held by the sending peer. Never dropped.
    pub const RELEASE_ALL: Self = Self(5);

    /// A hotkey chord matched by an agent, travelling agent to service only.
    ///
    /// Carried in the same ring as captured input rather than a channel of its own, so it stays
    /// ordered with the keystrokes around it: the release flushed by an engage must not overtake
    /// the chord that caused it. The router consumes it; it never reaches the wire.
    pub const HOTKEY: Self = Self(6);
}

/// Modifier bits describing how to interpret an [`InputEvent`].
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default, Debug)]
pub struct InputEventFlags(pub u8);

impl InputEventFlags {
    pub const NONE: Self = Self(0);

    /// Key or button transition is a release rather than a press.
    pub const UP: Self = Self(1 << 0);

    /// Scancode carried an E0 prefix; replayed via `KEYEVENTF_EXTENDEDKEY`.
    pub const EXTENDED: Self = Self(1 << 1);

    /// Scancode carried an E1 prefix (the Pause/Break sequence).
    pub const E1: Self = Self(1 << 2);

    /// Replay by virtual key instead of scancode. Used for Pause/Break, which cannot be faithfully
    /// reproduced from its scancode sequence.
    pub const VIRTUAL_KEY: Self = Self(1 << 3);

    /// Wheel movement is horizontal.
    pub const HORIZONTAL_WHEEL: Self = Self(1 << 4);

    #[inline]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    #[inline]
    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }
}

impl BitOr for InputEventFlags {
    type Output = Self;

    #[inline]
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for InputEventFlags {
    #[inline]
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl BitAnd for InputEventFlags {
    type Output = Self;

    #[inline]
    fn bitand(self, rhs: Self) -> Self {
        Self(self.0 & rhs.0)
    }
}

/// One captured input event.
///
/// Plain old data, fixed-size and `Copy`, so it can live by value in a lock-free ring buffer --
/// including one mapped across a process boundary -- with no allocation anywhere on the hot path.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct InputEvent {
    /// Raw [`crate::clock`] ticks at capture. Converted to microseconds before it leaves the
    /// machine, because tick frequency is a property of the machine.
    pub timestamp: i64,

    pub kind: InputEventKind,
    pub flags: InputEventFlags,

    /// Scancode for a key, button index for a button, action for a hotkey.
    pub code: u16,

    /// Relative dx for a move, the signed wheel delta, or a hotkey's slot.
    pub x: i32,

    /// Relative dy for a move.
    pub y: i32,
}

impl InputEvent {
    #[inline]
    pub const fn new(
        timestamp: i64,
        kind: InputEventKind,
        flags: InputEventFlags,
        code: u16,
        x: i32,
        y: i32,
    ) -> Self {
        Self { timestamp, kind, flags, code, x, y }
    }

    #[inline]
    pub const fn mouse_move(timestamp: i64, dx: i32, dy: i32) -> Self {
        Self::new(timestamp, InputEventKind::MOUSE_MOVE, InputEventFlags::NONE, 0, dx, dy)
    }

    #[inline]
    pub const fn mouse_button(timestamp: i64, button: u16, up: bool) -> Self {
        let flags = if up { InputEventFlags::UP } else { InputEventFlags::NONE };
        Self::new(timestamp, InputEventKind::MOUSE_BUTTON, flags, button, 0, 0)
    }

    #[inline]
    pub const fn mouse_wheel(timestamp: i64, delta: i32, horizontal: bool) -> Self {
        let flags = if horizontal { InputEventFlags::HORIZONTAL_WHEEL } else { InputEventFlags::NONE };
        Self::new(timestamp, InputEventKind::MOUSE_WHEEL, flags, 0, delta, 0)
    }

    #[inline]
    pub const fn key(timestamp: i64, scan_code: u16, flags: InputEventFlags) -> Self {
        Self::new(timestamp, InputEventKind::KEY, flags, scan_code, 0, 0)
    }

    #[inline]
    pub const fn release_all(timestamp: i64) -> Self {
        Self::new(timestamp, InputEventKind::RELEASE_ALL, InputEventFlags::NONE, 0, 0, 0)
    }

    /// A matched hotkey, on its way from an agent to the service.
    #[inline]
    pub const fn hotkey(timestamp: i64, action: HotkeyActionKind, slot: i32) -> Self {
        Self::new(timestamp, InputEventKind::HOTKEY, InputEventFlags::NONE, action as u16, slot, 0)
    }

    /// The action and slot of a [`InputEventKind::HOTKEY`] event, or `None` for anything else.
    #[inline]
    pub fn hotkey_action(&self) -> Option<(HotkeyActionKind, i32)> {
        if self.kind != InputEventKind::HOTKEY {
            return None;
        }

        HotkeyActionKind::from_code(self.code).map(|action| (action, self.x))
    }

    #[inline]
    pub const fn is_up(&self) -> bool {
        self.flags.contains(InputEventFlags::UP)
    }

    /// Whether this event may be merged with, or dropped in favour of, an adjacent one under
    /// backpressure. Only mouse motion qualifies: dropping a key-up would strand a modifier held
    /// down on a remote machine.
    #[inline]
    pub fn is_coalescible(&self) -> bool {
        self.kind == InputEventKind::MOUSE_MOVE
    }
}
