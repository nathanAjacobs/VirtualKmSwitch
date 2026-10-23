//! Moves input between the agents and the network, and holds the one fact that decides where local
//! input goes: which machine, if any, this one is driving.
//!
//! That fact is local and nobody else's business. Two people can drive the same machine at once,
//! and a machine can be driven while it drives another; each machine decides only where its *own*
//! keyboard and mouse go.

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, RwLock};
use std::thread::JoinHandle;
use std::time::Duration;

use uuid::Uuid;
use vkms_core::hotkeys::HotkeyActionKind;
use vkms_core::{clock, InputEvent, InputEventKind};
use vkms_win::event::NamedEvent;

use crate::supervisor::AgentChannels;

/// Where captured input goes when this machine is driving another.
pub trait PeerSender: Send + Sync {
    /// Queues one event for a peer. Returns `false` if it could not be accepted.
    fn try_send(&self, peer: Uuid, event: &InputEvent) -> bool;

    /// Whether there is a live link to that peer right now.
    fn is_connected(&self, peer: Uuid) -> bool;
}

type HotkeyHandler = Box<dyn Fn(HotkeyActionKind, i32) + Send + Sync>;
type StallHandler = Box<dyn Fn(Uuid) + Send + Sync>;
type CaptureHandler = Box<dyn Fn(&InputEvent) + Send + Sync>;

const BATCH_SIZE: usize = 256;

/// How long the drain loop sleeps when nothing is happening. A wakeup arrives from the agents'
/// events long before this; it only bounds how quickly a newly started agent is noticed.
const IDLE_WAIT: Duration = Duration::from_millis(50);

pub struct InputRouter {
    channels: Arc<AgentChannels>,

    /// The peer this machine is driving, or `None` for "typing into this machine".
    ///
    /// Read on every captured event and written when a person presses a key. An uncontended read
    /// lock is one atomic, and a `Uuid` is too wide to swap atomically on its own: a torn read would
    /// name a machine that is neither the old target nor the new one.
    engaged: RwLock<Option<Uuid>>,

    sender: RwLock<Option<Arc<dyn PeerSender>>>,
    hotkey_handlers: RwLock<Vec<HotkeyHandler>>,
    stall_handlers: RwLock<Vec<StallHandler>>,
    capture_handlers: RwLock<Vec<CaptureHandler>>,

    forwarded_from_agents: AtomicI64,
    delivered_to_agents: AtomicI64,
    sent_to_peer: AtomicI64,
    refused_by_peer: AtomicI64,
}

impl InputRouter {
    pub fn new(channels: Arc<AgentChannels>) -> Self {
        Self {
            channels,
            engaged: RwLock::new(None),
            sender: RwLock::new(None),
            hotkey_handlers: RwLock::new(Vec::new()),
            stall_handlers: RwLock::new(Vec::new()),
            capture_handlers: RwLock::new(Vec::new()),
            forwarded_from_agents: AtomicI64::new(0),
            delivered_to_agents: AtomicI64::new(0),
            sent_to_peer: AtomicI64::new(0),
            refused_by_peer: AtomicI64::new(0),
        }
    }

    /// Set once the mesh is up. With none, engaging is inert rather than fatal.
    pub fn set_sender(&self, sender: Option<Arc<dyn PeerSender>>) {
        *self.sender.write().unwrap_or_else(|poisoned| poisoned.into_inner()) = sender;
    }

    /// Called when an agent reports a matched hotkey.
    pub fn on_hotkey(&self, handler: impl Fn(HotkeyActionKind, i32) + Send + Sync + 'static) {
        push(&self.hotkey_handlers, Box::new(handler));
    }

    /// Called when the machine being driven could not take an event that may not be dropped.
    ///
    /// The listener is expected to disengage. Carrying on would mean discarding key transitions,
    /// and the one that goes missing is as likely to be a key-up as a key-down.
    pub fn on_target_stalled(&self, handler: impl Fn(Uuid) + Send + Sync + 'static) {
        push(&self.stall_handlers, Box::new(handler));
    }

    /// Called for every event captured locally, after it has been offered to the target.
    pub fn on_captured(&self, handler: impl Fn(&InputEvent) + Send + Sync + 'static) {
        push(&self.capture_handlers, Box::new(handler));
    }

    /// The peer being driven, if any.
    pub fn target(&self) -> Option<Uuid> {
        *self.engaged.read().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn is_engaged(&self) -> bool {
        self.target().is_some()
    }

    pub fn forwarded_from_agents(&self) -> i64 {
        self.forwarded_from_agents.load(Ordering::Relaxed)
    }

    pub fn delivered_to_agents(&self) -> i64 {
        self.delivered_to_agents.load(Ordering::Relaxed)
    }

    pub fn sent_to_peer(&self) -> i64 {
        self.sent_to_peer.load(Ordering::Relaxed)
    }

    pub fn refused_by_peer(&self) -> i64 {
        self.refused_by_peer.load(Ordering::Relaxed)
    }

    /// Starts driving `peer`. Local input then goes there instead of staying here.
    ///
    /// Switching straight from one machine to another releases everything on the one being left:
    /// the chord that caused the switch had its modifiers held down on it.
    pub fn engage(&self, peer: Uuid) {
        let previous = self.engaged.write().unwrap_or_else(|poisoned| poisoned.into_inner()).replace(peer);

        match previous {
            Some(previous) if previous == peer => return,
            Some(previous) => self.send_release(previous),
            None => {}
        }

        self.publish_engaged(true);
        log::info!("engaged with {peer}");
    }

    /// Returns control to this machine. Harmless when nothing is engaged.
    pub fn disengage(&self) {
        let previous = self.engaged.write().unwrap_or_else(|poisoned| poisoned.into_inner()).take();

        if let Some(previous) = previous {
            // Every key the peer thinks is held goes up, on the way out. Leaving without this is how
            // a remote machine ends up with a modifier stuck down and no way to clear it.
            self.send_release(previous);
            log::info!("disengaged from {previous}");
        }

        self.publish_engaged(false);
    }

    fn send_release(&self, peer: Uuid) {
        if let Some(sender) = self.sender() {
            sender.try_send(peer, &InputEvent::release_all(clock::ticks()));
        }
    }

    /// Tells every agent whether to block local input.
    fn publish_engaged(&self, engaged: bool) {
        self.channels.with(|channels| {
            for channel in channels {
                channel.set_engaged(engaged);
            }
        });
    }

    /// Queues remote input for injection, on every agent.
    ///
    /// Written to every agent rather than routed to one. The service sits in session 0 and cannot
    /// see which desktop owns input, so it does not try to guess: each agent gates itself on whether
    /// its own desktop is showing. Broadcasting a few bytes to two rings is far cheaper than a
    /// cross-session status channel, and cannot get the routing wrong during a desktop switch.
    pub fn deliver(&self, event: &InputEvent) {
        self.channels.with(|channels| {
            for channel in channels {
                if channel.to_agent.try_write(event) {
                    self.delivered_to_agents.fetch_add(1, Ordering::Relaxed);
                }

                channel.to_agent.signal_data_available();
            }
        });
    }

    /// Handles one event drained from an agent. The single place the engaged/local decision is made,
    /// which is what lets it be exercised without a live agent.
    pub fn capture(&self, event: &InputEvent) {
        if let Some((action, slot)) = event.hotkey_action() {
            for handler in self.hotkey_handlers.read().unwrap_or_else(|poisoned| poisoned.into_inner()).iter() {
                handler(action, slot);
            }
            return;
        }

        if event.kind == InputEventKind::HOTKEY {
            return; // an action this build does not know; never forwarded
        }

        self.forward(event);

        for handler in self.capture_handlers.read().unwrap_or_else(|poisoned| poisoned.into_inner()).iter() {
            handler(event);
        }
    }

    fn forward(&self, event: &InputEvent) {
        // Read once. Re-reading would let the target change between the check and the send, which
        // is how an event ends up delivered to a machine nobody asked for.
        let Some(target) = self.target() else {
            return;
        };

        let Some(sender) = self.sender() else {
            return;
        };

        if sender.try_send(target, event) {
            self.sent_to_peer.fetch_add(1, Ordering::Relaxed);
            return;
        }

        self.refused_by_peer.fetch_add(1, Ordering::Relaxed);
        for handler in self.stall_handlers.read().unwrap_or_else(|poisoned| poisoned.into_inner()).iter() {
            handler(target);
        }
    }

    fn sender(&self) -> Option<Arc<dyn PeerSender>> {
        self.sender.read().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }

    /// Starts the thread that drains the agents' capture rings.
    pub fn start(self: &Arc<Self>) -> RouterThread {
        let stop = Arc::new(AtomicBool::new(false));
        let router = Arc::clone(self);
        let stopping = Arc::clone(&stop);

        let thread = std::thread::Builder::new()
            .name("VirtualKmSwitch.Router".into())
            .spawn(move || {
                // This is the latency path between capture and the wire.
                vkms_win::thread::raise_current_priority();
                router.drain_loop(&stopping);
            })
            .ok();

        RouterThread { stop, thread }
    }

    fn drain_loop(&self, stop: &AtomicBool) {
        let mut batch = [InputEvent::default(); BATCH_SIZE];

        while !stop.load(Ordering::Acquire) {
            let channels = self.channels.snapshot();

            // Waits on the agents' own signals rather than a timer: Windows' default timer
            // resolution is 15.6 ms, several times the whole latency budget.
            let events: Vec<&NamedEvent> = channels.iter().map(|channel| channel.to_service.signal_event()).collect();
            NamedEvent::wait_any(&events, IDLE_WAIT);

            let engaged = self.is_engaged();

            for channel in &channels {
                // Restated every pass, so an agent that restarts while this machine is driving
                // another comes up blocking without anyone having to remember to tell it.
                channel.set_engaged(engaged);

                loop {
                    let count = channel.to_service.read_batch(&mut batch);
                    if count == 0 {
                        break;
                    }

                    self.forwarded_from_agents.fetch_add(count as i64, Ordering::Relaxed);
                    for event in &batch[..count] {
                        self.capture(event);
                    }
                }
            }
        }
    }
}

fn push<T>(handlers: &RwLock<Vec<T>>, handler: T) {
    handlers.write().unwrap_or_else(|poisoned| poisoned.into_inner()).push(handler);
}

/// The drain thread. Stops when dropped.
pub struct RouterThread {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for RouterThread {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use vkms_core::InputEventFlags;

    use super::*;

    #[derive(Default)]
    struct FakeSender {
        sent: Mutex<Vec<(Uuid, InputEvent)>>,
        refusing: Mutex<Vec<Uuid>>,
    }

    impl FakeSender {
        fn sent(&self) -> Vec<(Uuid, InputEvent)> {
            self.sent.lock().unwrap().clone()
        }
    }

    impl PeerSender for FakeSender {
        fn try_send(&self, peer: Uuid, event: &InputEvent) -> bool {
            if self.refusing.lock().unwrap().contains(&peer) {
                return false;
            }

            self.sent.lock().unwrap().push((peer, *event));
            true
        }

        fn is_connected(&self, _peer: Uuid) -> bool {
            true
        }
    }

    fn create() -> (InputRouter, Arc<FakeSender>) {
        let router = InputRouter::new(Arc::new(AgentChannels::new()));
        let sender = Arc::new(FakeSender::default());
        router.set_sender(Some(sender.clone()));
        (router, sender)
    }

    fn key(timestamp: i64, code: u16, flags: InputEventFlags) -> InputEvent {
        InputEvent::key(timestamp, code, flags)
    }

    #[test]
    fn with_nothing_engaged_nothing_leaves_the_machine() {
        let (router, sender) = create();

        router.capture(&key(1, 30, InputEventFlags::NONE));
        router.capture(&InputEvent::mouse_move(2, 5, 5));

        assert!(sender.sent().is_empty());
        assert!(!router.is_engaged());
        assert_eq!(None, router.target());
    }

    #[test]
    fn while_engaged_every_captured_event_goes_to_that_machine() {
        let (router, sender) = create();
        let peer = Uuid::new_v4();
        router.engage(peer);

        router.capture(&key(1, 30, InputEventFlags::NONE));
        router.capture(&InputEvent::mouse_move(2, 5, 5));
        router.capture(&key(3, 30, InputEventFlags::UP));

        assert_eq!(3, sender.sent().len());
        assert!(sender.sent().iter().all(|(target, _)| *target == peer));
        assert_eq!(3, router.sent_to_peer());
    }

    #[test]
    fn engaging_a_second_machine_redirects_and_releases_the_first() {
        let (router, sender) = create();
        let (first, second) = (Uuid::new_v4(), Uuid::new_v4());

        router.engage(first);
        router.capture(&key(1, 30, InputEventFlags::NONE));
        router.engage(second);
        router.capture(&key(2, 31, InputEventFlags::NONE));

        let sent = sender.sent();
        assert_eq!(first, sent[0].0);

        // The machine being left gets a release before anything goes to the new one.
        assert_eq!((first, InputEventKind::RELEASE_ALL), (sent[1].0, sent[1].1.kind));
        assert_eq!(second, sent[2].0);
        assert_eq!(Some(second), router.target());
    }

    #[test]
    fn engaging_the_machine_already_engaged_changes_nothing() {
        let (router, sender) = create();
        let peer = Uuid::new_v4();

        router.engage(peer);
        router.engage(peer);

        assert!(sender.sent().is_empty(), "no spurious release");
    }

    #[test]
    fn disengaging_releases_everything_held_on_the_machine_being_left() {
        // The single most common way a KVM breaks: a modifier is down when control moves away.
        let (router, sender) = create();
        let peer = Uuid::new_v4();
        router.engage(peer);
        router.capture(&key(1, 42, InputEventFlags::NONE));

        router.disengage();

        let (target, last) = *sender.sent().last().unwrap();
        assert_eq!(peer, target);
        assert_eq!(InputEventKind::RELEASE_ALL, last.kind);
        assert!(!router.is_engaged());
    }

    #[test]
    fn disengaging_when_nothing_is_engaged_is_harmless() {
        let (router, sender) = create();
        router.disengage();
        router.disengage();
        assert!(sender.sent().is_empty());
    }

    #[test]
    fn once_disengaged_input_stays_on_this_machine() {
        let (router, sender) = create();
        router.engage(Uuid::new_v4());
        router.disengage();
        let after_release = sender.sent().len();

        router.capture(&key(9, 30, InputEventFlags::NONE));

        assert_eq!(after_release, sender.sent().len());
    }

    #[test]
    fn a_target_that_cannot_take_a_key_event_is_reported() {
        let (router, sender) = create();
        let peer = Uuid::new_v4();
        sender.refusing.lock().unwrap().push(peer);

        let stalled = Arc::new(Mutex::new(None));
        let record = Arc::clone(&stalled);
        router.on_target_stalled(move |id| *record.lock().unwrap() = Some(id));

        router.engage(peer);
        router.capture(&key(1, 30, InputEventFlags::NONE));

        assert_eq!(Some(peer), *stalled.lock().unwrap());
        assert_eq!(1, router.refused_by_peer());
    }

    #[test]
    fn with_no_mesh_attached_engaging_is_inert_rather_than_fatal() {
        let router = InputRouter::new(Arc::new(AgentChannels::new()));
        router.engage(Uuid::new_v4());
        router.capture(&key(1, 30, InputEventFlags::NONE));
        router.disengage();
        assert!(!router.is_engaged());
    }

    #[test]
    fn captured_events_are_still_reported_locally_while_engaged() {
        let (router, _) = create();
        let seen = Arc::new(AtomicI64::new(0));
        let count = Arc::clone(&seen);
        router.on_captured(move |_| {
            count.fetch_add(1, Ordering::Relaxed);
        });

        router.engage(Uuid::new_v4());
        router.capture(&key(1, 30, InputEventFlags::NONE));
        router.capture(&key(2, 30, InputEventFlags::UP));

        assert_eq!(2, seen.load(Ordering::Relaxed));
    }

    #[test]
    fn a_hotkey_from_an_agent_is_handled_here_and_never_forwarded() {
        // The chord that switches machines must not itself arrive on the machine being driven.
        let (router, sender) = create();
        let heard = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&heard);
        router.on_hotkey(move |action, slot| record.lock().unwrap().push((action, slot)));

        router.engage(Uuid::new_v4());
        router.capture(&InputEvent::hotkey(1, HotkeyActionKind::Engage, 4));
        router.capture(&InputEvent::new(2, InputEventKind::HOTKEY, InputEventFlags::NONE, 99, 0, 0));

        assert_eq!(vec![(HotkeyActionKind::Engage, 4)], *heard.lock().unwrap());
        assert!(sender.sent().is_empty());
    }
}
