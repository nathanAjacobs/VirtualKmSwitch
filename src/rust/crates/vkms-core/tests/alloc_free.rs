//! The hot path allocates nothing.
//!
//! A counting global allocator stands in for .NET's `GC.GetAllocatedBytesForCurrentThread`. The
//! reason is different in Rust -- there is no garbage collector to pause the hook thread -- but the
//! property is worth just as much: an allocation can take the heap lock, and the hook thread is on
//! a budget that Windows enforces by silently unhooking whoever overruns it.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use vkms_core::calibration::{MouseCalibration, PointerBallistics};
use vkms_core::held_keys::HeldKeyTracker;
use vkms_core::hotkeys::HotkeyMatcher;
use vkms_core::{keyboard, spsc, InputEvent, InputEventFlags};

struct Counting;

thread_local! {
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.with(|count| count.set(count.get() + 1));
        System.alloc(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout)
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn allocations_during(work: impl FnOnce()) -> u64 {
    let before = ALLOCATIONS.with(Cell::get);
    work();
    ALLOCATIONS.with(Cell::get) - before
}

#[test]
fn steady_state_ring_traffic_allocates_nothing() {
    let (mut producer, mut consumer) = spsc::channel::<InputEvent>(1024);
    let mut destination = [InputEvent::default(); 64];

    let allocated = allocations_during(|| {
        for _ in 0..1000 {
            for i in 0..32 {
                producer.try_write(InputEvent::mouse_move(i, i as i32, i as i32));
            }

            while consumer.read_batch(&mut destination) > 0 {}
        }
    });

    assert_eq!(0, allocated);
}

#[test]
fn held_key_recording_allocates_nothing() {
    let tracker = HeldKeyTracker::new();

    let allocated = allocations_during(|| {
        for i in 0..10_000 {
            tracker.record(&InputEvent::key(1, 0x1E, InputEventFlags::NONE));
            tracker.record(&InputEvent::key(1, 0x1E, InputEventFlags::UP));
            tracker.record(&InputEvent::mouse_move(i, 1, 1));
        }
    });

    assert_eq!(0, allocated);
}

#[test]
fn hotkey_matching_allocates_nothing() {
    // Runs inside the hook callback, where anything slow risks the LowLevelHooksTimeout.
    let matcher = HotkeyMatcher::default();
    matcher.process(0xA2, false);
    matcher.process(0xA4, false);

    let allocated = allocations_during(|| {
        for _ in 0..10_000 {
            matcher.process(0x31, false);
            matcher.process(0x31, true);
            matcher.process(0x41, false);
        }
    });

    assert_eq!(0, allocated);
}

#[test]
fn keyboard_translation_allocates_nothing() {
    let allocated = allocations_during(|| {
        for i in 0..10_000 {
            std::hint::black_box(keyboard::try_translate(keyboard::WM_KEYDOWN, 0x41, 0x1E, 0, i));
        }
    });

    assert_eq!(0, allocated);
}

#[test]
fn calibration_allocates_nothing() {
    let mut calibration = MouseCalibration::new(PointerBallistics::DEFAULT);
    let mut output = [InputEvent::default(); MouseCalibration::MAX_CHUNKS_PER_EVENT];

    let allocated = allocations_during(|| {
        for i in 0..10_000 {
            calibration.prepare(i, 13, -7, &mut output);
        }
    });

    assert_eq!(0, allocated);
}
