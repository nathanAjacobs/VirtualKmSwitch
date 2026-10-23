//! This machine's pointer ballistics settings.
//!
//! They matter because Windows does not inject relative mouse motion verbatim: it applies the
//! pointer-speed slider and, if enabled, an acceleration curve that can multiply a delta by up to
//! four. Uncorrected, the same physical movement travels a different distance on every machine.
//!
//! Read only, never written. Changing the user's pointer settings would be visible to them and would
//! leak on a crash.

use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use windows_sys::Win32::UI::WindowsAndMessaging::{SystemParametersInfoW, SPI_GETMOUSE, SPI_GETMOUSESPEED};

use vkms_core::calibration::PointerBallistics;

/// Reads the two acceleration thresholds and the acceleration flag (`SPI_GETMOUSE`), plus the
/// speed slider 1-20 (`SPI_GETMOUSESPEED`). Falls back to the Windows defaults for anything that
/// cannot be read.
pub fn read() -> PointerBallistics {
    let mut result = PointerBallistics::DEFAULT;

    let mut mouse = [0i32; 3];
    if unsafe { SystemParametersInfoW(SPI_GETMOUSE, 0, mouse.as_mut_ptr().cast(), 0) } != 0 {
        result.threshold1 = mouse[0];
        result.threshold2 = mouse[1];
        result.acceleration = mouse[2];
    }

    let mut speed = 0i32;
    if unsafe { SystemParametersInfoW(SPI_GETMOUSESPEED, 0, (&mut speed as *mut i32).cast(), 0) } != 0 {
        result.speed = speed;
    }

    result
}

/// Whether "Enhance pointer precision" is on. When off there is no acceleration curve to
/// compensate for, and only the speed slider needs correcting.
pub fn is_pointer_precision_enabled(settings: &PointerBallistics) -> bool {
    settings.acceleration != 0
}

/// Watches the pointer settings and reports changes.
///
/// Without this, changing the speed slider or toggling "Enhance pointer precision" while running
/// leaves the calibration silently stale until a restart.
///
/// It polls rather than handling `WM_SETTINGCHANGE`, and not out of laziness: broadcast messages
/// are not delivered to message-only windows, which is what the agent's input threads use. Two
/// `SystemParametersInfo` calls every few seconds cost nothing by comparison with pumping a real
/// top-level window to observe a setting that changes perhaps twice in a machine's lifetime.
pub struct PointerSettingsMonitor {
    current: Arc<Mutex<PointerBallistics>>,
    stop: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl PointerSettingsMonitor {
    pub fn new(interval: Duration, on_changed: impl Fn(PointerBallistics) + Send + 'static) -> Self {
        let current = Arc::new(Mutex::new(read()));
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let shared = Arc::clone(&current);

        let thread = std::thread::Builder::new()
            .name("VirtualKmSwitch.PointerSettings".into())
            .spawn(move || loop {
                match stop_rx.recv_timeout(interval) {
                    Err(RecvTimeoutError::Timeout) => {}
                    _ => return,
                }

                let latest = read();
                let changed = {
                    let mut current = shared.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                    let changed = *current != latest;
                    *current = latest;
                    changed
                };

                if changed {
                    on_changed(latest);
                }
            })
            .ok();

        Self { current, stop: Some(stop_tx), thread }
    }

    /// The most recently observed settings.
    pub fn current(&self) -> PointerBallistics {
        *self.current.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Drop for PointerSettingsMonitor {
    fn drop(&mut self) {
        self.stop.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
