//! The outbound queue, its batching, and the one policy question that matters: what is allowed to be
//! lost when the link cannot keep up.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::watch;
use vkms_core::{InputEvent, InputEventFlags, InputEventKind};
use vkms_net::{InputSink, PeerSendPump, SendPumpStall};

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

/// A sink that records what it was handed, and can be held shut to let the queue build up behind a
/// send in progress -- which is what a slow write looks like from the queue's side.
struct RecordingSink {
    batches: Mutex<Vec<Vec<InputEvent>>>,
    held: watch::Sender<bool>,
    fail: Mutex<bool>,
}

impl Default for RecordingSink {
    fn default() -> Self {
        Self { batches: Mutex::default(), held: watch::channel(false).0, fail: Mutex::default() }
    }
}

impl RecordingSink {
    fn hold(&self) {
        self.held.send_replace(true);
    }

    fn release(&self) {
        self.held.send_replace(false);
    }

    fn batch_count(&self) -> usize {
        self.batches.lock().unwrap().len()
    }

    fn all_events(&self) -> Vec<InputEvent> {
        self.batches.lock().unwrap().iter().flatten().copied().collect()
    }
}

impl InputSink for RecordingSink {
    async fn try_send_input(&self, _capture_us: i64, events: &[InputEvent]) -> bool {
        let _ = self.held.subscribe().wait_for(|held| !*held).await;

        if *self.fail.lock().unwrap() {
            return false;
        }

        self.batches.lock().unwrap().push(events.to_vec());
        true
    }
}

fn wait_for(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if condition() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    panic!("timed out waiting for {what}");
}

fn key(timestamp: i64, code: u16) -> InputEvent {
    InputEvent::key(timestamp, code, InputEventFlags::NONE)
}

#[tokio::test(flavor = "multi_thread")]
async fn at_idle_an_event_goes_out_on_its_own_rather_than_waiting_for_company() {
    // The property that separates coalescing from buffering. A pump that waited for a second event
    // before sending would post better throughput numbers and feel worse to use.
    let sink = Arc::new(RecordingSink::default());
    let pump = PeerSendPump::new(sink.clone(), "idle", 4096);
    pump.start();

    assert!(pump.try_enqueue(&key(1, 30)));

    wait_for("the lone event to be sent", || sink.batch_count() == 1);
    assert_eq!(1, sink.batches.lock().unwrap()[0].len());
}

#[tokio::test(flavor = "multi_thread")]
async fn batches_emerge_from_the_drain_when_the_link_is_busy() {
    let sink = Arc::new(RecordingSink::default());
    let pump = PeerSendPump::new(sink.clone(), "busy", 4096);

    sink.hold();
    pump.start();
    pump.try_enqueue(&key(1, 1));
    std::thread::sleep(Duration::from_millis(30));

    for i in 0..200 {
        assert!(pump.try_enqueue(&key(i + 2, (i % 60) as u16)));
    }

    sink.release();
    wait_for("every event to arrive", || sink.all_events().len() == 201);

    // Nothing was dropped, and the events that queued during the stall left in far fewer frames
    // than they arrived in -- one syscall and one TLS record instead of 200.
    assert!(pump.largest_batch() > 1, "largest batch was {}; no batching happened", pump.largest_batch());
    assert!(sink.batch_count() < 201, "{} frames for 201 events means each went out alone", sink.batch_count());
}

#[tokio::test(flavor = "multi_thread")]
async fn motion_merges_but_a_click_never_crosses_it() {
    let sink = Arc::new(RecordingSink::default());
    let pump = PeerSendPump::new(sink.clone(), "merge", 4096);

    sink.hold();
    pump.start();

    // Prime the pump so it is blocked in a send, then queue a gesture behind it.
    pump.try_enqueue(&key(1, 1));
    std::thread::sleep(Duration::from_millis(30));

    pump.try_enqueue(&InputEvent::mouse_move(10, 3, 0));
    pump.try_enqueue(&InputEvent::mouse_move(11, 4, 0));
    pump.try_enqueue(&InputEvent::mouse_button(12, 0, false));
    pump.try_enqueue(&InputEvent::mouse_move(13, 5, 0));
    pump.try_enqueue(&InputEvent::mouse_button(14, 0, true));

    sink.release();
    wait_for("both button transitions", || {
        sink.all_events().iter().filter(|e| e.kind == InputEventKind::MOUSE_BUTTON).count() == 2
    });

    let gesture: Vec<_> = sink.all_events().into_iter().filter(|e| e.kind != InputEventKind::KEY).collect();

    // Motion before the press is one event carrying the sum, and the press still lands after it.
    assert_eq!(InputEventKind::MOUSE_MOVE, gesture[0].kind);
    assert_eq!(7, gesture[0].x);
    assert_eq!(InputEventKind::MOUSE_BUTTON, gesture[1].kind);
    assert!(!gesture[1].is_up());
    assert_eq!(InputEventKind::MOUSE_MOVE, gesture[2].kind);
    assert_eq!(5, gesture[2].x);
    assert_eq!(InputEventKind::MOUSE_BUTTON, gesture[3].kind);
    assert!(gesture[3].is_up());
}

#[test]
fn a_full_queue_drops_motion_and_says_it_succeeded() {
    // Never started, so nothing drains and the queue fills.
    let pump = PeerSendPump::new(Arc::new(RecordingSink::default()), "full-motion", 8);

    for i in 0..8 {
        assert!(pump.try_enqueue(&InputEvent::mouse_move(i, 1, 0)));
    }

    // Accepted, because the motion after it supersedes it: a hitch in movement is a far cheaper
    // failure than a stuck modifier.
    assert!(pump.try_enqueue(&InputEvent::mouse_move(9, 1, 0)));
    assert_eq!(1, pump.moves_dropped());
}

#[test]
fn a_full_queue_refuses_a_key_event_and_reports_the_stall() {
    let pump = PeerSendPump::new(Arc::new(RecordingSink::default()), "full-keys", 8);
    let stall = Arc::new(Mutex::new(None));
    let record = Arc::clone(&stall);
    pump.on_stalled(move |reason| *record.lock().unwrap() = Some(reason));

    for i in 0..8 {
        assert!(pump.try_enqueue(&key(i, i as u16)));
    }

    // Refused rather than dropped: losing one of these is a coin flip between a key that never
    // arrives and a key-up that never arrives.
    assert!(!pump.try_enqueue(&key(9, 9)));
    assert_eq!(0, pump.moves_dropped());
    assert_eq!(Some(SendPumpStall::QueueFull), *stall.lock().unwrap());
}

#[test]
fn a_release_all_is_never_treated_as_droppable() {
    let pump = PeerSendPump::new(Arc::new(RecordingSink::default()), "release-all", 8);

    for i in 0..8 {
        pump.try_enqueue(&InputEvent::mouse_move(i, 1, 0));
    }

    // The event whose entire purpose is to clean up after the others.
    assert!(!pump.try_enqueue(&InputEvent::release_all(9)));
    assert_eq!(0, pump.moves_dropped());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_link_that_dies_is_reported_rather_than_retried_forever() {
    let sink = Arc::new(RecordingSink::default());
    *sink.fail.lock().unwrap() = true;

    let pump = PeerSendPump::new(sink, "dead", 4096);
    let stall = Arc::new(Mutex::new(None));
    let record = Arc::clone(&stall);
    pump.on_stalled(move |reason| *record.lock().unwrap() = Some(reason));
    pump.start();

    pump.try_enqueue(&key(1, 1));

    wait_for("the link loss to be reported", || stall.lock().unwrap().is_some());
    assert_eq!(Some(SendPumpStall::LinkLost), *stall.lock().unwrap());
}

#[test]
fn queueing_an_event_allocates_nothing() {
    // The assertion that keeps the capture path honest as the code grows.
    let pump = PeerSendPump::new(Arc::new(RecordingSink::default()), "alloc", 1024);
    pump.try_enqueue(&InputEvent::mouse_move(1, 1, 1));

    let before = ALLOCATIONS.with(Cell::get);
    for i in 0..1000 {
        pump.try_enqueue(&InputEvent::mouse_move(i, i as i32, i as i32));
    }

    assert_eq!(0, ALLOCATIONS.with(Cell::get) - before);
}

#[tokio::test(flavor = "multi_thread")]
async fn waking_a_running_pump_allocates_nothing() {
    // The same promise with the task alive and waiting, so every enqueue also wakes it.
    let sink = Arc::new(RecordingSink::default());
    let pump = PeerSendPump::new(sink.clone(), "alloc-running", 1024);
    pump.start();

    pump.try_enqueue(&key(1, 1));
    wait_for("the task to be running and idle", || sink.batch_count() == 1);

    let before = ALLOCATIONS.with(Cell::get);
    for i in 0..1000 {
        pump.try_enqueue(&key(i, (i % 60) as u16));
        if i % 100 == 0 {
            // Let the task drain and go back to waiting, so later enqueues have a waiter to wake.
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    assert_eq!(0, ALLOCATIONS.with(Cell::get) - before);
}
