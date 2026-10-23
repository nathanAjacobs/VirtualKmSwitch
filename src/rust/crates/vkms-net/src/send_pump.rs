//! The outbound half of one peer link: a queue, a task, and the batching policy.
//!
//! One long-running task per connection rather than a task per frame. A frame here is a dozen bytes
//! and the work is a single write; spawning something to do it would cost more than the work.
//!
//! A task rather than a thread of its own, because the link it writes to is already async: the
//! socket, TLS and the receive side all live on the runtime, and a thread here would only block on
//! that runtime for each write. The producer never waits on the network either way -- it writes
//! into a fixed-size queue and signals a [`Notify`], whose stored permit makes a wake that arrives
//! before the task waits impossible to lose.
//!
//! Batching is what emerges from draining, never something waited for. At idle a single event goes
//! out alone; under load the batch grows only because more had already queued while the previous
//! frame was being written.

use std::future::Future;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use vkms_core::spsc::{self, Consumer, Producer};
use vkms_core::{clock, InputEvent};

use crate::codec::MAX_EVENTS_PER_BATCH;
use crate::coalescer;

/// Where a pump delivers a drained batch. Implemented by [`crate::PeerConnection`].
pub trait InputSink: Send + Sync + 'static {
    /// Sends one batch as a single frame. Resolves to `false` if the link is gone.
    ///
    /// A returned future rather than a boxed one, so a send allocates nothing; the price is that
    /// pumps are generic over their sink instead of holding a `dyn InputSink`.
    fn try_send_input(&self, capture_us: i64, events: &[InputEvent]) -> impl Future<Output = bool> + Send;
}

/// Why a pump could not deliver.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SendPumpStall {
    /// The queue filled with events that may not be dropped. The peer must be released.
    QueueFull,

    /// The link failed mid-send.
    LinkLost,
}

pub type StallHandler = Box<dyn Fn(SendPumpStall) + Send + Sync>;

/// Deep enough to ride out a scheduling hiccup, shallow enough that filling it means something. At
/// 8 kHz this is half a second of motion, far longer than any stall worth absorbing quietly.
pub const DEFAULT_CAPACITY: usize = 4096;

struct Shared {
    /// Behind a lock because several threads produce: the router's drain loop, and whoever
    /// disengages and sends the final release.
    producer: Mutex<Producer<InputEvent>>,
    wake: Notify,
    stop: CancellationToken,
    stall_handlers: RwLock<Vec<StallHandler>>,

    enqueued: AtomicI64,
    moves_dropped: AtomicI64,
    events_sent: AtomicI64,
    batches_sent: AtomicI64,
    merged_away: AtomicI64,
    largest_batch: AtomicI64,
}

impl Shared {
    fn raise(&self, stall: SendPumpStall) {
        for handler in self.stall_handlers.read().unwrap_or_else(|poisoned| poisoned.into_inner()).iter() {
            handler(stall);
        }
    }
}

pub struct PeerSendPump<S: InputSink> {
    shared: Arc<Shared>,
    sink: Arc<S>,
    name: String,
    consumer: Mutex<Option<Consumer<InputEvent>>>,
}

impl<S: InputSink> PeerSendPump<S> {
    pub fn new(sink: Arc<S>, name: &str, capacity: usize) -> Self {
        let (producer, consumer) = spsc::channel(capacity);

        Self {
            shared: Arc::new(Shared {
                producer: Mutex::new(producer),
                wake: Notify::new(),
                stop: CancellationToken::new(),
                stall_handlers: RwLock::new(Vec::new()),
                enqueued: AtomicI64::new(0),
                moves_dropped: AtomicI64::new(0),
                events_sent: AtomicI64::new(0),
                batches_sent: AtomicI64::new(0),
                merged_away: AtomicI64::new(0),
                largest_batch: AtomicI64::new(0),
            }),
            sink,
            name: name.to_owned(),
            consumer: Mutex::new(Some(consumer)),
        }
    }

    /// Registers a handler for stalls.
    ///
    /// Not merely a statistic. Losing a key-up leaves a modifier held down on a machine the user
    /// may no longer be driving, and no amount of later input clears it. Surfacing this lets the
    /// owner disengage while it can still send a release.
    pub fn on_stalled(&self, handler: impl Fn(SendPumpStall) + Send + Sync + 'static) {
        self.shared.stall_handlers.write().unwrap_or_else(|poisoned| poisoned.into_inner()).push(Box::new(handler));
    }

    /// Starts the task that drains the queue. Must be called inside a Tokio runtime.
    pub fn start(&self) {
        let Some(consumer) = self.consumer.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take() else {
            return; // already started
        };

        tokio::spawn(run(Arc::clone(&self.shared), consumer, Arc::clone(&self.sink), self.name.clone()));
    }

    /// Queues one event. Returns as soon as it is queued: the capture side never waits on a socket.
    ///
    /// Returns `false` only when an event that may not be dropped was. A discarded mouse move
    /// returns `true`: that is the policy working, not a failure.
    pub fn try_enqueue(&self, event: &InputEvent) -> bool {
        let shared = &*self.shared;
        shared.enqueued.fetch_add(1, Ordering::Relaxed);

        let written = shared.producer.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).try_write(*event);

        // Unconditionally, rather than only when the task is known to be waiting. Checking first is
        // a lost-wakeup bug in disguise; with nobody waiting this leaves a permit, and the task's
        // next wait returns at once.
        shared.wake.notify_one();

        if written {
            return true;
        }

        // Full. Motion is the one thing that may be discarded: the movement after it supersedes it
        // and the cursor arrives in the same place either way.
        if event.is_coalescible() {
            shared.moves_dropped.fetch_add(1, Ordering::Relaxed);
            return true;
        }

        shared.raise(SendPumpStall::QueueFull);
        false
    }

    pub fn enqueued(&self) -> i64 {
        self.shared.enqueued.load(Ordering::Relaxed)
    }

    /// Mouse motion refused by a full queue. The only class of event allowed to be lost.
    pub fn moves_dropped(&self) -> i64 {
        self.shared.moves_dropped.load(Ordering::Relaxed)
    }

    /// Motion events removed by merging. Saved work, not lost input.
    pub fn merged_away(&self) -> i64 {
        self.shared.merged_away.load(Ordering::Relaxed)
    }

    pub fn events_sent(&self) -> i64 {
        self.shared.events_sent.load(Ordering::Relaxed)
    }

    pub fn batches_sent(&self) -> i64 {
        self.shared.batches_sent.load(Ordering::Relaxed)
    }

    /// The largest batch actually sent. Batches growing under a fast mouse is the design working; a
    /// rising [`Self::moves_dropped`] beside a small largest batch means the drain is not keeping up.
    pub fn largest_batch(&self) -> i64 {
        self.shared.largest_batch.load(Ordering::Relaxed)
    }

    /// Events waiting to go out. A sustained non-zero depth means the link cannot keep up.
    pub fn queue_depth(&self) -> usize {
        self.shared.producer.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).count()
    }

    /// Stops the task once it is next idle. Idempotent, and never waits.
    ///
    /// A send already in progress is left to finish or fail rather than being cut off: abandoning a
    /// write part-way would leave half a frame on a link that may still be in use. A write to a dead
    /// socket gives up on its own timeout.
    pub fn stop(&self) {
        self.shared.stop.cancel();
    }
}

impl<S: InputSink> Drop for PeerSendPump<S> {
    fn drop(&mut self) {
        self.stop();
    }
}

async fn run<S: InputSink>(shared: Arc<Shared>, mut consumer: Consumer<InputEvent>, sink: Arc<S>, name: String) {
    // One frame's worth, so a drain is never split across two writes.
    let batch_limit = consumer.capacity().min(MAX_EVENTS_PER_BATCH);
    let mut drained = vec![InputEvent::default(); batch_limit];
    let mut coalesced = vec![InputEvent::default(); batch_limit];

    loop {
        let count = consumer.read_batch(&mut drained);

        if count == 0 {
            // A producer that wrote after the drain above has also left a permit, so this returns at
            // once rather than sleeping on work that is already queued.
            tokio::select! {
                biased;
                _ = shared.stop.cancelled() => return,
                _ = shared.wake.notified() => continue,
            }
        }

        if shared.stop.is_cancelled() {
            return;
        }

        let merged = coalescer::coalesce(&drained[..count], &mut coalesced);
        shared.merged_away.fetch_add((count - merged) as i64, Ordering::Relaxed);

        if merged == 0 {
            // The whole drain was motion that cancelled itself out. Nothing worth a frame.
            continue;
        }

        // Stamped with the earliest capture instant in the batch, so the latency it reports runs
        // from when the user moved rather than from when this task got round to it.
        let capture_us = clock::to_microseconds(coalesced[0].timestamp);

        if !sink.try_send_input(capture_us, &coalesced[..merged]).await {
            log::warn!("send pump for {name} stopping: the link is gone");
            shared.raise(SendPumpStall::LinkLost);
            return;
        }

        shared.events_sent.fetch_add(merged as i64, Ordering::Relaxed);
        shared.batches_sent.fetch_add(1, Ordering::Relaxed);
        shared.largest_batch.fetch_max(merged as i64, Ordering::Relaxed);
    }
}
