//! Inter-process communication on one machine.
//!
//! Two channels, because they have opposite requirements:
//!
//! * **Control** -- a named pipe between the unelevated controller and the service. Low rate, needs
//!   structure and authentication: 4-byte length + JSON, every request authorised from the caller's
//!   own token.
//! * **Input** -- shared-memory rings between the service and each session agent. The latency path,
//!   so no syscall per event and no copy beyond the ring slot itself.

pub mod agent_channel;
pub mod machines;
pub mod pipe_client;
pub mod pipe_server;
pub mod protocol;
pub mod shared_ring;

pub use agent_channel::AgentChannel;
pub use machines::{MachineInfo, MachinesView, TrustHealth};
pub use protocol::{tier_for, ControlRequest, ControlResponse, ControlTier, ControlVerb};
pub use shared_ring::SharedMemoryRing;
