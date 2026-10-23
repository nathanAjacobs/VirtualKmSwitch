//! Detects a low-level hook that Windows has silently removed, and reinstalls it.
//!
//! Not defensive padding. A hook callback that overruns `LowLevelHooksTimeout` is *unhooked with no
//! notification whatsoever*, and there is no API to ask whether a hook is still installed. Left
//! undetected, the consequence is the worst failure this system has: the user keeps typing believing
//! they are driving a remote machine while every keystroke lands locally.
//!
//! Liveness is proven by injecting a probe and confirming the hook saw it: a pair of opposed
//! one-pixel moves, observable, and with zero net displacement if the hook really is dead.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use vkms_core::{clock, InputEvent};
use vkms_win::hook::LowLevelHook;
use vkms_win::injector::InputInjector;

/// Outcome of a liveness probe. Three states, not two.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HookProbeResult {
    /// The hook observed the probe. It is installed and being called.
    Alive,

    /// The probe was injected but never observed. The hook is gone.
    Dead,

    /// The probe could not be delivered, so nothing was learned.
    ///
    /// The common case for an agent whose desktop is not showing: `SendInput` is refused outright.
    /// Treating that as death reinstalled healthy hooks every few seconds, and would have hidden a
    /// genuine failure behind constant false alarms. Found in the first live service run.
    Inconclusive,
}

/// Injects a probe and waits briefly for the hook to see it.
pub fn probe(hook: &LowLevelHook, injector: &mut InputInjector) -> HookProbeResult {
    let before = hook.injected_seen_count();
    let timestamp = clock::ticks();

    let accepted = injector.inject(&[InputEvent::mouse_move(timestamp, 1, 0), InputEvent::mouse_move(timestamp, -1, 0)]);

    // A refused injection says nothing about the hook.
    if accepted == 0 {
        return HookProbeResult::Inconclusive;
    }

    // The hook runs as the event is processed, but on another thread, so allow it a moment.
    let deadline = Instant::now() + Duration::from_millis(200);
    while Instant::now() < deadline {
        if hook.injected_seen_count() > before {
            return HookProbeResult::Alive;
        }

        std::thread::sleep(Duration::from_millis(10));
    }

    HookProbeResult::Dead
}

pub struct HookHealthMonitor {
    stop: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
    failed: Arc<AtomicI64>,
}

impl HookHealthMonitor {
    /// Probes every `interval`, but only while `should_probe` says injection can succeed.
    pub fn start(hook: Arc<LowLevelHook>, interval: Duration, should_probe: impl Fn() -> bool + Send + 'static) -> Self {
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let failed = Arc::new(AtomicI64::new(0));
        let counter = Arc::clone(&failed);

        let thread = std::thread::Builder::new()
            .name("VirtualKmSwitch.HookHealth".into())
            .spawn(move || {
                let mut injector = InputInjector::new();

                loop {
                    match stop_rx.recv_timeout(interval) {
                        Err(RecvTimeoutError::Timeout) => {}
                        _ => return,
                    }

                    if !should_probe() {
                        continue;
                    }

                    if probe(&hook, &mut injector) == HookProbeResult::Dead {
                        counter.fetch_add(1, Ordering::Relaxed);
                        log::warn!("hook liveness probe went unobserved -- reinstalling hooks");
                        hook.reinstall();
                    }
                }
            })
            .ok();

        Self { stop: Some(stop_tx), thread, failed }
    }

    /// Probes that went unobserved, each of which triggered a reinstall.
    pub fn probes_failed(&self) -> i64 {
        self.failed.load(Ordering::Relaxed)
    }
}

impl Drop for HookHealthMonitor {
    fn drop(&mut self) {
        self.stop.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
