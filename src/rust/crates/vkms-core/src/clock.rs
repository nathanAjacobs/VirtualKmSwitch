//! The single time source for anything measured or transmitted.
//!
//! Everything on the wire is microseconds, never raw ticks. Tick frequency is a property of the
//! machine, not of the platform, so a tick count means different things on two peers -- and the
//! failure is silent: latency figures come out scaled by the ratio of the two frequencies and look
//! plausible. Converting once, at the boundary, costs a divide per frame and removes the class of
//! bug entirely.
//!
//! Ticks are the performance counter rather than [`std::time::Instant`], because they have to mean
//! the same thing in two processes: an agent stamps an event at capture and the service converts
//! that stamp when it goes on the wire. `Instant` keeps its origin private, so it cannot cross the
//! process boundary.
//!
//! The origin is arbitrary and differs per boot, which is exactly why peers exchange clock samples
//! rather than comparing timestamps directly.

use std::sync::OnceLock;

/// Counter ticks per microsecond, held as a float.
///
/// The conversion is a divide rather than `ticks * 1_000_000 / frequency`. That obvious form
/// overflows an `i64`: at the usual 10 MHz it passes `i64::MAX` after roughly eleven days of
/// uptime, so the bug would appear only on machines that stay up -- which is every machine this
/// runs on.
fn ticks_per_microsecond() -> f64 {
    static VALUE: OnceLock<f64> = OnceLock::new();
    *VALUE.get_or_init(|| frequency() as f64 / 1_000_000.0)
}

/// Counter ticks per second.
pub fn frequency() -> i64 {
    static VALUE: OnceLock<i64> = OnceLock::new();
    *VALUE.get_or_init(platform::frequency)
}

/// The raw counter, comparable between processes on this machine.
#[inline]
pub fn ticks() -> i64 {
    platform::ticks()
}

/// Microseconds since an arbitrary origin fixed at boot.
#[inline]
pub fn microseconds() -> i64 {
    to_microseconds(ticks())
}

/// Converts a tick timestamp to this clock's microseconds.
#[inline]
pub fn to_microseconds(ticks: i64) -> i64 {
    (ticks as f64 / ticks_per_microsecond()) as i64
}

/// Microseconds elapsed since a tick timestamp.
#[inline]
pub fn elapsed_microseconds(since_ticks: i64) -> i64 {
    ((ticks() - since_ticks) as f64 / ticks_per_microsecond()) as i64
}

#[cfg(windows)]
mod platform {
    use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

    #[inline]
    pub fn ticks() -> i64 {
        let mut value = 0i64;
        // Cannot fail on any supported Windows version.
        unsafe { QueryPerformanceCounter(&mut value) };
        value
    }

    pub fn frequency() -> i64 {
        let mut value = 0i64;
        unsafe { QueryPerformanceFrequency(&mut value) };
        value.max(1)
    }
}

#[cfg(not(windows))]
mod platform {
    use std::sync::OnceLock;
    use std::time::Instant;

    fn origin() -> Instant {
        static ORIGIN: OnceLock<Instant> = OnceLock::new();
        *ORIGIN.get_or_init(Instant::now)
    }

    pub fn ticks() -> i64 {
        origin().elapsed().as_nanos() as i64
    }

    pub fn frequency() -> i64 {
        1_000_000_000
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversion_survives_a_timestamp_from_a_machine_that_has_been_up_for_years() {
        // The obvious form, ticks * 1_000_000 / frequency, overflows after about eleven days at
        // 10 MHz. That is not an edge case for a service that starts at boot and is expected never
        // to be restarted, and the symptom is a negative latency, not a crash.
        let three_years_of_ticks = frequency() * 60 * 60 * 24 * 365 * 3;
        let us = to_microseconds(three_years_of_ticks);
        assert!(us > 0, "conversion overflowed, giving {us}");

        // Within a second over three years. Exact equality would be asserting that the machine's
        // tick frequency divides a million.
        let expected_seconds = 60i64 * 60 * 24 * 365 * 3;
        assert!((us / 1_000_000 - expected_seconds).abs() <= 1);
    }

    #[test]
    fn the_clock_moves_forwards() {
        let first = microseconds();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let second = microseconds();
        assert!(second > first);
        assert!(second - first < 1_000_000, "two milliseconds should not read as a second");
    }
}
