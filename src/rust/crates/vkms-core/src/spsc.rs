//! A bounded single-producer / single-consumer ring buffer of `Copy` values.
//!
//! This is the spine of the input hot path, so it is hand-rolled rather than a channel: values are
//! stored inline, enqueue and dequeue allocate nothing, and there are no locks or wakeups on a path
//! that must stay inside a microsecond budget.
//!
//! The two ends are separate handles, [`Producer`] and [`Consumer`], each requiring `&mut self` to
//! move data. That turns the single-producer rule from a comment into something the compiler
//! checks: a call site with several producers has to put the [`Producer`] behind a lock, visibly,
//! instead of corrupting the ring silently.
//!
//! Head and tail are padded onto separate cache lines. Unpadded, the producer's write to the tail
//! invalidates the consumer's cache line on every single event and throughput collapses -- the
//! classic false-sharing failure, which is invisible in correctness tests and only shows up as
//! disappointing numbers.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

/// An index on a cache line of its own.
#[repr(C, align(128))]
struct PaddedIndex(AtomicI64);

struct Shared<T> {
    buffer: Box<[UnsafeCell<T>]>,
    mask: i64,

    /// Next slot to write. Owned by the producer.
    tail: PaddedIndex,

    /// Next slot to read. Owned by the consumer.
    head: PaddedIndex,

    rejected_writes: AtomicI64,
}

// The producer only writes slots the consumer has released, and the consumer only reads slots the
// producer has published; the acquire/release pairs on head and tail order those accesses.
unsafe impl<T: Send> Sync for Shared<T> {}
unsafe impl<T: Send> Send for Shared<T> {}

impl<T> Shared<T> {
    fn count(&self) -> usize {
        let tail = self.tail.0.load(Ordering::Acquire);
        let head = self.head.0.load(Ordering::Acquire);
        (tail - head).max(0) as usize
    }
}

/// Creates a ring. `capacity` is rounded up to a power of two so that wrapping is a mask rather
/// than a division.
///
/// # Panics
/// If `capacity` is less than 2.
pub fn channel<T: Copy + Default>(capacity: usize) -> (Producer<T>, Consumer<T>) {
    assert!(capacity >= 2, "an SPSC ring needs a capacity of at least 2");

    let rounded = capacity.next_power_of_two();
    let buffer: Box<[UnsafeCell<T>]> = (0..rounded).map(|_| UnsafeCell::new(T::default())).collect();

    let shared = Arc::new(Shared {
        buffer,
        mask: rounded as i64 - 1,
        tail: PaddedIndex(AtomicI64::new(0)),
        head: PaddedIndex(AtomicI64::new(0)),
        rejected_writes: AtomicI64::new(0),
    });

    (Producer { shared: Arc::clone(&shared) }, Consumer { shared })
}

/// The writing end. Exactly one; put it behind a lock if several threads must write.
pub struct Producer<T> {
    shared: Arc<Shared<T>>,
}

/// The reading end. Exactly one.
pub struct Consumer<T> {
    shared: Arc<Shared<T>>,
}

macro_rules! common_accessors {
    () => {
        /// Slot count, after rounding up to a power of two.
        pub fn capacity(&self) -> usize {
            self.shared.buffer.len()
        }

        /// Items currently queued.
        pub fn count(&self) -> usize {
            self.shared.count()
        }

        pub fn is_empty(&self) -> bool {
            self.count() == 0
        }

        /// Writes refused because the ring was full.
        ///
        /// A backpressure signal, *not* a loss count: whether a refusal becomes a dropped event is
        /// the caller's policy decision. Mouse motion may be coalesced or dropped; a key or button
        /// event never may, because a lost key-up strands a modifier held down on a remote machine.
        pub fn rejected_writes(&self) -> i64 {
            self.shared.rejected_writes.load(Ordering::Relaxed)
        }
    };
}

impl<T: Copy> Producer<T> {
    common_accessors!();

    /// Enqueues one item. Returns `false`, storing nothing, if the ring is full.
    #[inline]
    pub fn try_write(&mut self, item: T) -> bool {
        let shared = &*self.shared;

        // The producer owns the tail, so its own copy needs no barrier. The head must be read with
        // acquire semantics to see the consumer's progress.
        let tail = shared.tail.0.load(Ordering::Relaxed);
        let head = shared.head.0.load(Ordering::Acquire);

        if tail - head >= shared.buffer.len() as i64 {
            shared.rejected_writes.fetch_add(1, Ordering::Relaxed);
            return false;
        }

        // SAFETY: this slot is outside [head, tail), so the consumer is not reading it, and this
        // is the only producer.
        unsafe { *shared.buffer[(tail & shared.mask) as usize].get() = item };

        // Release: the slot write must be visible before the consumer can observe the new tail.
        shared.tail.0.store(tail + 1, Ordering::Release);
        true
    }
}

impl<T: Copy> Consumer<T> {
    common_accessors!();

    /// Dequeues one item.
    #[inline]
    pub fn try_read(&mut self) -> Option<T> {
        let shared = &*self.shared;
        let head = shared.head.0.load(Ordering::Relaxed);
        let tail = shared.tail.0.load(Ordering::Acquire);

        if head == tail {
            return None;
        }

        // SAFETY: the slot is inside [head, tail), published by the producer's release store.
        let item = unsafe { *shared.buffer[(head & shared.mask) as usize].get() };
        shared.head.0.store(head + 1, Ordering::Release);
        Some(item)
    }

    /// Drains up to `destination.len()` items in one go.
    ///
    /// Batch draining is what makes coalescing possible without ever *adding* latency: the
    /// consumer takes whatever happens to be queued right now, so at idle each event goes out
    /// immediately and under load batches emerge on their own.
    pub fn read_batch(&mut self, destination: &mut [T]) -> usize {
        if destination.is_empty() {
            return 0;
        }

        let shared = &*self.shared;
        let head = shared.head.0.load(Ordering::Relaxed);
        let tail = shared.tail.0.load(Ordering::Acquire);

        let available = tail - head;
        if available <= 0 {
            return 0;
        }

        let take = (available as usize).min(destination.len());
        for (offset, slot) in destination.iter_mut().take(take).enumerate() {
            // SAFETY: as in `try_read`.
            *slot = unsafe { *shared.buffer[((head + offset as i64) & shared.mask) as usize].get() };
        }

        shared.head.0.store(head + take as i64, Ordering::Release);
        take
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InputEvent;

    #[test]
    fn capacity_rounds_up_to_power_of_two() {
        assert_eq!(8, channel::<InputEvent>(5).0.capacity());
        assert_eq!(1024, channel::<InputEvent>(1024).0.capacity());
    }

    #[test]
    fn writes_are_read_back_in_order() {
        let (mut producer, mut consumer) = channel::<InputEvent>(16);

        for i in 1..=10 {
            assert!(producer.try_write(InputEvent::mouse_move(i as i64, i, -i)));
        }

        for i in 1..=10 {
            let read = consumer.try_read().expect("an item");
            assert_eq!(i, read.x);
            assert_eq!(-i, read.y);
            assert_eq!(i as i64, read.timestamp);
        }

        assert!(consumer.try_read().is_none());
    }

    #[test]
    fn refuses_writes_when_full_and_counts_them() {
        let (mut producer, mut consumer) = channel::<InputEvent>(4);

        for i in 0..4 {
            assert!(producer.try_write(InputEvent::mouse_move(i, i as i32, 0)));
        }

        assert!(!producer.try_write(InputEvent::mouse_move(99, 99, 0)));
        assert_eq!(1, producer.rejected_writes());

        // Draining one slot makes room again.
        assert!(consumer.try_read().is_some());
        assert!(producer.try_write(InputEvent::mouse_move(99, 99, 0)));
    }

    #[test]
    fn indices_wrap_without_corruption() {
        let (mut producer, mut consumer) = channel::<InputEvent>(4);

        // Many times capacity, so the mask arithmetic wraps repeatedly.
        for i in 0..1000 {
            assert!(producer.try_write(InputEvent::mouse_move(i, i as i32, 0)));
            assert_eq!(i as i32, consumer.try_read().unwrap().x);
        }
    }

    #[test]
    fn read_batch_drains_what_is_available() {
        let (mut producer, mut consumer) = channel::<InputEvent>(64);

        for i in 0..10 {
            producer.try_write(InputEvent::mouse_move(i, i as i32, 0));
        }

        let mut destination = [InputEvent::default(); 32];
        assert_eq!(10, consumer.read_batch(&mut destination));
        for (i, event) in destination.iter().take(10).enumerate() {
            assert_eq!(i as i32, event.x);
        }

        assert_eq!(0, consumer.read_batch(&mut destination));
    }

    #[test]
    fn read_batch_is_limited_by_destination_length() {
        let (mut producer, mut consumer) = channel::<InputEvent>(64);

        for i in 0..20 {
            producer.try_write(InputEvent::mouse_move(i, i as i32, 0));
        }

        let mut small = [InputEvent::default(); 8];
        assert_eq!(8, consumer.read_batch(&mut small));
        assert_eq!(0, small[0].x);
        assert_eq!(7, small[7].x);
        assert_eq!(12, consumer.count());
    }

    #[test]
    fn concurrent_producer_and_consumer_lose_nothing() {
        const TOTAL: i32 = 200_000;
        let (mut producer, mut consumer) = channel::<InputEvent>(256);

        let writer = std::thread::spawn(move || {
            for i in 0..TOTAL {
                // A full ring is expected under load; spin rather than drop, so the test measures
                // correctness rather than backpressure policy.
                while !producer.try_write(InputEvent::mouse_move(i as i64, i, 0)) {
                    std::hint::spin_loop();
                }
            }
        });

        let reader = std::thread::spawn(move || {
            let mut batch = [InputEvent::default(); 64];
            let mut expected = 0;

            while expected < TOTAL {
                let count = consumer.read_batch(&mut batch);
                if count == 0 {
                    std::hint::spin_loop();
                    continue;
                }

                for event in &batch[..count] {
                    // Order must be exact: a key-up overtaking its key-down would strand a
                    // modifier held on a remote machine.
                    assert_eq!(expected, event.x);
                    expected += 1;
                }
            }

            expected
        });

        writer.join().unwrap();
        assert_eq!(TOTAL, reader.join().unwrap());
    }
}
