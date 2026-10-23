//! Shapes injected mouse deltas so identical physical movement produces identical travel on every
//! machine, whatever its pointer settings.

use std::sync::Arc;

use crate::event::InputEvent;

/// A machine's pointer ballistics: the transformation Windows applies to injected relative motion.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PointerBallistics {
    pub threshold1: i32,
    pub threshold2: i32,
    pub acceleration: i32,
    pub speed: i32,
}

/// The pointer-speed slider is not linear. Positions 1-20 map to these multipliers, with position
/// 10 being 1.0 (no scaling).
const SPEED_MULTIPLIERS: [f64; 20] = [
    1.0 / 32.0, 1.0 / 16.0, 0.125, 0.25, 0.375, 0.5, 0.625, 0.75, 0.875, 1.0,
    1.25, 1.5, 1.75, 2.0, 2.25, 2.5, 2.75, 3.0, 3.25, 3.5,
];

impl PointerBallistics {
    /// Windows defaults: thresholds 6 and 10, acceleration on, speed slider at 10.
    pub const DEFAULT: Self = Self { threshold1: 6, threshold2: 10, acceleration: 1, speed: 10 };

    pub const fn new(threshold1: i32, threshold2: i32, acceleration: i32, speed: i32) -> Self {
        Self { threshold1, threshold2, acceleration, speed }
    }

    pub fn speed_multiplier(&self) -> f64 {
        SPEED_MULTIPLIERS[self.speed.clamp(1, SPEED_MULTIPLIERS.len() as i32) as usize - 1]
    }

    /// The largest delta that passes through without being doubled.
    ///
    /// Windows doubles a delta exceeding the first threshold, and doubles it again beyond the
    /// second when acceleration is set to 2 -- so up to four times. Staying at or below the first
    /// threshold sidesteps the whole curve, which is why motion is emitted in chunks.
    pub fn max_unaccelerated_chunk(&self) -> i32 {
        if self.acceleration == 0 {
            i32::MAX
        } else {
            self.threshold1.max(1)
        }
    }
}

impl Default for PointerBallistics {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// A measured correction curve: how much injected motion must be scaled, as a function of how far
/// it is being asked to travel.
///
/// A single scalar is not enough. Chunking linearises away the threshold doubling, but every chunk
/// in a batch reaches Windows at effectively the same instant, so its acceleration curve still sees
/// one fast movement and scales it by an amount that depends on the total magnitude. Measuring at
/// several magnitudes and interpolating between them captures that; one factor cannot.
#[derive(Clone, PartialEq, Debug)]
pub struct MouseCorrectionCurve {
    magnitudes: Vec<i32>,
    factors: Vec<f64>,
}

impl MouseCorrectionCurve {
    /// # Panics
    /// If the two lists are empty or of different lengths.
    pub fn new(magnitudes: Vec<i32>, factors: Vec<f64>) -> Self {
        assert!(
            !magnitudes.is_empty() && magnitudes.len() == factors.len(),
            "magnitudes and factors must be non-empty and equal length"
        );

        Self { magnitudes, factors }
    }

    /// No correction: what to use before a probe has run, or if probing failed.
    pub fn identity() -> Self {
        Self::new(vec![1], vec![1.0])
    }

    pub fn sample_count(&self) -> usize {
        self.magnitudes.len()
    }

    /// The scale to apply for a movement of the given magnitude, linearly interpolated between
    /// measured samples and held flat beyond the ends.
    pub fn factor_for(&self, magnitude: i32) -> f64 {
        let absolute = magnitude.unsigned_abs() as i64;
        let first = self.magnitudes[0] as i64;
        let last_index = self.magnitudes.len() - 1;

        if absolute <= first {
            return self.factors[0];
        }

        if absolute >= self.magnitudes[last_index] as i64 {
            return self.factors[last_index];
        }

        for i in 1..=last_index {
            let high = self.magnitudes[i] as i64;
            if absolute > high {
                continue;
            }

            let low = self.magnitudes[i - 1] as i64;
            let position = (absolute - low) as f64 / (high - low) as f64;
            return self.factors[i - 1] + position * (self.factors[i] - self.factors[i - 1]);
        }

        self.factors[last_index]
    }
}

impl Default for MouseCorrectionCurve {
    fn default() -> Self {
        Self::identity()
    }
}

/// Shapes outgoing mouse deltas.
///
/// Two corrections, in order:
///   1. Divide by the target's speed multiplier, so the slider position cancels out.
///   2. Split the result into chunks no larger than the acceleration threshold, so the 2x/4x
///      doubling never triggers. All chunks go out in a single `SendInput` call, so this costs one
///      syscall regardless of how many chunks it produces.
///
/// Truncation remainders are carried across calls. Without that, slow movement -- where each delta
/// scales to less than one unit -- would round to zero every time and the pointer would not move at
/// all, which is far more noticeable than a fractional error.
#[derive(Clone, Debug)]
pub struct MouseCalibration {
    pub ballistics: PointerBallistics,

    /// Empirical correction for the residual that the documented model does not explain.
    ///
    /// Chunking removes the threshold doubling, but with "Enhance pointer precision" enabled
    /// Windows also applies a smooth acceleration curve (`SmoothMouseXCurve`) that the `MOUSEINPUT`
    /// documentation does not describe. Measured, chunked motion lands about 10% short. Rather than
    /// model that curve -- undocumented, and liable to differ by version and device -- the agent's
    /// probe measures the residual on the target and sets it here.
    pub correction: Arc<MouseCorrectionCurve>,

    carry_x: f64,
    carry_y: f64,
}

impl Default for MouseCalibration {
    fn default() -> Self {
        Self::new(PointerBallistics::DEFAULT)
    }
}

impl MouseCalibration {
    /// Bound on chunks per event. A very large delta would otherwise expand without limit; past
    /// this point some acceleration is accepted rather than emitting an unbounded batch.
    pub const MAX_CHUNKS_PER_EVENT: usize = 64;

    pub fn new(ballistics: PointerBallistics) -> Self {
        Self { ballistics, correction: Arc::new(MouseCorrectionCurve::identity()), carry_x: 0.0, carry_y: 0.0 }
    }

    pub fn with_correction(mut self, correction: MouseCorrectionCurve) -> Self {
        self.correction = Arc::new(correction);
        self
    }

    /// Discards accumulated sub-unit remainders. Called when control changes machines.
    pub fn reset(&mut self) {
        self.carry_x = 0.0;
        self.carry_y = 0.0;
    }

    /// Converts one desired delta into the chunks to inject. Returns how many were written.
    pub fn prepare(&mut self, timestamp: i64, dx: i32, dy: i32, destination: &mut [InputEvent]) -> usize {
        if destination.is_empty() {
            return 0;
        }

        // Correction is keyed on the movement's overall magnitude, which is what the acceleration
        // curve responds to, rather than on either axis alone.
        let magnitude = (dx as f64).hypot(dy as f64).min(i32::MAX as f64) as i32;
        let scale = self.correction.factor_for(magnitude) / self.ballistics.speed_multiplier();

        // Carry the remainder so sub-unit movement accumulates instead of being lost.
        let want_x = dx as f64 * scale + self.carry_x;
        let want_y = dy as f64 * scale + self.carry_y;

        let total_x = want_x.trunc() as i32;
        let total_y = want_y.trunc() as i32;

        self.carry_x = want_x - total_x as f64;
        self.carry_y = want_y - total_y as f64;

        if total_x == 0 && total_y == 0 {
            return 0;
        }

        let max_chunk = self.ballistics.max_unaccelerated_chunk();
        let chunks = chunk_count(total_x, max_chunk)
            .max(chunk_count(total_y, max_chunk))
            .clamp(1, Self::MAX_CHUNKS_PER_EVENT.min(destination.len()) as i64) as i32;

        let mut remaining_x = total_x;
        let mut remaining_y = total_y;

        for (i, slot) in destination.iter_mut().take(chunks as usize).enumerate() {
            // Integer division by the number of chunks still to emit distributes the remainder
            // across them, so the pieces always sum to exactly the requested total.
            let divisor = chunks - i as i32;
            let chunk_x = remaining_x / divisor;
            let chunk_y = remaining_y / divisor;

            remaining_x -= chunk_x;
            remaining_y -= chunk_y;

            *slot = InputEvent::mouse_move(timestamp, chunk_x, chunk_y);
        }

        chunks as usize
    }
}

fn chunk_count(value: i32, max_chunk: i32) -> i64 {
    if value == 0 {
        return 0;
    }

    if max_chunk == i32::MAX {
        return 1;
    }

    let magnitude = value.unsigned_abs() as i64;
    let max_chunk = max_chunk as i64;
    (magnitude + max_chunk - 1) / max_chunk
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output() -> [InputEvent; MouseCalibration::MAX_CHUNKS_PER_EVENT] {
        [InputEvent::default(); MouseCalibration::MAX_CHUNKS_PER_EVENT]
    }

    fn sum(events: &[InputEvent]) -> (i32, i32) {
        events.iter().fold((0, 0), |(x, y), e| (x + e.x, y + e.y))
    }

    fn approx(expected: f64, actual: f64, places: i32) {
        let tolerance = 0.5 * 10f64.powi(-places);
        assert!((expected - actual).abs() <= tolerance, "expected {expected}, got {actual}");
    }

    #[test]
    fn speed_slider_midpoint_is_unity_and_the_curve_is_not_linear() {
        approx(1.0, PointerBallistics::DEFAULT.speed_multiplier(), 5);
        approx(0.5, PointerBallistics::new(6, 10, 1, 6).speed_multiplier(), 5);
        approx(3.5, PointerBallistics::new(6, 10, 1, 20).speed_multiplier(), 5);

        // Slider position is not proportional to multiplier: 20 is 3.5x, not 2x of 10.
        assert!((PointerBallistics::new(6, 10, 1, 20).speed_multiplier() - 2.0).abs() > 1e-5);
    }

    #[test]
    fn no_chunk_exceeds_the_acceleration_threshold() {
        // This is the whole point: a chunk above the threshold gets silently doubled by Windows.
        let mut calibration = MouseCalibration::default();
        let mut out = output();

        let count = calibration.prepare(1, 100, -73, &mut out);

        assert!(count > 1, "a large delta must be split");
        let threshold = PointerBallistics::DEFAULT.threshold1;
        for chunk in &out[..count] {
            assert!(chunk.x.abs() <= threshold, "chunk X {} exceeds threshold", chunk.x);
            assert!(chunk.y.abs() <= threshold, "chunk Y {} exceeds threshold", chunk.y);
        }
    }

    #[test]
    fn chunks_sum_to_the_requested_total() {
        let mut calibration = MouseCalibration::default();
        let mut out = output();

        for (dx, dy) in [(100, -73), (7, 0), (0, -1), (-45, 45), (1, 1)] {
            calibration.reset();
            let count = calibration.prepare(1, dx, dy, &mut out);
            assert_eq!((dx, dy), sum(&out[..count]));
        }
    }

    #[test]
    fn small_deltas_stay_as_one_chunk() {
        let mut calibration = MouseCalibration::default();
        let mut out = [InputEvent::default(); 16];
        assert_eq!(1, calibration.prepare(1, 3, 2, &mut out));
    }

    #[test]
    fn acceleration_disabled_means_no_chunking_at_all() {
        let mut calibration = MouseCalibration::new(PointerBallistics::new(6, 10, 0, 10));
        let mut out = output();

        let count = calibration.prepare(1, 500, 500, &mut out);
        assert_eq!(1, count);
        assert_eq!((500, 500), sum(&out[..count]));
    }

    #[test]
    fn fast_pointer_speed_is_compensated_by_scaling_down() {
        // Speed 14 doubles motion, so half as much must be sent to travel the same distance.
        let mut calibration = MouseCalibration::new(PointerBallistics::new(6, 10, 0, 14));
        let mut out = output();
        let count = calibration.prepare(1, 100, 0, &mut out);
        assert_eq!(50, sum(&out[..count]).0);
    }

    #[test]
    fn slow_pointer_speed_is_compensated_by_scaling_up() {
        // Speed 6 halves motion, so twice as much must be sent.
        let mut calibration = MouseCalibration::new(PointerBallistics::new(6, 10, 0, 6));
        let mut out = output();
        let count = calibration.prepare(1, 10, 0, &mut out);
        assert_eq!(20, sum(&out[..count]).0);
    }

    #[test]
    fn sub_unit_movement_accumulates_rather_than_vanishing() {
        // At speed 20 a delta of 1 scales to 0.28, which truncates to zero. Without carrying the
        // remainder the pointer would never move at all during slow, precise movement.
        let mut calibration = MouseCalibration::new(PointerBallistics::new(6, 10, 0, 20));
        let mut out = [InputEvent::default(); 16];

        let mut emitted = 0;
        for i in 0..20 {
            let count = calibration.prepare(i, 1, 0, &mut out);
            emitted += sum(&out[..count]).0;
        }

        // 20 units at 1/3.5 scaling is ~5.7; the carry should have produced most of that.
        assert!((4..=6).contains(&emitted), "emitted {emitted}");
    }

    #[test]
    fn reset_discards_carry() {
        let mut calibration = MouseCalibration::new(PointerBallistics::new(6, 10, 0, 20));
        let mut out = [InputEvent::default(); 16];

        for i in 0..3 {
            calibration.prepare(i, 1, 0, &mut out);
        }

        calibration.reset();

        // With the carry cleared, a single sub-unit delta must produce nothing.
        assert_eq!(0, calibration.prepare(9, 1, 0, &mut out));
    }

    #[test]
    fn chunk_count_is_bounded_for_enormous_deltas() {
        let mut calibration = MouseCalibration::default();
        let mut out = output();

        let count = calibration.prepare(1, 100_000, 0, &mut out);
        assert!((1..=MouseCalibration::MAX_CHUNKS_PER_EVENT).contains(&count));
        assert_eq!(100_000, sum(&out[..count]).0);
    }

    #[test]
    fn correction_curve_interpolates_between_measured_samples() {
        let curve = MouseCorrectionCurve::new(vec![10, 100], vec![1.5, 1.1]);

        approx(1.5, curve.factor_for(10), 4);
        approx(1.1, curve.factor_for(100), 4);
        approx(1.3, curve.factor_for(55), 2);

        // Held flat beyond the measured range rather than extrapolated into nonsense.
        approx(1.5, curve.factor_for(1), 4);
        approx(1.1, curve.factor_for(10_000), 4);
    }

    #[test]
    fn correction_curve_is_applied_by_magnitude_not_per_axis() {
        // The acceleration curve responds to the overall movement, so a diagonal move of the same
        // magnitude as a horizontal one must be corrected the same way.
        let curve = MouseCorrectionCurve::new(vec![10, 100], vec![2.0, 1.0]);
        let mut calibration = MouseCalibration::new(PointerBallistics::new(6, 10, 0, 10)).with_correction(curve);
        let mut out = output();

        let count = calibration.prepare(1, 10, 0, &mut out);
        assert_eq!(20, sum(&out[..count]).0);

        // 30/40 has magnitude 50, midway on the curve, so a factor of ~1.5.
        calibration.reset();
        let count = calibration.prepare(1, 30, 40, &mut out);
        let (x, y) = sum(&out[..count]);
        assert!((43..=47).contains(&x), "x was {x}");
        assert!((58..=62).contains(&y), "y was {y}");
    }

    #[test]
    fn identity_curve_leaves_motion_unchanged() {
        let mut calibration = MouseCalibration::new(PointerBallistics::new(6, 10, 0, 10));
        let mut out = output();
        let count = calibration.prepare(1, 137, -42, &mut out);
        assert_eq!((137, -42), sum(&out[..count]));
    }
}
