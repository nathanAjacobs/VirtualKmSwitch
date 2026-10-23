//! Pairing: the one moment two machines that do not trust each other may talk.
//!
//! One machine opens a short pairing window and shows a code. The other machine's user types that
//! code in. Each side proves knowledge of the code with an HMAC bound to *both* TLS certificates in
//! use, and on success each records the other's certificate thumbprint in its own local list.
//!
//! That is the whole of the shared state. Nothing is replicated afterwards: pairing A with B and A
//! with C does not introduce B to C, and which machine A drives is A's business alone.

pub mod code;
pub mod handshake;
mod protocol;
pub mod window;

pub use code::PairingCode;
pub use handshake::EnrollmentResult;
pub use window::{join, EnrollmentOutcome, PairingIdentity, PairingWindow};
