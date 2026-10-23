//! The peer transport.
//!
//! Every machine is a peer; there is no server and no coordinator. Machines that have been paired
//! hold each other's certificate thumbprint, and that pin is the whole of steady-state
//! authentication. Nothing about the mesh is replicated: each machine decides alone whom it trusts
//! and which machine it is driving.

pub mod beacon;
pub mod clock_sync;
pub mod coalescer;
pub mod codec;
pub mod connection;
pub mod identity;
pub mod latency;
pub mod mesh;
pub mod pairing;
pub mod pins;
pub mod send_pump;
pub mod tls;

pub use connection::PeerConnection;
pub use identity::NodeIdentity;
pub use mesh::PeerMesh;
pub use pins::PinStore;
pub use send_pump::{InputSink, PeerSendPump, SendPumpStall};
