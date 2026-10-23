//! Remembers every key and button a single source currently holds down, so they can all be
//! released when that source stops driving a machine.
//!
//! This is the fix for the most common software-KVM bug: switch away mid-chord and Ctrl or Shift
//! stays stuck down on the machine you left, with no way to clear it from there.
//!
//! One instance per *source*, deliberately. Several peers can drive the same machine at once, and
//! releasing one source's Shift must not clear another's.
//!
//! Fixed-size atomic arrays rather than a set: this runs on the capture path, where allocation and
//! locks are forbidden, and it is touched from more than one thread -- the hook thread records
//! keys, the raw-input thread records buttons, and a transition flushes them. The key space is
//! small enough to index directly.

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use crate::event::{InputEvent, InputEventFlags, InputEventKind};

/// Scancodes are a byte, doubled to give the E0-extended variants their own slots.
const SCAN_CODE_SLOTS: usize = 512;
const VIRTUAL_KEY_SLOTS: usize = 256;
const BUTTON_SLOTS: usize = 8;

pub struct HeldKeyTracker {
    scan_codes: [AtomicBool; SCAN_CODE_SLOTS],
    virtual_keys: [AtomicBool; VIRTUAL_KEY_SLOTS],
    buttons: [AtomicBool; BUTTON_SLOTS],
    held: AtomicI32,
}

impl Default for HeldKeyTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl HeldKeyTracker {
    /// The maximum number of events [`Self::build_release_all`] can produce.
    pub const MAX_RELEASE_EVENTS: usize = SCAN_CODE_SLOTS + VIRTUAL_KEY_SLOTS + BUTTON_SLOTS;

    pub const fn new() -> Self {
        Self {
            scan_codes: [const { AtomicBool::new(false) }; SCAN_CODE_SLOTS],
            virtual_keys: [const { AtomicBool::new(false) }; VIRTUAL_KEY_SLOTS],
            buttons: [const { AtomicBool::new(false) }; BUTTON_SLOTS],
            held: AtomicI32::new(0),
        }
    }

    /// How many keys and buttons are currently held.
    pub fn held_count(&self) -> i32 {
        self.held.load(Ordering::Relaxed)
    }

    /// Updates the held set from one forwarded event. Ignores anything not stateful.
    pub fn record(&self, event: &InputEvent) {
        match event.kind {
            InputEventKind::KEY => {
                if event.flags.contains(InputEventFlags::VIRTUAL_KEY) {
                    self.set(&self.virtual_keys, event.code as usize, !event.is_up());
                } else {
                    self.set(&self.scan_codes, scan_code_index(event), !event.is_up());
                }
            }

            InputEventKind::MOUSE_BUTTON => {
                self.set(&self.buttons, event.code as usize, !event.is_up());
            }

            // Motion and wheel carry no state to release.
            _ => {}
        }
    }

    /// Writes the releases needed to return this source to a clean state, clearing each slot as
    /// it is written. Anything that did not fit stays held, so calling again releases the rest.
    pub fn build_release_all(&self, timestamp: i64, destination: &mut [InputEvent]) -> usize {
        let mut count = 0;

        for (index, slot) in self.scan_codes.iter().enumerate() {
            if count == destination.len() {
                return count;
            }

            if slot.swap(false, Ordering::AcqRel) {
                let mut flags = InputEventFlags::UP;
                if index >= 256 {
                    flags |= InputEventFlags::EXTENDED;
                }

                destination[count] = InputEvent::key(timestamp, (index & 0xFF) as u16, flags);
                count += 1;
                self.held.fetch_sub(1, Ordering::Relaxed);
            }
        }

        for (index, slot) in self.virtual_keys.iter().enumerate() {
            if count == destination.len() {
                return count;
            }

            if slot.swap(false, Ordering::AcqRel) {
                destination[count] = InputEvent::new(
                    timestamp,
                    InputEventKind::KEY,
                    InputEventFlags::VIRTUAL_KEY | InputEventFlags::UP,
                    index as u16,
                    0,
                    0,
                );
                count += 1;
                self.held.fetch_sub(1, Ordering::Relaxed);
            }
        }

        for (index, slot) in self.buttons.iter().enumerate() {
            if count == destination.len() {
                return count;
            }

            if slot.swap(false, Ordering::AcqRel) {
                destination[count] = InputEvent::mouse_button(timestamp, index as u16, true);
                count += 1;
                self.held.fetch_sub(1, Ordering::Relaxed);
            }
        }

        count
    }

    /// Forgets everything without emitting releases.
    pub fn clear(&self) {
        for slot in self.scan_codes.iter().chain(&self.virtual_keys).chain(&self.buttons) {
            slot.store(false, Ordering::Relaxed);
        }

        self.held.store(0, Ordering::Relaxed);
    }

    fn set(&self, slots: &[AtomicBool], index: usize, held: bool) {
        let Some(slot) = slots.get(index) else {
            return;
        };

        // Only a real transition moves the count, so auto-repeat's stream of key-downs counts once.
        if slot.swap(held, Ordering::AcqRel) != held {
            self.held.fetch_add(if held { 1 } else { -1 }, Ordering::Relaxed);
        }
    }
}

fn scan_code_index(event: &InputEvent) -> usize {
    let extended = if event.flags.contains(InputEventFlags::EXTENDED) { 256 } else { 0 };
    (event.code as usize & 0xFF) | extended
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCAN_LEFT_SHIFT: u16 = 0x2A;
    const SCAN_A: u16 = 0x1E;
    const SCAN_RIGHT_CTRL: u16 = 0x1D;

    fn key_down(scan: u16, flags: InputEventFlags) -> InputEvent {
        InputEvent::key(1, scan, flags)
    }

    fn key_up(scan: u16, flags: InputEventFlags) -> InputEvent {
        InputEvent::key(1, scan, flags | InputEventFlags::UP)
    }

    fn releases() -> Vec<InputEvent> {
        vec![InputEvent::default(); HeldKeyTracker::MAX_RELEASE_EVENTS]
    }

    #[test]
    fn held_keys_are_released_and_the_set_is_cleared() {
        let tracker = HeldKeyTracker::new();
        tracker.record(&key_down(SCAN_LEFT_SHIFT, InputEventFlags::NONE));
        tracker.record(&key_down(SCAN_A, InputEventFlags::NONE));
        assert_eq!(2, tracker.held_count());

        let mut out = releases();
        let count = tracker.build_release_all(99, &mut out);

        assert_eq!(2, count);
        assert!(out[..count].iter().all(|e| e.is_up() && e.timestamp == 99));
        assert_eq!(0, tracker.held_count());

        // A second call has nothing left to release.
        assert_eq!(0, tracker.build_release_all(100, &mut out));
    }

    #[test]
    fn released_keys_are_no_longer_held() {
        let tracker = HeldKeyTracker::new();
        tracker.record(&key_down(SCAN_A, InputEventFlags::NONE));
        tracker.record(&key_up(SCAN_A, InputEventFlags::NONE));

        assert_eq!(0, tracker.held_count());
        assert_eq!(0, tracker.build_release_all(1, &mut releases()));
    }

    #[test]
    fn extended_keys_are_tracked_separately_from_their_base_scancode() {
        // Right Ctrl and left Ctrl share scancode 0x1D and differ only by the extended flag.
        // Collapsing them would release the wrong key, or miss one entirely.
        let tracker = HeldKeyTracker::new();
        tracker.record(&key_down(SCAN_RIGHT_CTRL, InputEventFlags::EXTENDED));
        tracker.record(&key_down(SCAN_RIGHT_CTRL, InputEventFlags::NONE));
        assert_eq!(2, tracker.held_count());

        let mut out = releases();
        let count = tracker.build_release_all(1, &mut out);
        assert_eq!(2, count);
        assert!(out[..count].iter().any(|e| e.flags.contains(InputEventFlags::EXTENDED)));
        assert!(out[..count].iter().any(|e| !e.flags.contains(InputEventFlags::EXTENDED)));
    }

    #[test]
    fn virtual_keys_are_released_as_virtual_keys() {
        // Pause travels as a virtual key because its scancode sequence cannot be replayed, so its
        // release has to go back the same way.
        let tracker = HeldKeyTracker::new();
        tracker.record(&InputEvent::new(1, InputEventKind::KEY, InputEventFlags::VIRTUAL_KEY, 0x13, 0, 0));

        let mut out = releases();
        assert_eq!(1, tracker.build_release_all(1, &mut out));
        assert!(out[0].flags.contains(InputEventFlags::VIRTUAL_KEY));
        assert!(out[0].is_up());
        assert_eq!(0x13, out[0].code);
    }

    #[test]
    fn mouse_buttons_are_released_too() {
        let tracker = HeldKeyTracker::new();
        tracker.record(&InputEvent::mouse_button(1, 0, false));
        tracker.record(&InputEvent::mouse_button(1, 2, false));

        let mut out = releases();
        let count = tracker.build_release_all(1, &mut out);
        assert_eq!(2, count);
        assert!(out[..count].iter().all(|e| e.kind == InputEventKind::MOUSE_BUTTON && e.is_up()));
    }

    #[test]
    fn motion_and_wheel_carry_no_state_to_release() {
        let tracker = HeldKeyTracker::new();
        tracker.record(&InputEvent::mouse_move(1, 10, 10));
        tracker.record(&InputEvent::mouse_wheel(1, 120, false));
        assert_eq!(0, tracker.held_count());
    }

    #[test]
    fn repeated_key_down_counts_once() {
        // Auto-repeat sends key-down many times without an intervening key-up.
        let tracker = HeldKeyTracker::new();
        for _ in 0..50 {
            tracker.record(&key_down(SCAN_A, InputEventFlags::NONE));
        }

        assert_eq!(1, tracker.held_count());
    }

    #[test]
    fn trackers_are_independent_per_source() {
        // Two peers may drive one machine at once; releasing one peer's Shift must not clear the
        // other's, or the second peer's typing silently loses its modifier.
        let from_a = HeldKeyTracker::new();
        let from_b = HeldKeyTracker::new();
        from_a.record(&key_down(SCAN_LEFT_SHIFT, InputEventFlags::NONE));
        from_b.record(&key_down(SCAN_LEFT_SHIFT, InputEventFlags::NONE));

        from_a.build_release_all(1, &mut releases());

        assert_eq!(0, from_a.held_count());
        assert_eq!(1, from_b.held_count());
    }

    #[test]
    fn a_small_destination_releases_the_rest_on_the_next_call() {
        let tracker = HeldKeyTracker::new();
        for scan in 1..=5u16 {
            tracker.record(&key_down(scan, InputEventFlags::NONE));
        }

        let mut small = [InputEvent::default(); 2];
        let mut total = 0;
        loop {
            let count = tracker.build_release_all(1, &mut small);
            if count == 0 {
                break;
            }
            total += count;
        }

        assert_eq!(5, total);
        assert_eq!(0, tracker.held_count());
    }

    #[test]
    fn clear_forgets_without_emitting_releases() {
        let tracker = HeldKeyTracker::new();
        tracker.record(&key_down(SCAN_A, InputEventFlags::NONE));
        tracker.clear();
        assert_eq!(0, tracker.build_release_all(1, &mut releases()));
    }
}
