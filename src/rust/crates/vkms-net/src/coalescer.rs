//! Merges consecutive mouse movement within a drained batch.
//!
//! The coalescing is opportunistic, never deliberate: nothing is ever held back waiting for more to
//! arrive. At idle a single event goes out on its own; under load, batching emerges from the drain
//! because more has queued up in the meantime. Adding latency to build a batch would trade away the
//! exact thing this system exists to protect.
//!
//! Only motion merges. A button or key event ends the run, so a click can never be reordered
//! relative to the movement that preceded it -- which is what makes a drag land where it was aimed.

use vkms_core::{InputEvent, InputEventKind};

/// Compacts `source` into `destination`, summing runs of adjacent mouse movement into one event.
/// Returns the number written.
pub fn coalesce(source: &[InputEvent], destination: &mut [InputEvent]) -> usize {
    let mut written = 0;
    let mut index = 0;

    while index < source.len() && written < destination.len() {
        let current = source[index];

        if current.kind != InputEventKind::MOUSE_MOVE {
            destination[written] = current;
            written += 1;
            index += 1;
            continue;
        }

        // Accumulate the whole run. The earliest timestamp is kept, so a latency measurement
        // reflects when the movement started rather than when it was finally flushed.
        let timestamp = current.timestamp;
        let mut total_x = 0i64;
        let mut total_y = 0i64;

        while index < source.len() && source[index].kind == InputEventKind::MOUSE_MOVE {
            total_x += source[index].x as i64;
            total_y += source[index].y as i64;
            index += 1;
        }

        // A run that cancels itself out is not movement, and is dropped rather than sent as a zero
        // delta the far side would inject for nothing.
        if total_x != 0 || total_y != 0 {
            destination[written] = InputEvent::mouse_move(timestamp, saturate(total_x), saturate(total_y));
            written += 1;
        }
    }

    written
}

/// Clamps an accumulated delta into the range the wire format carries.
fn saturate(value: i64) -> i32 {
    value.clamp(i32::MIN as i64, i32::MAX as i64) as i32
}

#[cfg(test)]
mod tests {
    use vkms_core::InputEventFlags;

    use super::*;

    fn run(source: &[InputEvent]) -> Vec<InputEvent> {
        let mut destination = vec![InputEvent::default(); source.len()];
        let count = coalesce(source, &mut destination);
        destination.truncate(count);
        destination
    }

    #[test]
    fn consecutive_motion_merges_into_one_event() {
        let result = run(&[
            InputEvent::mouse_move(10, 3, 1),
            InputEvent::mouse_move(20, 4, 2),
            InputEvent::mouse_move(30, -1, 3),
        ]);

        assert_eq!(1, result.len());
        assert_eq!((6, 6), (result[0].x, result[0].y));
        assert_eq!(10, result[0].timestamp);
    }

    #[test]
    fn buttons_and_keys_end_the_run() {
        let result = run(&[
            InputEvent::mouse_move(1, 5, 0),
            InputEvent::mouse_move(2, 5, 0),
            InputEvent::mouse_button(3, 0, false),
            InputEvent::mouse_move(4, 7, 0),
            InputEvent::mouse_button(5, 0, true),
        ]);

        assert_eq!(4, result.len());
        assert_eq!(InputEventKind::MOUSE_MOVE, result[0].kind);
        assert_eq!(10, result[0].x);
        assert_eq!(InputEventKind::MOUSE_BUTTON, result[1].kind);
        assert!(!result[1].is_up());
        assert_eq!(7, result[2].x);
        assert!(result[3].is_up());
    }

    #[test]
    fn key_events_are_never_merged_or_dropped() {
        let source = [
            InputEvent::key(1, 0x2A, InputEventFlags::NONE),
            InputEvent::key(2, 0x1E, InputEventFlags::NONE),
            InputEvent::key(3, 0x1E, InputEventFlags::UP),
            InputEvent::key(4, 0x2A, InputEventFlags::UP),
        ];

        assert_eq!(source.to_vec(), run(&source));
    }

    #[test]
    fn motion_that_cancels_out_is_dropped() {
        assert!(run(&[InputEvent::mouse_move(1, 5, 5), InputEvent::mouse_move(2, -5, -5)]).is_empty());
    }

    #[test]
    fn a_cancelling_run_between_clicks_still_preserves_the_clicks() {
        let result = run(&[
            InputEvent::mouse_button(1, 0, false),
            InputEvent::mouse_move(2, 3, 0),
            InputEvent::mouse_move(3, -3, 0),
            InputEvent::mouse_button(4, 0, true),
        ]);

        assert_eq!(2, result.len());
        assert!(result.iter().all(|e| e.kind == InputEventKind::MOUSE_BUTTON));
    }

    #[test]
    fn accumulated_motion_saturates_rather_than_wrapping() {
        // Wrapping would send the pointer hard in the opposite direction.
        let result = run(&[InputEvent::mouse_move(1, i32::MAX, 0), InputEvent::mouse_move(2, i32::MAX, 0)]);
        assert_eq!(1, result.len());
        assert_eq!(i32::MAX, result[0].x);
    }

    #[test]
    fn other_event_kinds_pass_through_untouched() {
        let result = run(&[InputEvent::mouse_wheel(1, 120, false), InputEvent::release_all(2)]);
        assert_eq!(InputEventKind::MOUSE_WHEEL, result[0].kind);
        assert_eq!(InputEventKind::RELEASE_ALL, result[1].kind);
    }

    #[test]
    fn empty_input_produces_nothing() {
        assert_eq!(0, coalesce(&[], &mut [InputEvent::default(); 4]));
    }

    #[test]
    fn motion_heavy_traffic_compresses_sharply() {
        let source: Vec<_> = (0..1000).map(|i| InputEvent::mouse_move(i, 1, 0)).collect();
        let result = run(&source);
        assert_eq!(1, result.len());
        assert_eq!(1000, result[0].x);
    }
}
