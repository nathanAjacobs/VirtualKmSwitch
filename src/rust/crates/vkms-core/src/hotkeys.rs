//! Matches hotkey chords from low-level keyboard events.
//!
//! This has to happen inside the hook rather than through `RegisterHotKey`: low-level hooks run
//! *before* hotkey processing, so once this machine starts swallowing keys a registered hotkey would
//! never fire -- and it must keep working precisely while everything is being swallowed.
//!
//! Runs in the hook callback, so it allocates nothing, takes no lock, and does little but compare
//! integers. Its state is atomic because a transition on another thread may reset it.

use std::ops::BitOr;
use std::sync::atomic::{AtomicU16, AtomicU32, Ordering};

/// Modifier keys held down, with left and right treated as equivalent for matching.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default, Debug)]
pub struct Modifiers(pub u8);

impl Modifiers {
    pub const NONE: Self = Self(0);
    pub const CONTROL: Self = Self(1 << 0);
    pub const ALT: Self = Self(1 << 1);
    pub const SHIFT: Self = Self(1 << 2);
    pub const WINDOWS: Self = Self(1 << 3);

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

impl BitOr for Modifiers {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

/// What a matched hotkey asks for.
#[repr(u16)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum HotkeyActionKind {
    None = 0,

    /// Drive the machine bound to the slot in *this* machine's own map.
    Engage = 1,

    /// Stop driving anything; return control to this machine.
    ReturnLocal = 2,

    /// Release everything and unblock, unconditionally.
    Panic = 3,
}

impl HotkeyActionKind {
    pub const fn from_code(code: u16) -> Option<Self> {
        match code {
            1 => Some(Self::Engage),
            2 => Some(Self::ReturnLocal),
            3 => Some(Self::Panic),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct HotkeyMatch {
    pub kind: HotkeyActionKind,
    pub slot: i32,
}

impl HotkeyMatch {
    pub const NONE: Self = Self { kind: HotkeyActionKind::None, slot: 0 };

    pub fn is_match(&self) -> bool {
        self.kind != HotkeyActionKind::None
    }
}

/// One configured chord.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct HotkeyBinding {
    pub modifiers: Modifiers,
    pub virtual_key: u16,
    pub kind: HotkeyActionKind,
    pub slot: i32,
}

// Modifier virtual keys are tracked individually, so releasing one of a pair does not clear the
// modifier while the other is still held.
const VK_SHIFT: u16 = 0x10;
const VK_CONTROL: u16 = 0x11;
const VK_MENU: u16 = 0x12;
pub const VK_LSHIFT: u16 = 0xA0;
pub const VK_RSHIFT: u16 = 0xA1;
pub const VK_LCONTROL: u16 = 0xA2;
pub const VK_RCONTROL: u16 = 0xA3;
pub const VK_LMENU: u16 = 0xA4;
pub const VK_RMENU: u16 = 0xA5;
pub const VK_LWIN: u16 = 0x5B;
pub const VK_RWIN: u16 = 0x5C;

/// Every modifier key the matcher tracks, in bit order.
pub const MODIFIER_KEYS: [u16; 8] =
    [VK_LCONTROL, VK_RCONTROL, VK_LMENU, VK_RMENU, VK_LSHIFT, VK_RSHIFT, VK_LWIN, VK_RWIN];

pub struct HotkeyMatcher {
    bindings: Vec<HotkeyBinding>,

    /// One bit per tracked modifier key, so pairs release independently.
    modifier_keys: AtomicU32,

    /// Trigger key whose press was consumed by a chord. Its release must be consumed too, or the
    /// application underneath sees a key-up with no matching key-down.
    swallowed_key: AtomicU16,
}

impl Default for HotkeyMatcher {
    fn default() -> Self {
        Self::new(default_bindings())
    }
}

/// The fixed chords.
///
/// `Ctrl+Alt+0` returns control locally rather than engaging a tenth machine. Panic deliberately
/// avoids Pause, whose scancode sequence reports no key-up at all.
pub fn default_bindings() -> Vec<HotkeyBinding> {
    let ctrl_alt = Modifiers::CONTROL | Modifiers::ALT;

    let mut bindings = vec![HotkeyBinding {
        modifiers: ctrl_alt,
        virtual_key: 0x30,
        kind: HotkeyActionKind::ReturnLocal,
        slot: 0,
    }];

    for slot in 1..=9 {
        bindings.push(HotkeyBinding {
            modifiers: ctrl_alt,
            virtual_key: 0x30 + slot as u16,
            kind: HotkeyActionKind::Engage,
            slot,
        });
    }

    bindings.push(HotkeyBinding {
        modifiers: ctrl_alt | Modifiers::SHIFT,
        virtual_key: 0x1B,
        kind: HotkeyActionKind::Panic,
        slot: 0,
    });

    bindings
}

impl HotkeyMatcher {
    pub fn new(bindings: Vec<HotkeyBinding>) -> Self {
        Self { bindings, modifier_keys: AtomicU32::new(0), swallowed_key: AtomicU16::new(0) }
    }

    /// Modifiers currently held.
    pub fn current_modifiers(&self) -> Modifiers {
        modifiers_from_bits(self.modifier_keys.load(Ordering::Relaxed))
    }

    /// Modifier keys currently held, as virtual keys. Used to release them locally when control
    /// moves away, since their key-ups will be swallowed while engaged.
    pub fn held_modifier_keys(&self) -> impl Iterator<Item = u16> {
        let bits = self.modifier_keys.load(Ordering::Relaxed);
        MODIFIER_KEYS.into_iter().filter(move |&vk| bits & bit(vk) != 0)
    }

    /// Feeds one keyboard event through the matcher.
    ///
    /// The second value says whether the event must be withheld from the rest of the system: true
    /// for a matched chord's trigger key and for its release, so the local machine never sees it.
    pub fn process(&self, virtual_key: u16, is_up: bool) -> (HotkeyMatch, bool) {
        if let Some(modifier) = modifier_bit(virtual_key) {
            if is_up {
                self.modifier_keys.fetch_and(!modifier, Ordering::Relaxed);
            } else {
                self.modifier_keys.fetch_or(modifier, Ordering::Relaxed);
            }

            // Modifiers always pass through: swallowing them would break every ordinary shortcut
            // on the machine while merely idling.
            return (HotkeyMatch::NONE, false);
        }

        if is_up {
            let swallow = virtual_key != 0
                && self
                    .swallowed_key
                    .compare_exchange(virtual_key, 0, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok();

            return (HotkeyMatch::NONE, swallow);
        }

        let modifiers = self.current_modifiers();
        for binding in &self.bindings {
            if binding.virtual_key != virtual_key || binding.modifiers != modifiers {
                continue;
            }

            // Consume the press, and remember to consume the matching release.
            self.swallowed_key.store(virtual_key, Ordering::Relaxed);
            return (HotkeyMatch { kind: binding.kind, slot: binding.slot }, true);
        }

        (HotkeyMatch::NONE, false)
    }

    /// Forgets modifier state, so a modifier whose key-up was never seen -- delivered to another
    /// desktop, or lost when hooks were reinstalled -- is not remembered as held forever.
    pub fn reset(&self) {
        self.modifier_keys.store(0, Ordering::Relaxed);
        self.swallowed_key.store(0, Ordering::Relaxed);
    }
}

fn modifier_bit(virtual_key: u16) -> Option<u32> {
    // Generic VK_CONTROL / VK_MENU / VK_SHIFT fold onto their left variants: low-level hooks
    // normally report the specific side, but not every driver does.
    let normalised = match virtual_key {
        VK_CONTROL => VK_LCONTROL,
        VK_MENU => VK_LMENU,
        VK_SHIFT => VK_LSHIFT,
        other => other,
    };

    let value = bit(normalised);
    (value != 0).then_some(value)
}

const fn bit(modifier_virtual_key: u16) -> u32 {
    match modifier_virtual_key {
        VK_LCONTROL => 1 << 0,
        VK_RCONTROL => 1 << 1,
        VK_LMENU => 1 << 2,
        VK_RMENU => 1 << 3,
        VK_LSHIFT => 1 << 4,
        VK_RSHIFT => 1 << 5,
        VK_LWIN => 1 << 6,
        VK_RWIN => 1 << 7,
        _ => 0,
    }
}

fn modifiers_from_bits(bits: u32) -> Modifiers {
    let mut modifiers = 0u8;

    if bits & (bit(VK_LCONTROL) | bit(VK_RCONTROL)) != 0 {
        modifiers |= Modifiers::CONTROL.0;
    }

    if bits & (bit(VK_LMENU) | bit(VK_RMENU)) != 0 {
        modifiers |= Modifiers::ALT.0;
    }

    if bits & (bit(VK_LSHIFT) | bit(VK_RSHIFT)) != 0 {
        modifiers |= Modifiers::SHIFT.0;
    }

    if bits & (bit(VK_LWIN) | bit(VK_RWIN)) != 0 {
        modifiers |= Modifiers::WINDOWS.0;
    }

    Modifiers(modifiers)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LCTRL: u16 = 0xA2;
    const RCTRL: u16 = 0xA3;
    const LALT: u16 = 0xA4;
    const LSHIFT: u16 = 0xA0;
    const KEY1: u16 = 0x31;
    const KEY0: u16 = 0x30;
    const HOME: u16 = 0x24;
    const ESCAPE: u16 = 0x1B;
    const KEY_A: u16 = 0x41;

    fn press(matcher: &HotkeyMatcher, key: u16) -> (HotkeyMatch, bool) {
        matcher.process(key, false)
    }

    fn hold(matcher: &HotkeyMatcher, keys: &[u16]) {
        for &key in keys {
            matcher.process(key, false);
        }
    }

    #[test]
    fn ctrl_alt_digit_engages_the_matching_slot() {
        let matcher = HotkeyMatcher::default();
        hold(&matcher, &[LCTRL, LALT]);

        let (found, swallow) = press(&matcher, KEY1);

        assert_eq!(HotkeyActionKind::Engage, found.kind);
        assert_eq!(1, found.slot);
        assert!(swallow, "the trigger key must not reach the local machine");
    }

    #[test]
    fn ctrl_alt_zero_returns_control_locally() {
        let matcher = HotkeyMatcher::default();
        hold(&matcher, &[LCTRL, LALT]);
        assert_eq!(HotkeyActionKind::ReturnLocal, press(&matcher, KEY0).0.kind);
    }

    #[test]
    fn ctrl_alt_home_is_no_longer_a_hotkey() {
        // There is no host role to claim, so the chord is left to the applications underneath.
        let matcher = HotkeyMatcher::default();
        hold(&matcher, &[LCTRL, LALT]);

        let (found, swallow) = press(&matcher, HOME);
        assert!(!found.is_match());
        assert!(!swallow);
    }

    #[test]
    fn panic_requires_shift_as_well() {
        let matcher = HotkeyMatcher::default();

        // Ctrl+Alt+Esc is not the panic chord and must not fire it.
        hold(&matcher, &[LCTRL, LALT]);
        assert!(!press(&matcher, ESCAPE).0.is_match());

        hold(&matcher, &[LSHIFT]);
        assert_eq!(HotkeyActionKind::Panic, press(&matcher, ESCAPE).0.kind);
    }

    #[test]
    fn either_side_of_a_modifier_pair_counts() {
        let matcher = HotkeyMatcher::default();
        hold(&matcher, &[RCTRL, LALT]);
        assert_eq!(HotkeyActionKind::Engage, press(&matcher, KEY1).0.kind);
    }

    #[test]
    fn releasing_one_of_a_pair_keeps_the_modifier_held() {
        // Holding both controls and releasing one must not clear Control, or a chord typed with
        // both hands would silently stop matching.
        let matcher = HotkeyMatcher::default();
        hold(&matcher, &[LCTRL, RCTRL, LALT]);
        matcher.process(LCTRL, true);

        assert!(matcher.current_modifiers().contains(Modifiers::CONTROL));
        assert_eq!(HotkeyActionKind::Engage, press(&matcher, KEY1).0.kind);
    }

    #[test]
    fn extra_modifiers_prevent_a_match() {
        // Ctrl+Alt+Shift+1 is not Ctrl+Alt+1. Matching it would hijack chords belonging to other
        // applications.
        let matcher = HotkeyMatcher::default();
        hold(&matcher, &[LCTRL, LALT, LSHIFT]);

        let (found, swallow) = press(&matcher, KEY1);
        assert!(!found.is_match());
        assert!(!swallow);
    }

    #[test]
    fn plain_keys_are_neither_matched_nor_swallowed() {
        let matcher = HotkeyMatcher::default();
        let (found, swallow) = press(&matcher, KEY_A);
        assert!(!found.is_match());
        assert!(!swallow);

        hold(&matcher, &[LCTRL, LALT]);
        let (found, swallow) = press(&matcher, KEY_A);
        assert!(!found.is_match());
        assert!(!swallow);
    }

    #[test]
    fn modifier_keys_themselves_pass_through() {
        // Swallowing modifiers would break every ordinary shortcut on the machine while idle.
        let matcher = HotkeyMatcher::default();
        assert!(!press(&matcher, LCTRL).1);
        assert!(!press(&matcher, LALT).1);
    }

    #[test]
    fn release_of_a_swallowed_trigger_is_also_swallowed() {
        // Otherwise the application underneath sees a key-up with no matching key-down, which some
        // apps treat as a stuck key.
        let matcher = HotkeyMatcher::default();
        hold(&matcher, &[LCTRL, LALT]);
        press(&matcher, KEY1);

        assert!(matcher.process(KEY1, true).1);

        // Only the one release is consumed, not every subsequent release of that key.
        assert!(!matcher.process(KEY1, true).1);
    }

    #[test]
    fn reset_forgets_held_modifiers() {
        let matcher = HotkeyMatcher::default();
        hold(&matcher, &[LCTRL, LALT]);
        matcher.reset();

        assert_eq!(Modifiers::NONE, matcher.current_modifiers());
        assert!(!press(&matcher, KEY1).0.is_match());
    }

    #[test]
    fn held_modifier_keys_lists_exactly_what_is_down() {
        let matcher = HotkeyMatcher::default();
        hold(&matcher, &[LCTRL, LALT]);

        let held: Vec<u16> = matcher.held_modifier_keys().collect();
        assert_eq!(vec![VK_LCONTROL, VK_LMENU], held);
    }

    #[test]
    fn a_hotkey_survives_the_trip_through_an_input_event() {
        let event = crate::InputEvent::hotkey(5, HotkeyActionKind::Engage, 3);
        assert_eq!(Some((HotkeyActionKind::Engage, 3)), event.hotkey_action());
        assert_eq!(None, crate::InputEvent::mouse_move(1, 1, 1).hotkey_action());
    }
}
