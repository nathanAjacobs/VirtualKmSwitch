//! The cross-process ring, exercised within one process. Both ends map the same named region, so the
//! shared-memory path, the header handshake and the index arithmetic are all real; only the process
//! boundary is absent.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;
use std::time::Duration;

use vkms_core::{InputEvent, InputEventFlags, InputEventKind};
use vkms_ipc::{AgentChannel, SharedMemoryRing};

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

/// Named per test so parallel runs cannot collide. Created in the Local namespace without the
/// SYSTEM-only DACL, because the test process is an ordinary user; the ring logic is identical.
fn name(test: &str) -> String {
    format!("Test.{test}.{}", std::process::id())
}

fn create(test: &str, capacity: usize) -> SharedMemoryRing {
    SharedMemoryRing::create(&name(test), capacity, false, false).expect("create ring")
}

fn open(test: &str, capacity: usize) -> SharedMemoryRing {
    SharedMemoryRing::open(&name(test), capacity, false).expect("open ring")
}

#[test]
fn items_survive_the_shared_memory_round_trip() {
    let writer = create("round_trip", 64);
    let reader = open("round_trip", 64);

    for i in 1..=10 {
        assert!(writer.try_write(&InputEvent::mouse_move(i as i64, i, -i)));
    }

    let mut received = [InputEvent::default(); 32];
    assert_eq!(10, reader.read_batch(&mut received));

    for (i, event) in received.iter().take(10).enumerate() {
        let expected = i as i32 + 1;
        assert_eq!(expected, event.x);
        assert_eq!(-expected, event.y);
        assert_eq!(expected as i64, event.timestamp);
    }
}

#[test]
fn every_field_crosses_the_boundary_intact() {
    // The struct is copied as raw bytes through shared memory, so a layout mistake would silently
    // corrupt fields rather than fail loudly.
    let writer = create("every_field", 64);
    let reader = open("every_field", 64);

    let original = InputEvent::new(
        i64::MAX,
        InputEventKind::KEY,
        InputEventFlags::EXTENDED | InputEventFlags::UP,
        0x1D,
        i32::MIN,
        i32::MAX,
    );

    writer.try_write(&original);

    let mut received = [InputEvent::default(); 1];
    assert_eq!(1, reader.read_batch(&mut received));
    assert_eq!(original, received[0]);
}

#[test]
fn full_ring_refuses_writes_and_counts_them() {
    let ring = create("full", 4);

    for i in 0..ring.capacity() {
        assert!(ring.try_write(&InputEvent::mouse_move(i as i64, i as i32, 0)));
    }

    assert!(!ring.try_write(&InputEvent::mouse_move(99, 99, 0)));
    assert_eq!(1, ring.rejected_writes());
}

#[test]
fn indices_wrap_without_corruption() {
    let writer = create("wrap", 4);
    let reader = open("wrap", 4);
    let mut received = [InputEvent::default(); 4];

    for i in 0..1000 {
        assert!(writer.try_write(&InputEvent::mouse_move(i, i as i32, 0)));
        assert_eq!(1, reader.read_batch(&mut received));
        assert_eq!(i as i32, received[0].x);
    }
}

#[test]
fn mismatched_capacity_is_rejected_rather_than_misread() {
    // Both sides compute slot addresses from the capacity. Disagreeing about it would not fail --
    // it would silently read the wrong memory -- so the header is checked on open.
    let _writer = create("mismatch", 64);
    assert!(SharedMemoryRing::open(&name("mismatch"), 128, false).is_err());
}

#[test]
fn discard_pending_drops_stale_input() {
    // The ring outlives an agent that crashes. Replaying what it had queued would inject
    // keystrokes captured seconds ago into whatever has focus now.
    let writer = create("discard", 64);
    let reader = open("discard", 64);

    for i in 0..10 {
        writer.try_write(&InputEvent::mouse_move(i, i as i32, 0));
    }

    reader.discard_pending();
    let mut batch = [InputEvent::default(); 32];
    assert_eq!(0, reader.read_batch(&mut batch));

    // Still usable afterwards.
    writer.try_write(&InputEvent::mouse_move(99, 5, 5));
    assert_eq!(1, reader.read_batch(&mut batch));
}

#[test]
fn signal_wakes_a_waiting_consumer() {
    let writer = create("signal", 64);
    let reader = open("signal", 64);

    assert!(!reader.wait_for_data(Duration::from_millis(50)));

    writer.try_write(&InputEvent::mouse_move(1, 1, 1));
    writer.signal_data_available();

    assert!(reader.wait_for_data(Duration::from_secs(2)));
}

#[test]
fn steady_state_traffic_allocates_nothing() {
    let writer = create("alloc", 1024);
    let reader = open("alloc", 1024);
    let mut destination = [InputEvent::default(); 64];

    // Warm up so first-touch costs are not counted.
    for i in 0..100 {
        writer.try_write(&InputEvent::mouse_move(i, 1, 1));
    }
    reader.read_batch(&mut destination);

    let before = ALLOCATIONS.with(Cell::get);
    for _ in 0..1000 {
        for i in 0..32 {
            writer.try_write(&InputEvent::mouse_move(i, i as i32, i as i32));
        }

        while reader.read_batch(&mut destination) > 0 {}
    }

    assert_eq!(0, ALLOCATIONS.with(Cell::get) - before);
}

#[test]
fn concurrent_producer_and_consumer_lose_nothing() {
    const TOTAL: i32 = 100_000;
    let writer = create("concurrent", 256);
    let reader = open("concurrent", 256);

    std::thread::scope(|scope| {
        scope.spawn(|| {
            for i in 0..TOTAL {
                while !writer.try_write(&InputEvent::mouse_move(i as i64, i, 0)) {
                    std::hint::spin_loop();
                }
            }
        });

        let consumer = scope.spawn(|| {
            let mut batch = [InputEvent::default(); 64];
            let mut expected = 0;

            while expected < TOTAL {
                let count = reader.read_batch(&mut batch);
                if count == 0 {
                    std::hint::spin_loop();
                    continue;
                }

                for event in &batch[..count] {
                    // A key-up overtaking its key-down would strand a modifier on a remote machine.
                    assert_eq!(expected, event.x);
                    expected += 1;
                }
            }

            expected
        });

        assert_eq!(TOTAL, consumer.join().unwrap());
    });
}

#[test]
fn several_threads_in_one_process_may_write_without_corrupting_the_ring() {
    // The agent's hook thread and raw-input thread both capture. The C# version wrote to this ring
    // from both without serialising them; here each event must arrive whole and exactly once.
    const PER_THREAD: i32 = 20_000;
    let writer = Arc::new(create("multi_writer", 256));
    let reader = open("multi_writer", 256);

    let producers: Vec<_> = (0..2)
        .map(|thread| {
            let writer = Arc::clone(&writer);
            std::thread::spawn(move || {
                for i in 0..PER_THREAD {
                    let event = InputEvent::mouse_move(thread, i, thread as i32);
                    while !writer.try_write(&event) {
                        std::hint::spin_loop();
                    }
                }
            })
        })
        .collect();

    let mut next = [0i32; 2];
    let mut batch = [InputEvent::default(); 64];
    while next.iter().sum::<i32>() < PER_THREAD * 2 {
        let count = reader.read_batch(&mut batch);
        for event in &batch[..count] {
            let thread = event.timestamp as usize;
            assert_eq!(thread as i32, event.y, "fields from two writers were interleaved");
            assert_eq!(next[thread], event.x, "an event was lost or duplicated");
            next[thread] += 1;
        }
    }

    for producer in producers {
        producer.join().unwrap();
    }
}

#[test]
fn the_engaged_state_reaches_the_agent_side_and_wakes_it() {
    let service = AgentChannel::create("TestDesk", std::process::id(), 64, false, false).unwrap();
    let agent = AgentChannel::open("TestDesk", std::process::id(), 64, false).unwrap();

    assert!(!agent.engaged());

    service.set_engaged(true);
    assert!(agent.engaged());
    assert!(agent.to_agent.wait_for_data(Duration::from_secs(1)), "a change must wake the agent");

    // Setting the same state again is not news, and must not keep waking it.
    service.set_engaged(true);
    assert!(!agent.to_agent.wait_for_data(Duration::from_millis(50)));

    service.set_engaged(false);
    assert!(!agent.engaged());
}
