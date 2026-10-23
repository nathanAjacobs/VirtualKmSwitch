//! Estimates the offset between this node's clock and a peer's, from heartbeat round trips.
//!
//! Without this, the latency figures are meaningless. Every peer's monotonic clock starts at an
//! arbitrary point at boot, so subtracting a peer's capture timestamp from a local instant yields
//! the difference between two unrelated origins -- a number that is stable, plausible-looking, and
//! unrelated to how long anything actually took.
//!
//! The four-timestamp method is NTP's, and is used for the same reason: it cancels the one-way delay
//! out of the offset without either side needing a shared clock.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Which half of a heartbeat exchange a frame is.
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HeartbeatKind {
    /// Sent by the peer taking the measurement.
    Ping = 0,

    /// The reply, carrying the responder's receive and send instants.
    Pong = 1,
}

/// ping/pong (1) + t1 (8) + t2 (8) + t3 (8).
pub const PAYLOAD_SIZE: usize = 25;

/// How long a held sample may be trusted before any fresher one replaces it.
///
/// Keeping the lowest-round-trip sample forever would be right if clocks ran at identical rates.
/// They do not: ordinary crystal drift of a few tens of parts per million is tens of milliseconds
/// an hour, which would quietly swamp a budget measured in microseconds.
const MAX_SAMPLE_AGE: Duration = Duration::from_secs(30);

pub struct ClockSync {
    offset_us: AtomicI64,
    round_trip_us: AtomicI64,
    samples: AtomicI64,
    taken_at: Mutex<Option<Instant>>,
}

impl Default for ClockSync {
    fn default() -> Self {
        Self::new()
    }
}

impl ClockSync {
    pub fn new() -> Self {
        Self {
            offset_us: AtomicI64::new(0),
            round_trip_us: AtomicI64::new(i64::MAX),
            samples: AtomicI64::new(0),
            taken_at: Mutex::new(None),
        }
    }

    /// Microseconds to add to a peer timestamp to express it on this node's clock.
    ///
    /// Signed local-minus-remote because that is how it is used: every consumer converts an
    /// arriving timestamp into local terms, never the reverse.
    pub fn offset_microseconds(&self) -> i64 {
        self.offset_us.load(Ordering::Acquire)
    }

    /// Round trip of the sample the offset came from. A quality signal, not a latency.
    pub fn round_trip_microseconds(&self) -> i64 {
        match self.round_trip_us.load(Ordering::Acquire) {
            i64::MAX => 0,
            value => value,
        }
    }

    /// Whether any exchange has completed. Until one has, the offset is zero, not wrong.
    pub fn has_sample(&self) -> bool {
        self.samples.load(Ordering::Acquire) > 0
    }

    pub fn sample_count(&self) -> i64 {
        self.samples.load(Ordering::Acquire)
    }

    /// Folds in one completed round trip: ping sent (local), received (peer), pong sent (peer),
    /// pong received (local).
    pub fn record(&self, t1: i64, t2: i64, t3: i64, t4: i64) {
        // Time the peer spent thinking is removed before the round trip is halved; without it a
        // slow responder reads as a slow network and biases the offset by half its own delay.
        let round_trip = (t4 - t1) - (t3 - t2);
        if round_trip < 0 {
            // Only possible if a clock stepped mid-exchange. The sample says nothing usable.
            return;
        }

        let offset = ((t1 - t2) + (t4 - t3)) / 2;

        let mut taken_at = self.taken_at.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

        // The lowest round trip is the sample whose one-way delays were most nearly equal, which is
        // the assumption the whole calculation rests on. A sample taken while a switch was busy is
        // not merely noisier -- it is asymmetrically wrong.
        let better = round_trip <= self.round_trip_us.load(Ordering::Acquire);
        let stale = taken_at.is_some_and(|instant| instant.elapsed() > MAX_SAMPLE_AGE);

        if better || stale || taken_at.is_none() {
            self.offset_us.store(offset, Ordering::Release);
            self.round_trip_us.store(round_trip, Ordering::Release);
            *taken_at = Some(Instant::now());
        }

        self.samples.fetch_add(1, Ordering::AcqRel);
    }
}

/// Writes a ping. The peer echoes it back with its own two instants filled in.
pub fn write_ping(destination: &mut [u8; PAYLOAD_SIZE], local_send_us: i64) {
    destination[0] = HeartbeatKind::Ping as u8;
    destination[1..9].copy_from_slice(&local_send_us.to_le_bytes());
    destination[9..17].copy_from_slice(&0i64.to_le_bytes());
    destination[17..25].copy_from_slice(&0i64.to_le_bytes());
}

/// Writes the reply to a ping, preserving the originator's send instant untouched.
pub fn write_pong(destination: &mut [u8; PAYLOAD_SIZE], originator_send: i64, local_receive: i64, local_send: i64) {
    destination[0] = HeartbeatKind::Pong as u8;
    destination[1..9].copy_from_slice(&originator_send.to_le_bytes());
    destination[9..17].copy_from_slice(&local_receive.to_le_bytes());
    destination[17..25].copy_from_slice(&local_send.to_le_bytes());
}

/// Reads a heartbeat payload, or `None` if it is not one.
pub fn read(payload: &[u8]) -> Option<(HeartbeatKind, i64, i64, i64)> {
    if payload.len() != PAYLOAD_SIZE {
        return None;
    }

    let kind = match payload[0] {
        0 => HeartbeatKind::Ping,
        1 => HeartbeatKind::Pong,
        _ => return None,
    };

    let field = |at: usize| i64::from_le_bytes(payload[at..at + 8].try_into().unwrap());
    Some((kind, field(1), field(9), field(17)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(local_send: i64, remote_ahead: i64, delay_out: i64, thinking: i64, delay_back: i64) -> (i64, i64, i64, i64) {
        let t1 = local_send;
        let t2 = local_send + delay_out + remote_ahead;
        let t3 = t2 + thinking;
        let t4 = t3 - remote_ahead + delay_back;
        (t1, t2, t3, t4)
    }

    #[test]
    fn a_symmetric_round_trip_recovers_the_offset_exactly() {
        let sync = ClockSync::new();
        let (t1, t2, t3, t4) = round_trip(1_000_000, 250_000, 300, 50, 300);
        sync.record(t1, t2, t3, t4);

        assert_eq!(-250_000, sync.offset_microseconds());
        assert_eq!(600, sync.round_trip_microseconds());
        assert!(sync.has_sample());
    }

    #[test]
    fn the_responder_is_not_charged_for_its_own_thinking_time() {
        let sync = ClockSync::new();
        let (t1, t2, t3, t4) = round_trip(0, 0, 200, 5_000, 200);
        sync.record(t1, t2, t3, t4);

        assert_eq!(0, sync.offset_microseconds());
        assert_eq!(400, sync.round_trip_microseconds());
    }

    #[test]
    fn the_lowest_round_trip_sample_wins() {
        let sync = ClockSync::new();

        // A congested sample first: lopsided delays, so its offset is wrong by half the asymmetry.
        let (t1, t2, t3, t4) = round_trip(0, 1_000, 9_000, 0, 1_000);
        sync.record(t1, t2, t3, t4);
        let congested = sync.offset_microseconds();

        let (t1, t2, t3, t4) = round_trip(100_000, 1_000, 150, 0, 150);
        sync.record(t1, t2, t3, t4);

        assert_ne!(congested, sync.offset_microseconds());
        assert_eq!(-1_000, sync.offset_microseconds());
        assert_eq!(300, sync.round_trip_microseconds());
        assert_eq!(2, sync.sample_count());
    }

    #[test]
    fn a_worse_sample_does_not_displace_a_better_one() {
        let sync = ClockSync::new();
        let (t1, t2, t3, t4) = round_trip(0, 500, 100, 0, 100);
        sync.record(t1, t2, t3, t4);

        let (t1, t2, t3, t4) = round_trip(10_000, 500, 20_000, 0, 100);
        sync.record(t1, t2, t3, t4);

        assert_eq!(-500, sync.offset_microseconds());
        assert_eq!(200, sync.round_trip_microseconds());
    }

    #[test]
    fn a_sample_from_a_clock_that_stepped_is_ignored() {
        let sync = ClockSync::new();
        sync.record(1_000, 500, 9_000, 1_100);

        assert!(!sync.has_sample());
        assert_eq!(0, sync.offset_microseconds());
    }

    #[test]
    fn until_an_exchange_completes_the_offset_is_zero_and_says_so() {
        let sync = ClockSync::new();
        assert!(!sync.has_sample());
        assert_eq!(0, sync.offset_microseconds());
        assert_eq!(0, sync.round_trip_microseconds());
    }

    #[test]
    fn a_ping_round_trips_through_the_wire_format() {
        let mut buffer = [0u8; PAYLOAD_SIZE];
        write_ping(&mut buffer, 123_456);
        assert_eq!(Some((HeartbeatKind::Ping, 123_456, 0, 0)), read(&buffer));
    }

    #[test]
    fn a_pong_carries_the_originators_instant_back_untouched() {
        let mut buffer = [0u8; PAYLOAD_SIZE];
        write_pong(&mut buffer, 111, 222, 333);
        assert_eq!(Some((HeartbeatKind::Pong, 111, 222, 333)), read(&buffer));
    }

    #[test]
    fn a_wrongly_sized_heartbeat_is_refused() {
        for size in [0, PAYLOAD_SIZE - 1, PAYLOAD_SIZE + 1] {
            assert!(read(&vec![0u8; size]).is_none());
        }
    }

    #[test]
    fn an_unknown_heartbeat_kind_is_refused() {
        let mut payload = [0u8; PAYLOAD_SIZE];
        payload[0] = 7;
        assert!(read(&payload).is_none());
    }
}
