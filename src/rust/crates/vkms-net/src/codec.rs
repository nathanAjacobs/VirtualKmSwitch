//! The wire format: 2-byte length + 1-byte type + payload.
//!
//! Input frames are hand-encoded: this is the latency path, and it must allocate nothing. Control
//! messages, which are rare and benefit from being easy to evolve, travel as JSON inside the same
//! envelope.
//!
//! Events are batched with one capture timestamp for the batch instead of one per event. Per-event
//! timestamps would add eight bytes to a twelve-byte record to measure a spread of microseconds,
//! since a batch is drained from the ring in a single pass.

use vkms_core::{InputEvent, InputEventFlags, InputEventKind};

/// What a frame carries.
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FrameType {
    /// A batch of input events, binary encoded.
    InputBatch = 1,

    /// A control message, JSON encoded.
    Control = 2,

    /// Liveness, and the round-trip sample that calibrates the clock offset.
    Heartbeat = 3,

    /// Sent by both sides immediately after TLS, carrying the node id and a clock sample.
    ///
    /// Required because TLS 1.3 completes the client handshake before the server has validated the
    /// client certificate: without this exchange a peer looks connected even when the far side has
    /// already rejected it.
    Hello = 4,
}

impl FrameType {
    /// An explicit match rather than a transmute: the byte arrives from the network.
    fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            1 => Some(Self::InputBatch),
            2 => Some(Self::Control),
            3 => Some(Self::Heartbeat),
            4 => Some(Self::Hello),
            _ => None,
        }
    }
}

/// Result of attempting to read one frame from the head of a buffer.
#[derive(Debug, PartialEq, Eq)]
pub enum FrameRead<'a> {
    Complete { frame_type: FrameType, payload: &'a [u8], consumed: usize },

    /// Not enough bytes yet. Read more and try again.
    Incomplete,

    /// The bytes cannot be a valid frame. The connection must be dropped.
    Malformed,
}

/// length (2) + type (1).
pub const HEADER_SIZE: usize = 3;

/// kind (1) + flags (1) + code (2) + x (4) + y (4).
pub const ENCODED_EVENT_SIZE: usize = 12;

/// timestamp (8) + count (2).
pub const INPUT_BATCH_PREFIX_SIZE: usize = 10;

/// Bounds a hostile or desynchronised peer: a frame larger than this cannot be legitimate, and the
/// length prefix is attacker-controlled until it has been checked.
pub const MAX_PAYLOAD_SIZE: usize = 60 * 1024;

/// Largest batch that fits within [`MAX_PAYLOAD_SIZE`].
pub const MAX_EVENTS_PER_BATCH: usize = (MAX_PAYLOAD_SIZE - INPUT_BATCH_PREFIX_SIZE) / ENCODED_EVENT_SIZE;

/// Bytes needed to encode a batch of `event_count` events.
pub const fn input_batch_size(event_count: usize) -> usize {
    HEADER_SIZE + INPUT_BATCH_PREFIX_SIZE + event_count * ENCODED_EVENT_SIZE
}

/// Writes a batch of input events. Returns the bytes written, or `None` if it does not fit.
pub fn write_input_batch(destination: &mut [u8], capture_timestamp: i64, events: &[InputEvent]) -> Option<usize> {
    if events.len() > MAX_EVENTS_PER_BATCH {
        return None;
    }

    let required = input_batch_size(events.len());
    if destination.len() < required {
        return None;
    }

    let payload_length = (required - HEADER_SIZE) as u16;
    destination[0..2].copy_from_slice(&payload_length.to_le_bytes());
    destination[2] = FrameType::InputBatch as u8;

    let payload = &mut destination[HEADER_SIZE..required];
    payload[0..8].copy_from_slice(&capture_timestamp.to_le_bytes());
    payload[8..10].copy_from_slice(&(events.len() as u16).to_le_bytes());

    for (event, record) in events.iter().zip(payload[INPUT_BATCH_PREFIX_SIZE..].chunks_exact_mut(ENCODED_EVENT_SIZE)) {
        record[0] = event.kind.0;
        record[1] = event.flags.0;
        record[2..4].copy_from_slice(&event.code.to_le_bytes());
        record[4..8].copy_from_slice(&event.x.to_le_bytes());
        record[8..12].copy_from_slice(&event.y.to_le_bytes());
    }

    Some(required)
}

/// Writes a frame around an already-serialised payload.
pub fn write_control(destination: &mut [u8], frame_type: FrameType, payload: &[u8]) -> Option<usize> {
    let required = HEADER_SIZE + payload.len();
    if payload.len() > MAX_PAYLOAD_SIZE || destination.len() < required {
        return None;
    }

    destination[0..2].copy_from_slice(&(payload.len() as u16).to_le_bytes());
    destination[2] = frame_type as u8;
    destination[HEADER_SIZE..required].copy_from_slice(payload);
    Some(required)
}

/// Reads one frame from the head of `source`. The payload borrows from `source`; nothing is copied.
pub fn read_frame(source: &[u8]) -> FrameRead<'_> {
    if source.len() < HEADER_SIZE {
        return FrameRead::Incomplete;
    }

    let payload_length = u16::from_le_bytes([source[0], source[1]]) as usize;

    // Both checked before either is trusted: the length before it sizes anything, the type before
    // it is dispatched.
    let Some(frame_type) = FrameType::from_byte(source[2]) else {
        return FrameRead::Malformed;
    };

    if payload_length > MAX_PAYLOAD_SIZE {
        return FrameRead::Malformed;
    }

    let consumed = HEADER_SIZE + payload_length;
    if source.len() < consumed {
        return FrameRead::Incomplete;
    }

    FrameRead::Complete { frame_type, payload: &source[HEADER_SIZE..consumed], consumed }
}

/// Decodes an input batch payload into `destination`. Returns the count and the batch's capture
/// timestamp, or `None` if the payload is malformed or does not fit.
pub fn read_input_batch(payload: &[u8], destination: &mut [InputEvent]) -> Option<(usize, i64)> {
    if payload.len() < INPUT_BATCH_PREFIX_SIZE {
        return None;
    }

    let capture_timestamp = i64::from_le_bytes(payload[0..8].try_into().ok()?);
    let count = u16::from_le_bytes([payload[8], payload[9]]) as usize;

    // The declared count must match the bytes actually present. A mismatch means a truncated or
    // forged frame, and trusting it would read past the payload.
    if payload.len() != INPUT_BATCH_PREFIX_SIZE + count * ENCODED_EVENT_SIZE || destination.len() < count {
        return None;
    }

    for (record, slot) in payload[INPUT_BATCH_PREFIX_SIZE..].chunks_exact(ENCODED_EVENT_SIZE).zip(destination.iter_mut()) {
        *slot = InputEvent::new(
            capture_timestamp,
            InputEventKind(record[0]),
            InputEventFlags(record[1]),
            u16::from_le_bytes([record[2], record[3]]),
            i32::from_le_bytes([record[4], record[5], record[6], record[7]]),
            i32::from_le_bytes([record[8], record[9], record[10], record[11]]),
        );
    }

    Some((count, capture_timestamp))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> [InputEvent; 5] {
        [
            InputEvent::mouse_move(1000, 12, -34),
            InputEvent::mouse_button(1000, 1, false),
            InputEvent::key(1000, 0x1E, InputEventFlags::EXTENDED),
            InputEvent::mouse_wheel(1000, -120, true),
            InputEvent::release_all(1000),
        ]
    }

    fn payload_of(buffer: &[u8]) -> &[u8] {
        match read_frame(buffer) {
            FrameRead::Complete { payload, .. } => payload,
            other => panic!("expected a complete frame, got {other:?}"),
        }
    }

    #[test]
    fn input_batches_round_trip() {
        let sample = sample();
        let mut buffer = vec![0u8; input_batch_size(sample.len())];

        let written = write_input_batch(&mut buffer, 987_654_321, &sample).unwrap();
        assert_eq!(buffer.len(), written);

        let FrameRead::Complete { frame_type, payload, consumed } = read_frame(&buffer) else {
            panic!("expected a complete frame");
        };
        assert_eq!(FrameType::InputBatch, frame_type);
        assert_eq!(written, consumed);

        let mut decoded = [InputEvent::default(); 5];
        let (count, timestamp) = read_input_batch(payload, &mut decoded).unwrap();
        assert_eq!(sample.len(), count);
        assert_eq!(987_654_321, timestamp);

        for (original, decoded) in sample.iter().zip(&decoded) {
            assert_eq!(original.kind, decoded.kind);
            assert_eq!(original.flags, decoded.flags);
            assert_eq!(original.code, decoded.code);
            assert_eq!(original.x, decoded.x);
            assert_eq!(original.y, decoded.y);
        }
    }

    #[test]
    fn extreme_values_survive_encoding() {
        // Deltas are signed and a wheel delta is negative; a width or signedness mistake here would
        // send the pointer the wrong way rather than fail visibly.
        let extremes = [
            InputEvent::new(i64::MAX, InputEventKind::MOUSE_MOVE, InputEventFlags::NONE, u16::MAX, i32::MIN, i32::MAX),
            InputEvent::new(i64::MIN, InputEventKind::KEY, InputEventFlags::UP | InputEventFlags::E1, 0, -1, 1),
        ];

        let mut buffer = vec![0u8; input_batch_size(extremes.len())];
        write_input_batch(&mut buffer, -5, &extremes).unwrap();

        let mut decoded = [InputEvent::default(); 2];
        let (_, timestamp) = read_input_batch(payload_of(&buffer), &mut decoded).unwrap();

        assert_eq!(-5, timestamp);
        assert_eq!(i32::MIN, decoded[0].x);
        assert_eq!(i32::MAX, decoded[0].y);
        assert_eq!(u16::MAX, decoded[0].code);
        assert_eq!(-1, decoded[1].x);
    }

    #[test]
    fn partial_frames_report_incomplete_rather_than_failing() {
        // TCP delivers a stream, not messages: a frame arriving in pieces is the normal case.
        let sample = sample();
        let mut buffer = vec![0u8; input_batch_size(sample.len())];
        let written = write_input_batch(&mut buffer, 1, &sample).unwrap();

        for prefix in 0..written {
            assert_eq!(FrameRead::Incomplete, read_frame(&buffer[..prefix]));
        }

        assert!(matches!(read_frame(&buffer), FrameRead::Complete { .. }));
    }

    #[test]
    fn oversized_length_is_rejected() {
        let mut buffer = [0u8; 16];
        buffer[0..2].copy_from_slice(&u16::MAX.to_le_bytes());
        buffer[2] = FrameType::InputBatch as u8;
        assert_eq!(FrameRead::Malformed, read_frame(&buffer));
    }

    #[test]
    fn unknown_frame_types_are_rejected() {
        for frame_type in [0u8, 99, 255] {
            let mut buffer = [0u8; 8];
            buffer[0..2].copy_from_slice(&1u16.to_le_bytes());
            buffer[2] = frame_type;
            assert_eq!(FrameRead::Malformed, read_frame(&buffer), "type {frame_type}");
        }
    }

    #[test]
    fn a_declared_count_that_disagrees_with_the_payload_is_rejected() {
        let sample = sample();
        let mut buffer = vec![0u8; input_batch_size(sample.len())];
        write_input_batch(&mut buffer, 1, &sample).unwrap();

        let mut tampered = payload_of(&buffer).to_vec();
        tampered[8..10].copy_from_slice(&9999u16.to_le_bytes());

        assert!(read_input_batch(&tampered, &mut [InputEvent::default(); 64]).is_none());
    }

    #[test]
    fn truncated_batch_payload_is_rejected() {
        let sample = sample();
        let mut buffer = vec![0u8; input_batch_size(sample.len())];
        write_input_batch(&mut buffer, 1, &sample).unwrap();
        let payload = payload_of(&buffer);

        assert!(read_input_batch(&payload[..payload.len() - 3], &mut [InputEvent::default(); 64]).is_none());
        assert!(read_input_batch(&[], &mut [InputEvent::default(); 64]).is_none());
    }

    #[test]
    fn a_destination_too_small_is_rejected_rather_than_overflowing() {
        let sample = sample();
        let mut buffer = vec![0u8; input_batch_size(sample.len())];
        write_input_batch(&mut buffer, 1, &sample).unwrap();

        assert!(read_input_batch(payload_of(&buffer), &mut [InputEvent::default(); 2]).is_none());
    }

    #[test]
    fn consecutive_frames_are_read_independently() {
        let sample = sample();
        let mut buffer = [0u8; 512];
        let first = write_input_batch(&mut buffer, 1, &sample[0..2]).unwrap();
        let second = write_input_batch(&mut buffer[first..], 2, &sample[2..5]).unwrap();

        let FrameRead::Complete { payload, consumed, .. } = read_frame(&buffer[..first + second]) else {
            panic!("expected the first frame");
        };
        assert_eq!(first, consumed);
        assert_eq!(Some((2, 1)), read_input_batch(payload, &mut [InputEvent::default(); 8]));

        let payload = payload_of(&buffer[consumed..first + second]);
        assert_eq!(Some((3, 2)), read_input_batch(payload, &mut [InputEvent::default(); 8]));
    }

    #[test]
    fn a_mouse_move_costs_twelve_bytes_in_a_batch() {
        // Guards the wire size against drifting upward unnoticed.
        assert_eq!(12, input_batch_size(2) - input_batch_size(1));
    }
}
