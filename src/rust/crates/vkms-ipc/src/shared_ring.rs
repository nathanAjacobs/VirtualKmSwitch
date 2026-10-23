//! A ring of input events living in shared memory, carrying input between the service and a
//! session agent.
//!
//! Shared memory rather than a named pipe because this is the latency path: a pipe would cost a
//! syscall and a copy per batch, where this costs a memory write and, when the consumer is already
//! awake, nothing else.
//!
//! One writer *process* and one reader *process* per ring. Within a process, several threads may
//! write -- the agent's hook thread and raw-input thread both capture -- so each end is serialised by
//! an in-process lock. Uncontended, that is a single atomic; contended, it is held for one slot copy.

use std::sync::atomic::{AtomicI32, AtomicI64, AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use vkms_core::InputEvent;
use vkms_win::event::NamedEvent;
use vkms_win::security;
use vkms_win::shared_memory::SharedMemoryRegion;
use vkms_win::Win32Error;

const CACHE_LINE: usize = 64;

// Header: identity on the first line, producer index on its own line, consumer on the next.
// Adjacent, the producer's write would invalidate the consumer's cache line on every event.
const MAGIC_OFFSET: usize = 0;
const CAPACITY_OFFSET: usize = 8;
const SLOT_SIZE_OFFSET: usize = 12;
const CONTROL_OFFSET: usize = 16;
const PRODUCER_OFFSET: usize = CACHE_LINE;
const CONSUMER_OFFSET: usize = CACHE_LINE * 2;
const SLOTS_OFFSET: usize = CACHE_LINE * 3;

const RING_MAGIC: i64 = 0x474E_4952_534D_4B56; // "VKMSRING"

pub struct SharedMemoryRing {
    region: SharedMemoryRegion,
    signal: NamedEvent,
    capacity: usize,
    producer_gate: Mutex<()>,
    consumer_gate: Mutex<()>,
    rejected_writes: AtomicI64,
}

impl SharedMemoryRing {
    /// Creates the ring. Exactly one side does this; the other calls [`Self::open`].
    ///
    /// `global` places it in `Global\`, which the session-0 service and an agent in the console
    /// session need in order to see the same object. Creating there needs
    /// `SeCreateGlobalPrivilege`, which LocalSystem holds and an ordinary user does not.
    ///
    /// `system_only` restricts it to SYSTEM; otherwise SYSTEM, Administrators and the creator.
    pub fn create(name: &str, capacity: usize, system_only: bool, global: bool) -> Result<Self, Win32Error> {
        let rounded = round_up(capacity);
        let sddl = if system_only { security::SYSTEM_ONLY_SDDL.to_owned() } else { security::current_user_sddl()? };

        let region = SharedMemoryRegion::create(&object_name(name, global), region_len(rounded), Some(&sddl))
            .map_err(|error| {
                if global && error.is_access_denied() {
                    Win32Error::with_code(
                        error.code,
                        format!(
                            "cannot create '{}': the Global namespace requires SeCreateGlobalPrivilege, held by \
                             LocalSystem but not by an ordinary user",
                            object_name(name, true)
                        ),
                    )
                } else {
                    error
                }
            })?;

        // The event is as much a control surface as the ring itself, so it gets the same DACL.
        let signal = NamedEvent::create(&event_name(name, global), Some(&sddl))?;

        let ring = Self::wrap(region, signal, rounded);

        // Stamp the header before anyone can open it and read a half-built ring.
        ring.atomic_i64(PRODUCER_OFFSET).store(0, Ordering::Relaxed);
        ring.atomic_i64(CONSUMER_OFFSET).store(0, Ordering::Relaxed);
        ring.atomic_u32(CONTROL_OFFSET).store(0, Ordering::Relaxed);
        ring.atomic_i32(CAPACITY_OFFSET).store(rounded as i32, Ordering::Relaxed);
        ring.atomic_i32(SLOT_SIZE_OFFSET).store(std::mem::size_of::<InputEvent>() as i32, Ordering::Relaxed);
        ring.atomic_i64(MAGIC_OFFSET).store(RING_MAGIC, Ordering::Release);

        Ok(ring)
    }

    /// Opens a ring created by the other process, refusing one whose layout disagrees with ours.
    pub fn open(name: &str, capacity: usize, global: bool) -> Result<Self, Win32Error> {
        let rounded = round_up(capacity);
        let region = SharedMemoryRegion::open(&object_name(name, global), region_len(rounded))?;
        let signal = NamedEvent::open(&event_name(name, global))?;
        let ring = Self::wrap(region, signal, rounded);

        let magic = ring.atomic_i64(MAGIC_OFFSET).load(Ordering::Acquire);
        let declared_capacity = ring.atomic_i32(CAPACITY_OFFSET).load(Ordering::Relaxed);
        let slot_size = ring.atomic_i32(SLOT_SIZE_OFFSET).load(Ordering::Relaxed);

        if magic != RING_MAGIC
            || declared_capacity as usize != rounded
            || slot_size as usize != std::mem::size_of::<InputEvent>()
        {
            // Both sides compute slot addresses from these. Disagreeing would not fail -- it would
            // silently read the wrong memory.
            return Err(Win32Error::with_code(
                0,
                format!(
                    "'{name}' is not a ring of capacity {rounded} (magic {magic:#x}, capacity {declared_capacity}, \
                     slot size {slot_size}); the two sides disagree about the layout"
                ),
            ));
        }

        Ok(ring)
    }

    fn wrap(region: SharedMemoryRegion, signal: NamedEvent, capacity: usize) -> Self {
        Self {
            region,
            signal,
            capacity,
            producer_gate: Mutex::new(()),
            consumer_gate: Mutex::new(()),
            rejected_writes: AtomicI64::new(0),
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Writes this process refused because the ring was full. A backpressure signal, not a loss
    /// count.
    pub fn rejected_writes(&self) -> i64 {
        self.rejected_writes.load(Ordering::Relaxed)
    }

    pub fn count(&self) -> usize {
        let producer = self.atomic_i64(PRODUCER_OFFSET).load(Ordering::Acquire);
        let consumer = self.atomic_i64(CONSUMER_OFFSET).load(Ordering::Acquire);
        (producer - consumer).clamp(0, self.capacity as i64) as usize
    }

    /// A word of shared state alongside the ring, for the one fact that must survive a dropped
    /// message: whether the agent should be blocking local input.
    pub fn control_word(&self) -> &AtomicU32 {
        self.atomic_u32(CONTROL_OFFSET)
    }

    /// Enqueues one event. Returns `false`, storing nothing, if the ring is full.
    pub fn try_write(&self, event: &InputEvent) -> bool {
        let _gate = self.producer_gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

        // This process owns the producer index; the consumer's needs acquire to see its progress.
        let producer = self.atomic_i64(PRODUCER_OFFSET).load(Ordering::Relaxed);
        let consumer = self.atomic_i64(CONSUMER_OFFSET).load(Ordering::Acquire);

        if producer - consumer >= self.capacity as i64 {
            self.rejected_writes.fetch_add(1, Ordering::Relaxed);
            return false;
        }

        // SAFETY: the slot is outside [consumer, producer), so the reader is not touching it.
        unsafe { self.slot(producer).write_volatile(*event) };

        // Release: the slot must be visible before the consumer can see the advanced index.
        self.atomic_i64(PRODUCER_OFFSET).store(producer + 1, Ordering::Release);
        true
    }

    /// Signals that a batch has been written. Cheaper than signalling per item.
    pub fn signal_data_available(&self) {
        self.signal.set();
    }

    /// The wakeup event, for a consumer watching several rings at once.
    pub fn signal_event(&self) -> &NamedEvent {
        &self.signal
    }

    /// Blocks until data may be available, or the timeout expires.
    pub fn wait_for_data(&self, timeout: Duration) -> bool {
        self.signal.wait(timeout)
    }

    /// Drains up to `destination.len()` events.
    pub fn read_batch(&self, destination: &mut [InputEvent]) -> usize {
        if destination.is_empty() {
            return 0;
        }

        let _gate = self.consumer_gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

        let consumer = self.atomic_i64(CONSUMER_OFFSET).load(Ordering::Relaxed);
        let producer = self.atomic_i64(PRODUCER_OFFSET).load(Ordering::Acquire);

        let available = producer - consumer;
        if available <= 0 {
            return 0;
        }

        let take = (available as usize).min(destination.len()).min(self.capacity);
        for (offset, slot) in destination.iter_mut().take(take).enumerate() {
            // SAFETY: the slot is inside [consumer, producer), published by the release above.
            // Every bit pattern is a valid InputEvent, so even a hostile writer cannot produce an
            // invalid value -- only a wrong one.
            *slot = unsafe { self.slot(consumer + offset as i64).read_volatile() };
        }

        self.atomic_i64(CONSUMER_OFFSET).store(consumer + take as i64, Ordering::Release);
        take
    }

    /// Abandons anything queued.
    ///
    /// Used when an agent's desktop stops showing or an agent restarts: the ring outlives the
    /// process that was reading it, and replaying input captured before would inject stale
    /// keystrokes into whatever has focus now.
    pub fn discard_pending(&self) {
        let _gate = self.consumer_gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let producer = self.atomic_i64(PRODUCER_OFFSET).load(Ordering::Acquire);
        self.atomic_i64(CONSUMER_OFFSET).store(producer, Ordering::Release);
    }

    fn atomic_i64(&self, offset: usize) -> &AtomicI64 {
        // SAFETY: offsets are 8-aligned within a page-aligned view that lives as long as `self`.
        unsafe { &*(self.region.as_ptr().add(offset) as *const AtomicI64) }
    }

    fn atomic_i32(&self, offset: usize) -> &AtomicI32 {
        unsafe { &*(self.region.as_ptr().add(offset) as *const AtomicI32) }
    }

    fn atomic_u32(&self, offset: usize) -> &AtomicU32 {
        unsafe { &*(self.region.as_ptr().add(offset) as *const AtomicU32) }
    }

    fn slot(&self, index: i64) -> *mut InputEvent {
        let position = (index as usize) & (self.capacity - 1);
        unsafe { self.region.as_ptr().add(SLOTS_OFFSET).cast::<InputEvent>().add(position) }
    }
}

fn round_up(capacity: usize) -> usize {
    capacity.max(2).next_power_of_two()
}

fn region_len(capacity: usize) -> usize {
    SLOTS_OFFSET + capacity * std::mem::size_of::<InputEvent>()
}

fn object_name(name: &str, global: bool) -> String {
    format!(r"{}\VirtualKmSwitch.{name}", if global { "Global" } else { "Local" })
}

fn event_name(name: &str, global: bool) -> String {
    format!("{}.Signal", object_name(name, global))
}
