//! The pure core of Virtual KM Switch: what an input event is, how time is measured, the lock-free ring
//! the hot path runs on, and the input-engine logic that needs no window, hook or socket.
//!
//! Everything here is testable without a keyboard attached, which is the point of keeping it apart
//! from `vkms-win`: the awkward cases (fake shift, Pause, the NumLock collision, sub-pixel carry)
//! are exactly the ones that are hard to reproduce by hand.

pub mod calibration;
pub mod clock;
pub mod event;
pub mod held_keys;
pub mod hotkeys;
pub mod keyboard;
pub mod spsc;

pub use event::{InputEvent, InputEventFlags, InputEventKind};
