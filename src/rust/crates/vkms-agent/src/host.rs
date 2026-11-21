//! One agent, bound for its whole life to the desktop it was created on.
//!
//! Two of these run per session -- one on Default, one on Winlogon -- because a process may only
//! inject on the desktop it was created on. `SetThreadDesktop` moves a thread there successfully and
//! `SendInput` is then still denied, which was measured rather than assumed.
//!
//! Both agents see the same stream from the service, so each gates itself on whether its desktop
//! currently owns input. Without that gate, capture would be duplicated and injection attempted
//! from the wrong desktop.

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use vkms_core::calibration::MouseCalibration;
use vkms_core::{clock, InputEvent, InputEventKind};
use vkms_ipc::AgentChannel;
use vkms_win::injector::InputInjector;
use vkms_win::pointer::{self, PointerSettingsMonitor};
use vkms_win::{desktop, Win32Error};

use crate::engagement::EngagementController;
use crate::probe;

/// How long after this desktop starts showing to wait before injecting.
///
/// The desktop becomes Winlogon before consent.exe has created and focused its dialog, and input
/// delivered in that window is silently discarded -- `SendInput` still reports success. Measured:
/// 5 ms was swallowed, 750 ms worked. Input arriving inside the window is dropped rather than
/// injected into nothing.
const DESKTOP_SETTLE: Duration = Duration::from_millis(750);

struct Shared {
    desktop: String,
    channel: Arc<AgentChannel>,
    stop: AtomicBool,

    /// Whether this agent's desktop currently owns input.
    desktop_active: AtomicBool,

    /// When it last became active, in clock ticks; zero while inactive.
    active_since: AtomicI64,

    calibration: Mutex<MouseCalibration>,
    injected: AtomicI64,
    captured: AtomicI64,
    discarded_while_inactive: AtomicI64,
}

pub struct AgentHost {
    shared: Arc<Shared>,
    controller: Option<Arc<EngagementController>>,
    pointer: Option<PointerSettingsMonitor>,
    threads: Vec<JoinHandle<()>>,
}

impl AgentHost {
    pub fn start(desktop: &str, channel: AgentChannel) -> Result<Self, Win32Error> {
        log::info!("agent starting on desktop '{desktop}'");

        let shared = Arc::new(Shared {
            desktop: desktop.to_owned(),
            channel: Arc::new(channel),
            stop: AtomicBool::new(false),
            desktop_active: AtomicBool::new(false),
            active_since: AtomicI64::new(0),
            calibration: Mutex::new(MouseCalibration::default()),
            injected: AtomicI64::new(0),
            captured: AtomicI64::new(0),
            discarded_while_inactive: AtomicI64::new(0),
        });

        let engaged = Arc::new(AtomicBool::new(false));

        let for_sink = Arc::clone(&shared);
        let stand_down = Arc::clone(&engaged);
        let for_probe = Arc::clone(&shared);

        let controller = Arc::new(EngagementController::new(
            Arc::new(move |event| for_sink.on_captured(event, &stand_down)),
            engaged,
            // The watchdog probes only while this desktop is showing: injection is refused
            // otherwise, which would look identical to a dead hook.
            move || for_probe.desktop_active.load(Ordering::Acquire),
        )?);

        let for_pointer = Arc::clone(&shared);
        let for_pointer_controller = Arc::downgrade(&controller);
        let pointer = PointerSettingsMonitor::new(Duration::from_secs(3), move |settings| {
            log::info!(
                "pointer settings changed (speed {}, precision {}); recalibrating",
                settings.speed,
                if pointer::is_pointer_precision_enabled(&settings) { "on" } else { "off" }
            );

            let idle = for_pointer_controller.upgrade().is_some_and(|controller| !controller.is_engaged());
            for_pointer.recalibrate(settings, idle);
        });

        shared.recalibrate(pointer.current(), false);

        let threads = vec![
            spawn("DesktopGate", {
                let shared = Arc::clone(&shared);
                let controller = Arc::clone(&controller);
                move || shared.watch_desktop(&controller)
            })?,
            spawn("Injection", {
                let shared = Arc::clone(&shared);
                let controller = Arc::clone(&controller);
                move || shared.drain_injection_ring(&controller)
            })?,
        ];

        Ok(Self { shared, controller: Some(controller), pointer: Some(pointer), threads })
    }

    pub fn injected_count(&self) -> i64 {
        self.shared.injected.load(Ordering::Relaxed)
    }

    pub fn captured_count(&self) -> i64 {
        self.shared.captured.load(Ordering::Relaxed)
    }
}

impl Drop for AgentHost {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);

        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }

        self.pointer.take();
        self.controller.take();
    }
}

fn spawn(name: &str, body: impl FnOnce() + Send + 'static) -> Result<JoinHandle<()>, Win32Error> {
    std::thread::Builder::new()
        .name(format!("VirtualKmSwitch.Agent.{name}"))
        .spawn(move || {
            vkms_win::thread::raise_current_priority();
            body();
        })
        .map_err(|error| Win32Error::with_code(0, format!("could not start the {name} thread: {error}")))
}

impl Shared {
    /// Captured local input on its way to the service. Runs on the capture threads.
    fn on_captured(&self, event: &InputEvent, engaged: &AtomicBool) {
        // Both agents observe input on their own desktop only, but a hotkey matched while switching
        // desktops could otherwise be reported twice.
        if !self.desktop_active.load(Ordering::Acquire) {
            return;
        }

        if self.channel.to_service.try_write(event) {
            self.captured.fetch_add(1, Ordering::Relaxed);
        } else if !event.is_coalescible() {
            // Motion may be dropped under pressure; a key or button never may, because a lost key-up
            // strands a modifier held down on a remote machine. A ring this full means the service
            // has stopped draining it, so stop blocking rather than wait to be told.
            log::error!("capture ring full while dropping a non-coalescible event -- unblocking local input");
            engaged.store(false, Ordering::Release);
        }

        self.channel.to_service.signal_data_available();
    }

    /// Tracks whether this agent's desktop currently owns input.
    ///
    /// Reads the name only; never attaches, because attaching would move the thread off the desktop
    /// this process was created on and injection would start failing.
    fn watch_desktop(&self, controller: &EngagementController) {
        let mut previous = String::new();

        while !self.stop.load(Ordering::Acquire) {
            let active = desktop::input_desktop_name();
            let mine = active.eq_ignore_ascii_case(&self.desktop);

            if active != previous {
                log::info!("input desktop is now '{active}' (this agent owns '{}')", self.desktop);
                previous = active;
            }

            if mine != self.desktop_active.load(Ordering::Acquire) {
                if mine {
                    self.active_since.store(clock::ticks(), Ordering::Release);
                } else {
                    self.active_since.store(0, Ordering::Release);

                    // Anything queued for a desktop that is no longer showing is stale by the time
                    // it comes back, and would land in whatever has focus then. Key-ups delivered to
                    // the other desktop are never seen here, so held modifiers are forgotten too.
                    self.channel.to_agent.discard_pending();
                    controller.reset_modifiers();
                }

                self.desktop_active.store(mine, Ordering::Release);
            }

            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Follows the service's engaged state, and replays inbound input while this desktop is showing.
    fn drain_injection_ring(&self, controller: &EngagementController) {
        let mut injector = InputInjector::new();
        let mut batch = [InputEvent::default(); 256];
        let mut shaped = [InputEvent::default(); MouseCalibration::MAX_CHUNKS_PER_EVENT];

        // Applied only when the service's state *changes*. A local stand-down -- panic, or a capture
        // ring the service stopped draining -- must hold until the service says otherwise, not be
        // overridden a moment later by a stale "engaged" from a service that may be wedged.
        let mut last_engaged = false;

        // A setting rather than a decision, but carried the same way and applied the same way: on
        // change. The controller starts with it on, which is also what an all-zero word means.
        let mut last_focus_decoy = true;

        while !self.stop.load(Ordering::Acquire) {
            self.channel.to_agent.wait_for_data(Duration::from_millis(100));

            let engaged = self.channel.engaged();
            if engaged != last_engaged {
                controller.set_engaged(engaged);
                last_engaged = engaged;
            }

            let focus_decoy = self.channel.focus_decoy_enabled();
            if focus_decoy != last_focus_decoy {
                controller.set_focus_decoy_enabled(focus_decoy);
                last_focus_decoy = focus_decoy;
            }

            loop {
                let count = self.channel.to_agent.read_batch(&mut batch);
                if count == 0 {
                    break;
                }

                if !self.settled() {
                    self.discarded_while_inactive.fetch_add(count as i64, Ordering::Relaxed);
                    continue;
                }

                for event in &batch[..count] {
                    if event.kind == InputEventKind::MOUSE_MOVE {
                        // Calibration is applied here, on the machine that knows its own pointer
                        // settings, rather than by the sender guessing them.
                        let chunks = self
                            .calibration
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .prepare(event.timestamp, event.x, event.y, &mut shaped);

                        if chunks > 0 {
                            injector.inject(&shaped[..chunks]);
                            self.injected.fetch_add(chunks as i64, Ordering::Relaxed);
                        }
                    } else {
                        InputInjector::inject_one(event);
                        self.injected.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
    }

    fn settled(&self) -> bool {
        if !self.desktop_active.load(Ordering::Acquire) {
            return false;
        }

        let since = self.active_since.load(Ordering::Acquire);
        since != 0 && clock::elapsed_microseconds(since) >= DESKTOP_SETTLE.as_micros() as i64
    }

    fn recalibrate(&self, settings: vkms_core::calibration::PointerBallistics, idle: bool) {
        let correction = (self.desktop_active.load(Ordering::Acquire) && idle).then(|| {
            // Probing moves the cursor, so it only runs while this desktop is showing and nobody is
            // driving from it.
            probe::measure(&mut InputInjector::new(), settings)
        });

        let mut calibration = self.calibration.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        calibration.ballistics = settings;

        if let Some(correction) = correction {
            log::info!("recalibrated: {} samples", correction.sample_count());
            calibration.correction = Arc::new(correction);
        }
    }
}
