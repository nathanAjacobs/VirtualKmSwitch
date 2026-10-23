//! The session agent: everything to do with input, on one desktop.
//!
//! A library as well as a binary so the verification harness can drive the same engagement code
//! the agent runs.

pub mod engagement;
pub mod health;
pub mod host;
pub mod probe;
