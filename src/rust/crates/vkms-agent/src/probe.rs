//! Measures how far injected motion actually travels on this machine, and derives the correction
//! that makes requested and actual agree.
//!
//! This exists because the documented model is incomplete. `SPI_GETMOUSE` and `SPI_GETMOUSESPEED`
//! explain the threshold doubling and the speed slider, but not the smooth acceleration curve
//! Windows applies when "Enhance pointer precision" is on. Measuring the result sidesteps modelling
//! an undocumented curve that varies by Windows version and device.
//!
//! The cursor is moved during the probe and restored afterwards. It takes well under a second.

use std::thread::sleep;
use std::time::Duration;

use vkms_core::calibration::{MouseCalibration, MouseCorrectionCurve, PointerBallistics};
use vkms_core::{clock, InputEvent};
use vkms_win::cursor;
use vkms_win::injector::InputInjector;

/// Magnitudes probed. Larger samples dominate because they are least affected by rounding.
const PROBE_DISTANCES: [i32; 6] = [10, 25, 50, 100, 200, 400];

/// Parking spot far enough from any screen edge that travel is never clamped.
const PARK: (i32, i32) = (400, 400);

/// Measures the residual across a range of movement sizes. Returns the identity curve if
/// measurement is not possible.
pub fn measure(injector: &mut InputInjector, ballistics: PointerBallistics) -> MouseCorrectionCurve {
    let Some(origin) = cursor::position() else {
        return MouseCorrectionCurve::identity();
    };

    let curve = measure_from_park(injector, ballistics);
    cursor::set_position(origin.0, origin.1);
    curve
}

fn measure_from_park(injector: &mut InputInjector, ballistics: PointerBallistics) -> MouseCorrectionCurve {
    let mut calibration = MouseCalibration::new(ballistics);
    let mut chunks = [InputEvent::default(); MouseCalibration::MAX_CHUNKS_PER_EVENT];
    let mut magnitudes = Vec::new();
    let mut factors = Vec::new();

    for requested in PROBE_DISTANCES {
        calibration.reset();

        if !cursor::set_position(PARK.0, PARK.1) {
            return MouseCorrectionCurve::identity();
        }

        sleep(Duration::from_millis(40));
        let Some((start, _)) = cursor::position() else {
            return MouseCorrectionCurve::identity();
        };

        let count = calibration.prepare(clock::ticks(), requested, 0, &mut chunks);
        injector.inject(&chunks[..count]);

        sleep(Duration::from_millis(80));
        let Some((end, _)) = cursor::position() else {
            return MouseCorrectionCurve::identity();
        };

        let travelled = end - start;

        // Zero or negative means something interfered -- another process moving the cursor, or the
        // injection being swallowed. Discarded rather than allowed to skew the curve.
        if travelled <= 0 {
            continue;
        }

        let factor = requested as f64 / travelled as f64;

        // Implausible samples are refused: applying one would make the pointer unusable rather than
        // merely imprecise.
        if factor > 0.25 && factor < 4.0 {
            magnitudes.push(requested);
            factors.push(factor);
        }
    }

    if magnitudes.is_empty() {
        MouseCorrectionCurve::identity()
    } else {
        MouseCorrectionCurve::new(magnitudes, factors)
    }
}
