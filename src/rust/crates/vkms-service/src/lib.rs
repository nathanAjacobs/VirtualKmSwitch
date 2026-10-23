//! The Virtual KM Switch service, as a library so its composition can be tested without the SCM.
//!
//! Runs as LocalSystem in session 0. It owns everything that is not input: which machines this one
//! is paired with, the links to them, pairing itself, and which machine -- if any -- this one is
//! currently driving. Input is captured and injected by the session agents it supervises.

pub mod control;
pub mod durable;
pub mod mesh_host;
pub mod paths;
pub mod peers;
pub mod registry;
pub mod remote_input;
pub mod router;
pub mod service;
pub mod settings;
pub mod supervisor;
