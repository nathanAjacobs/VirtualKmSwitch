//! The pair of rings connecting the service to one session agent, and the naming both sides derive
//! independently.
//!
//! There is a pair per *desktop*, not per machine. Injection only works from a process created on
//! the target desktop, so the service runs one agent on Default and one on Winlogon, each with its
//! own channel.

use std::sync::atomic::Ordering;

use vkms_win::Win32Error;

use crate::shared_ring::SharedMemoryRing;

/// Bit in the inject ring's control word: this machine is driving another, so the agent should
/// swallow local input and forward it.
const ENGAGED: u32 = 1;

/// Bit in the inject ring's control word: do not give the foreground to the focus decoy while
/// blocking. A bit for "off" rather than "on", so a freshly created ring -- all zeroes -- means the
/// default.
const FOCUS_DECOY_OFF: u32 = 2;

pub struct AgentChannel {
    desktop: String,

    /// Captured input, and matched hotkeys, travelling agent to service.
    pub to_service: SharedMemoryRing,

    /// Remote input travelling service to agent, for injection.
    pub to_agent: SharedMemoryRing,
}

impl AgentChannel {
    /// Sized for roughly a quarter-second of an 8 kHz mouse. Large enough that a scheduling hiccup
    /// does not drop input; small enough that a stalled consumer surfaces as backpressure quickly
    /// rather than replaying seconds-old events.
    pub const DEFAULT_CAPACITY: usize = 2048;

    /// Creates both rings. The service does this before launching the agent, so the agent never
    /// races to open something that is not there yet.
    pub fn create(
        desktop: &str,
        session_id: u32,
        capacity: usize,
        system_only: bool,
        global: bool,
    ) -> Result<Self, Win32Error> {
        let to_service = SharedMemoryRing::create(&capture_ring_name(desktop, session_id), capacity, system_only, global)?;
        let to_agent = SharedMemoryRing::create(&inject_ring_name(desktop, session_id), capacity, system_only, global)?;

        Ok(Self { desktop: desktop.to_owned(), to_service, to_agent })
    }

    /// Opens both rings from the agent side.
    pub fn open(desktop: &str, session_id: u32, capacity: usize, global: bool) -> Result<Self, Win32Error> {
        let to_service = SharedMemoryRing::open(&capture_ring_name(desktop, session_id), capacity, global)?;
        let to_agent = SharedMemoryRing::open(&inject_ring_name(desktop, session_id), capacity, global)?;

        Ok(Self { desktop: desktop.to_owned(), to_service, to_agent })
    }

    /// The desktop this channel serves, e.g. "Default" or "Winlogon".
    pub fn desktop(&self) -> &str {
        &self.desktop
    }

    /// Tells the agent whether to block local input. Service side.
    ///
    /// State rather than a message: an agent whose desktop is not showing discards its queue, and
    /// one that restarts starts from nothing, but both must still come up blocking if this machine
    /// is driving another.
    pub fn set_engaged(&self, engaged: bool) {
        let word = self.to_agent.control_word();
        let previous = if engaged {
            word.fetch_or(ENGAGED, Ordering::AcqRel)
        } else {
            word.fetch_and(!ENGAGED, Ordering::AcqRel)
        };

        if (previous & ENGAGED != 0) != engaged {
            // Wakes the agent's injection thread, which is the one that reads this.
            self.to_agent.signal_data_available();
        }
    }

    /// Whether the service says this machine is driving another. Agent side.
    pub fn engaged(&self) -> bool {
        self.to_agent.control_word().load(Ordering::Acquire) & ENGAGED != 0
    }

    /// Tells the agent whether to use the focus decoy while blocking. Service side.
    pub fn set_focus_decoy(&self, enabled: bool) {
        let word = self.to_agent.control_word();
        let previous = if enabled {
            word.fetch_and(!FOCUS_DECOY_OFF, Ordering::AcqRel)
        } else {
            word.fetch_or(FOCUS_DECOY_OFF, Ordering::AcqRel)
        };

        if (previous & FOCUS_DECOY_OFF == 0) != enabled {
            self.to_agent.signal_data_available();
        }
    }

    /// Whether the agent should use the focus decoy while blocking. Agent side.
    pub fn focus_decoy_enabled(&self) -> bool {
        self.to_agent.control_word().load(Ordering::Acquire) & FOCUS_DECOY_OFF == 0
    }
}

// The session id is part of the name so that a fast user switch, which brings up agents for a new
// session, cannot collide with rings the old session left behind.
fn capture_ring_name(desktop: &str, session_id: u32) -> String {
    format!("Capture.{desktop}.{session_id}")
}

fn inject_ring_name(desktop: &str, session_id: u32) -> String {
    format!("Inject.{desktop}.{session_id}")
}
