//! Blocking and capture: decides when local input is swallowed and forwarded.
//!
//! The agent does not decide *where* input goes, or even whether this machine is driving another.
//! It matches hotkeys and reports them; the service, which knows which machine each slot means and
//! whether it is reachable, decides; and the agent is told the result. The one exception is panic,
//! which acts locally first and reports afterwards, because it is the way out when something in that
//! loop has gone wrong.
//!
//! Capture comes from two sources, which is a measured constraint rather than a preference. Blocking
//! a key in the hook also suppresses its raw input, so keyboard is read from the hook; mouse motion
//! and buttons survive blocking, so those are read from raw input where the deltas are true and
//! pre-ballistics.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use vkms_core::held_keys::HeldKeyTracker;
use vkms_core::hotkeys::{HotkeyActionKind, HotkeyMatcher};
use vkms_core::{clock, keyboard, InputEvent, InputEventKind};
use vkms_win::focus_decoy::FocusDecoy;
use vkms_win::hook::{HookInputInfo, LowLevelHook};
use vkms_win::injector::InputInjector;
use vkms_win::raw_input::RawInputReader;
use vkms_win::Win32Error;

use crate::health::HookHealthMonitor;

/// Receives captured input and matched hotkeys. Called on the capture threads; must not block.
pub type CapturedSink = Arc<dyn Fn(&InputEvent) + Send + Sync>;

struct Inner {
    sink: CapturedSink,
    matcher: HotkeyMatcher,

    /// What this source has pressed and not yet released, so a transition can let go of it.
    held: HeldKeyTracker,

    /// Whether local input is being swallowed and forwarded. Read by the hook on every event.
    engaged: Arc<AtomicBool>,

    /// Holds the foreground while engaged. Blocking leaves raw mouse input flowing, and a game that
    /// reads it would otherwise keep responding locally for as long as its window is active.
    decoy: FocusDecoy,

    /// Whether the decoy is used at all; a setting of this machine's, applied by the service.
    decoy_enabled: AtomicBool,
}

impl Inner {
    /// Keyboard capture and hotkey matching. Runs on the hook thread, inside its latency budget.
    fn on_hook(&self, info: &HookInputInfo) -> bool {
        let engaged = self.engaged.load(Ordering::Acquire);

        if !keyboard::is_keyboard_message(info.message) {
            // Mouse is captured from raw input; the hook only blocks it.
            return engaged;
        }

        let virtual_key = info.data as u16;
        let (matched, swallow) = self.matcher.process(virtual_key, keyboard::is_key_up_message(info.message));

        if matched.is_match() {
            if matched.kind == HotkeyActionKind::Panic {
                self.panic_locally();
            }

            // Reported, not acted on: the service owns what a slot means. One ring write.
            (self.sink)(&InputEvent::hotkey(clock::ticks(), matched.kind, matched.slot));
            return true;
        }

        if engaged {
            if let Some(event) = keyboard::try_translate(info.message, virtual_key, info.code, info.flags, clock::ticks()) {
                self.held.record(&event);
                (self.sink)(&event);
            }

            return true;
        }

        swallow
    }

    /// Mouse capture. Runs on the raw-input thread.
    fn on_raw(&self, event: &InputEvent) {
        if !self.engaged.load(Ordering::Acquire) {
            // A stand-down that bypasses set_engaged -- a capture ring the service stopped draining --
            // leaves the decoy up; the next local input takes it down. One load when it is already down.
            self.decoy.hide();
            return;
        }

        if event.kind == InputEventKind::KEY {
            return;
        }

        self.held.record(event);
        (self.sink)(event);
    }

    /// Unblocks everything at once, before anything else is attempted.
    ///
    /// The escape hatch: without it, a bug in the blocking logic locks the operator out of their own
    /// machine with no way back.
    fn panic_locally(&self) {
        self.engaged.store(false, Ordering::Release);
        self.decoy.hide();
        self.flush_releases();
        self.matcher.reset();
    }

    /// Sends releases for everything the machine being left believes this one is holding.
    fn flush_releases(&self) {
        let mut releases = [InputEvent::default(); 64];
        let timestamp = clock::ticks();

        loop {
            let count = self.held.build_release_all(timestamp, &mut releases);
            if count == 0 {
                break;
            }

            for release in &releases[..count] {
                (self.sink)(release);
            }
        }

        // A final marker, so a target that missed individual releases still clears this source.
        (self.sink)(&InputEvent::release_all(timestamp));
    }
}

pub struct EngagementController {
    inner: Arc<Inner>,

    // Declared in drop order: the watchdog first, so it never probes a hook being torn down.
    health: Option<HookHealthMonitor>,
    _reader: RawInputReader,
    hook: Arc<LowLevelHook>,
}

impl EngagementController {
    /// Installs the hooks and raw-input capture.
    ///
    /// `engaged` is shared so that whoever feeds the sink can also stand down: an agent whose ring
    /// to the service is full of events it may not drop clears it, which unblocks the machine
    /// without needing anything from a service that has evidently stopped listening.
    ///
    /// `can_inject` gates the hook watchdog, which proves liveness by injecting: on an agent whose
    /// desktop is not showing there is nothing to learn and no point trying.
    pub fn new(
        sink: CapturedSink,
        engaged: Arc<AtomicBool>,
        can_inject: impl Fn() -> bool + Send + 'static,
    ) -> Result<Self, Win32Error> {
        let inner = Arc::new(Inner {
            sink,
            matcher: HotkeyMatcher::default(),
            held: HeldKeyTracker::new(),
            engaged,
            decoy: FocusDecoy::new()?,
            decoy_enabled: AtomicBool::new(true),
        });

        // Hooks first: they must be in place before raw input starts delivering, or the first
        // events after engaging would reach the local machine.
        let for_hook = Arc::clone(&inner);
        let hook = Arc::new(LowLevelHook::new(Some(Box::new(move |info| for_hook.on_hook(info))))?);

        let for_reader = Arc::clone(&inner);
        let reader = RawInputReader::new(Box::new(move |event| for_reader.on_raw(event)))?;

        // The watchdog lives here because this is what owns the hook. A hook that overruns
        // LowLevelHooksTimeout is removed with no notification at all, and the failure is
        // invisible: input silently stops being blocked while the user believes it is.
        let health = HookHealthMonitor::start(Arc::clone(&hook), Duration::from_secs(5), can_inject);

        Ok(Self { inner, health: Some(health), _reader: reader, hook })
    }

    pub fn is_engaged(&self) -> bool {
        self.inner.engaged.load(Ordering::Acquire)
    }

    /// Applies the service's decision about whether this machine is driving another.
    pub fn set_engaged(&self, engaged: bool) {
        let was = self.inner.engaged.swap(engaged, Ordering::AcqRel);
        if was == engaged {
            return;
        }

        if engaged {
            // The chord that engaged us pressed modifiers this machine saw go down. Their key-ups
            // will now be swallowed, so release them here, or this machine thinks Ctrl and Alt are
            // held for as long as it is driving another -- and applies them to any input injected
            // into it meanwhile.
            for virtual_key in self.inner.matcher.held_modifier_keys() {
                InputInjector::release_virtual_key(virtual_key);
            }

            if self.inner.decoy_enabled.load(Ordering::Acquire) {
                self.inner.decoy.show();
            }

            log::info!("blocking local input; it is being forwarded");
        } else {
            self.inner.decoy.hide();
            self.inner.flush_releases();
            log::info!("local input returned to this machine");
        }
    }

    /// Applies the service's setting for whether the focus decoy is used. Takes effect at once, even
    /// while blocking: turning it off hands the foreground back, and turning it on takes it.
    pub fn set_focus_decoy_enabled(&self, enabled: bool) {
        if self.inner.decoy_enabled.swap(enabled, Ordering::AcqRel) == enabled {
            return;
        }

        if !enabled {
            self.inner.decoy.hide();
        } else if self.is_engaged() {
            self.inner.decoy.show();
        }

        log::info!("focus decoy {}", if enabled { "enabled" } else { "disabled" });
    }

    /// Releases everything and unblocks, as the panic hotkey does.
    pub fn panic(&self) {
        log::warn!("PANIC: releasing all input");
        self.inner.panic_locally();
    }

    /// Forgets which modifiers are held. Called when this desktop stops showing, since key-ups
    /// delivered to another desktop are never seen here.
    pub fn reset_modifiers(&self) {
        self.inner.matcher.reset();
    }

    /// Times the hook watchdog found the hooks dead and reinstalled them.
    pub fn hook_reinstalls(&self) -> i64 {
        self.hook.reinstall_count()
    }
}

impl Drop for EngagementController {
    fn drop(&mut self) {
        // Unblocked before anything is torn down: disposing while blocked would leave the machine
        // deaf for as long as it takes the hooks to come out.
        self.inner.engaged.store(false, Ordering::Release);
        self.health.take();
    }
}
