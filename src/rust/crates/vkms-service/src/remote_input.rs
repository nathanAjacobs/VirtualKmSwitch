//! Input arriving from machines that are driving this one.
//!
//! Keys and buttons held by each *source* machine are tracked here, on the receiving side, so they
//! can be released when that source lets go -- by saying so, or by vanishing. The sender also tries
//! to release what it holds on the way out, but it cannot always: the link may already be gone, or
//! the release may be racing the switch that caused it. The receiver always knows what it was told
//! to press, so this is the release that cannot be missed.
//!
//! Per source, because several machines may drive this one at once, and releasing one machine's
//! Shift must not clear another's.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use uuid::Uuid;
use vkms_core::held_keys::HeldKeyTracker;
use vkms_core::{clock, InputEvent, InputEventKind};

use crate::router::InputRouter;

pub struct RemoteInput {
    router: Arc<InputRouter>,
    held: Mutex<HashMap<Uuid, Arc<HeldKeyTracker>>>,
}

impl RemoteInput {
    pub fn new(router: Arc<InputRouter>) -> Self {
        Self { router, held: Mutex::new(HashMap::new()) }
    }

    /// Handles one batch from `source`, on that link's receive task.
    pub fn receive(&self, source: Uuid, events: &[InputEvent]) {
        let tracker = self.tracker(source);

        for event in events {
            match event.kind {
                InputEventKind::RELEASE_ALL => self.release(&tracker),

                // Never accepted from the network: hotkeys only ever come from this machine's own
                // agents.
                InputEventKind::HOTKEY => {}

                _ => {
                    tracker.record(event);
                    self.router.deliver(event);
                }
            }
        }
    }

    /// Releases whatever a machine that has just disconnected was holding.
    pub fn source_lost(&self, source: Uuid) {
        let removed = self.held.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(&source);
        if let Some(tracker) = removed {
            self.release(&tracker);
        }
    }

    /// How many keys and buttons `source` currently holds down here.
    pub fn held_by(&self, source: Uuid) -> i32 {
        self.held
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&source)
            .map_or(0, |tracker| tracker.held_count())
    }

    fn tracker(&self, source: Uuid) -> Arc<HeldKeyTracker> {
        let mut held = self.held.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        Arc::clone(held.entry(source).or_insert_with(|| Arc::new(HeldKeyTracker::new())))
    }

    fn release(&self, tracker: &HeldKeyTracker) {
        let mut releases = [InputEvent::default(); 64];

        loop {
            let count = tracker.build_release_all(clock::ticks(), &mut releases);
            if count == 0 {
                return;
            }

            for release in &releases[..count] {
                self.router.deliver(release);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use vkms_core::InputEventFlags;

    use super::*;
    use crate::supervisor::AgentChannels;

    fn create() -> RemoteInput {
        RemoteInput::new(Arc::new(InputRouter::new(Arc::new(AgentChannels::new()))))
    }

    #[test]
    fn keys_held_by_a_source_are_counted_until_released() {
        let remote = create();
        let source = Uuid::new_v4();

        remote.receive(source, &[InputEvent::key(1, 0x2A, InputEventFlags::NONE), InputEvent::mouse_button(1, 0, false)]);
        assert_eq!(2, remote.held_by(source));

        remote.receive(source, &[InputEvent::release_all(2)]);
        assert_eq!(0, remote.held_by(source));
    }

    #[test]
    fn a_source_that_disconnects_lets_go_of_everything() {
        let remote = create();
        let source = Uuid::new_v4();
        remote.receive(source, &[InputEvent::key(1, 0x1D, InputEventFlags::NONE)]);

        remote.source_lost(source);

        assert_eq!(0, remote.held_by(source));
    }

    #[test]
    fn sources_are_tracked_independently() {
        let remote = create();
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        remote.receive(a, &[InputEvent::key(1, 0x2A, InputEventFlags::NONE)]);
        remote.receive(b, &[InputEvent::key(1, 0x2A, InputEventFlags::NONE)]);

        remote.receive(a, &[InputEvent::release_all(2)]);

        assert_eq!(0, remote.held_by(a));
        assert_eq!(1, remote.held_by(b));
    }
}
