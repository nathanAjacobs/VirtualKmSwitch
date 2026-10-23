//! Records added latency and reports percentiles.
//!
//! This exists because "high performance" is otherwise unfalsifiable. The budget is stated as a
//! p99, and a p99 cannot be judged by eye -- the cases that make a KVM feel broken are the rare
//! ones, which an average hides completely.
//!
//! A fixed logarithmic histogram rather than a sample buffer: recording is a bucket increment on the
//! receive path, so it must allocate nothing and never grow.

use std::sync::atomic::{AtomicI64, Ordering};

use vkms_core::clock;

// Buckets are 10 us wide up to 1 ms, then 100 us to 10 ms, then 1 ms to 100 ms. Resolution is
// concentrated where the budget lives; anything past 100 ms is already a failure and only needs
// to be counted.
const FINE_BUCKETS: usize = 100;
const MEDIUM_BUCKETS: usize = 90;
const COARSE_BUCKETS: usize = 90;
const OVERFLOW_BUCKET: usize = FINE_BUCKETS + MEDIUM_BUCKETS + COARSE_BUCKETS;
const BUCKET_COUNT: usize = OVERFLOW_BUCKET + 1;

pub struct LatencyTracker {
    buckets: [AtomicI64; BUCKET_COUNT],
    count: AtomicI64,
    total_us: AtomicI64,
    max_us: AtomicI64,
}

impl Default for LatencyTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl LatencyTracker {
    pub const fn new() -> Self {
        Self {
            buckets: [const { AtomicI64::new(0) }; BUCKET_COUNT],
            count: AtomicI64::new(0),
            total_us: AtomicI64::new(0),
            max_us: AtomicI64::new(0),
        }
    }

    pub fn count(&self) -> i64 {
        self.count.load(Ordering::Relaxed)
    }

    pub fn mean_microseconds(&self) -> f64 {
        match self.count() {
            0 => 0.0,
            count => self.total_us.load(Ordering::Relaxed) as f64 / count as f64,
        }
    }

    pub fn max_microseconds(&self) -> i64 {
        self.max_us.load(Ordering::Relaxed)
    }

    /// Records one measurement.
    pub fn record(&self, microseconds: i64) {
        if microseconds < 0 {
            // A negative sample means the two clocks disagree about which came first, so the offset
            // estimate is stale. Counting it would drag the percentiles below reality.
            return;
        }

        self.count.fetch_add(1, Ordering::Relaxed);
        self.total_us.fetch_add(microseconds, Ordering::Relaxed);
        self.max_us.fetch_max(microseconds, Ordering::Relaxed);
        self.buckets[bucket_for(microseconds)].fetch_add(1, Ordering::Relaxed);
    }

    /// Records how long ago a peer captured something, given the capture instant on the peer's
    /// clock and the offset that converts it to ours.
    pub fn record_since(&self, capture_us: i64, clock_offset_us: i64) {
        self.record(clock::microseconds() - (capture_us + clock_offset_us));
    }

    /// Approximate percentile in microseconds; 0.99 for p99.
    pub fn percentile(&self, percentile: f64) -> i64 {
        let total = self.count();
        if total == 0 {
            return 0;
        }

        let target = (total as f64 * percentile.clamp(0.0, 1.0)).ceil() as i64;
        let mut running = 0;

        for (bucket, count) in self.buckets.iter().enumerate() {
            running += count.load(Ordering::Relaxed);
            if running >= target {
                return upper_bound_of(bucket);
            }
        }

        upper_bound_of(BUCKET_COUNT - 1)
    }

    pub fn reset(&self) {
        for bucket in &self.buckets {
            bucket.store(0, Ordering::Relaxed);
        }

        self.count.store(0, Ordering::Relaxed);
        self.total_us.store(0, Ordering::Relaxed);
        self.max_us.store(0, Ordering::Relaxed);
    }

    pub fn summary(&self) -> String {
        if self.count() == 0 {
            return "no samples".into();
        }

        format!(
            "n={} p50={}us p99={}us p999={}us max={}us mean={:.0}us",
            self.count(),
            self.percentile(0.50),
            self.percentile(0.99),
            self.percentile(0.999),
            self.max_microseconds(),
            self.mean_microseconds()
        )
    }
}

fn bucket_for(microseconds: i64) -> usize {
    match microseconds {
        0..1_000 => (microseconds / 10) as usize,
        1_000..10_000 => FINE_BUCKETS + ((microseconds - 1_000) / 100) as usize,
        10_000..100_000 => FINE_BUCKETS + MEDIUM_BUCKETS + ((microseconds - 10_000) / 1_000) as usize,
        _ => OVERFLOW_BUCKET,
    }
}

fn upper_bound_of(bucket: usize) -> i64 {
    let bucket = bucket as i64;
    let fine = FINE_BUCKETS as i64;
    let medium = MEDIUM_BUCKETS as i64;

    if bucket < fine {
        (bucket + 1) * 10
    } else if bucket < fine + medium {
        1_000 + (bucket - fine + 1) * 100
    } else if bucket < OVERFLOW_BUCKET as i64 {
        10_000 + (bucket - fine - medium + 1) * 1_000
    } else {
        100_000
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_reflect_the_distribution() {
        let tracker = LatencyTracker::new();

        // 99 fast samples and one very slow one: the mean would hide the outlier entirely.
        for _ in 0..99 {
            tracker.record(100);
        }
        tracker.record(50_000);

        assert_eq!(100, tracker.count());
        assert!(tracker.percentile(0.50) <= 200);
        assert!(tracker.percentile(0.99) <= 200);
        assert!(tracker.percentile(1.0) >= 50_000);
        assert_eq!(50_000, tracker.max_microseconds());
    }

    #[test]
    fn negative_samples_are_discarded() {
        let tracker = LatencyTracker::new();
        tracker.record(-500);
        assert_eq!(0, tracker.count());
    }

    #[test]
    fn resolution_is_finest_where_the_budget_lives() {
        let tracker = LatencyTracker::new();
        tracker.record(45);
        assert!((45..=55).contains(&tracker.percentile(0.5)));
    }

    #[test]
    fn an_empty_tracker_reports_zero_rather_than_failing() {
        let tracker = LatencyTracker::new();
        assert_eq!(0, tracker.count());
        assert_eq!(0, tracker.percentile(0.99));
        assert_eq!(0.0, tracker.mean_microseconds());
        assert_eq!("no samples", tracker.summary());
    }
}
