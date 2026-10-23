//! The wire path carries every event, so encoding, decoding, coalescing and measuring allocate
//! nothing.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use vkms_core::{InputEvent, InputEventFlags};
use vkms_net::codec::{self, FrameRead};
use vkms_net::coalescer;
use vkms_net::latency::LatencyTracker;

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
fn encoding_and_decoding_allocate_nothing() {
    let sample = [
        InputEvent::mouse_move(1000, 12, -34),
        InputEvent::mouse_button(1000, 1, false),
        InputEvent::key(1000, 0x1E, InputEventFlags::EXTENDED),
    ];
    let mut buffer = [0u8; codec::input_batch_size(3)];
    let mut decoded = [InputEvent::default(); 3];

    let allocated = allocations_during(|| {
        for i in 0..10_000 {
            codec::write_input_batch(&mut buffer, i, &sample);
            if let FrameRead::Complete { payload, .. } = codec::read_frame(&buffer) {
                codec::read_input_batch(payload, &mut decoded);
            }
        }
    });

    assert_eq!(0, allocated);
}

#[test]
fn coalescing_allocates_nothing() {
    let source: Vec<_> = (0..64)
        .map(|i| if i % 8 == 7 { InputEvent::key(i, 0x1E, InputEventFlags::NONE) } else { InputEvent::mouse_move(i, 1, 1) })
        .collect();
    let mut destination = [InputEvent::default(); 64];

    let allocated = allocations_during(|| {
        for _ in 0..10_000 {
            coalescer::coalesce(&source, &mut destination);
        }
    });

    assert_eq!(0, allocated);
}

#[test]
fn recording_latency_allocates_nothing() {
    let tracker = LatencyTracker::new();

    let allocated = allocations_during(|| {
        for i in 0..10_000 {
            tracker.record(i % 5_000);
        }
    });

    assert_eq!(0, allocated);
}
