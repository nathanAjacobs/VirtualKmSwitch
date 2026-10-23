//! Proves that two machines know the same pairing code, and binds that proof to the exact TLS
//! certificates in use.
//!
//! The binding is the entire point. A bare "do we both know the secret" exchange can be relayed
//! verbatim by an interceptor who knows neither certificate's private key, leaving both honest
//! machines convinced they paired with each other while the interceptor sits in the middle holding
//! every keystroke. Mixing both thumbprints into the MAC means a proof is only valid for the pair of
//! certificates it was computed over, so a relayed proof simply fails.

use ring::hmac;
use ring::rand::{SecureRandom, SystemRandom};

/// Domain separation, so this MAC can never be confused with another use of the code.
const CONTEXT: &str = "vkms-enroll-v1";

pub const NONCE_SIZE: usize = 32;
pub const PROOF_SIZE: usize = 32;

/// Why a pairing attempt ended as it did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EnrollmentResult {
    Accepted,

    /// The proof did not match: the wrong code.
    WrongKey,

    /// The proof was valid for a different pair of certificates than the ones in use: something
    /// re-terminated TLS between the two machines.
    CertificateMismatch,

    /// The other machine is not accepting a pairing right now.
    PairingWindowClosed,

    /// Malformed, truncated, or abandoned.
    Malformed,
}

pub fn create_nonce() -> [u8; NONCE_SIZE] {
    let mut nonce = [0u8; NONCE_SIZE];
    SystemRandom::new().fill(&mut nonce).expect("the system random number generator failed");
    nonce
}

/// Each field is length-prefixed so that concatenation is unambiguous: without it, two different
/// field splits could produce the same MAC input.
fn message(local_thumbprint: &str, remote_thumbprint: &str, initiator_nonce: &[u8], responder_nonce: &[u8]) -> Vec<u8> {
    let mut message = Vec::with_capacity(256);

    // Order matters and must be identical on both sides, so each is described by its role in the
    // exchange rather than by which end is computing it.
    for field in [
        CONTEXT.as_bytes(),
        local_thumbprint.to_ascii_uppercase().as_bytes(),
        remote_thumbprint.to_ascii_uppercase().as_bytes(),
        initiator_nonce,
        responder_nonce,
    ] {
        message.extend_from_slice(&(field.len() as i32).to_le_bytes());
        message.extend_from_slice(field);
    }

    message
}

/// Computes the proof for one direction of the exchange.
///
/// `local_thumbprint` is the certificate this side presented; `remote_thumbprint` the one it
/// actually observed from the peer.
pub fn compute_proof(
    code: &str,
    local_thumbprint: &str,
    remote_thumbprint: &str,
    initiator_nonce: &[u8],
    responder_nonce: &[u8],
) -> [u8; PROOF_SIZE] {
    let key = hmac::Key::new(hmac::HMAC_SHA256, code.as_bytes());
    let tag = hmac::sign(&key, &message(local_thumbprint, remote_thumbprint, initiator_nonce, responder_nonce));

    let mut proof = [0u8; PROOF_SIZE];
    proof.copy_from_slice(tag.as_ref());
    proof
}

/// Verifies a proof received from a peer.
///
/// `expected_local_thumbprint` is the peer's own certificate *as this side observed it* -- never as
/// the peer described it. Trusting a self-reported thumbprint would defeat the binding entirely.
pub fn verify(
    code: &str,
    expected_local_thumbprint: &str,
    expected_remote_thumbprint: &str,
    initiator_nonce: &[u8],
    responder_nonce: &[u8],
    received_proof: &[u8],
) -> EnrollmentResult {
    if received_proof.len() != PROOF_SIZE || initiator_nonce.len() != NONCE_SIZE || responder_nonce.len() != NONCE_SIZE {
        return EnrollmentResult::Malformed;
    }

    let key = hmac::Key::new(hmac::HMAC_SHA256, code.as_bytes());
    let expected = message(expected_local_thumbprint, expected_remote_thumbprint, initiator_nonce, responder_nonce);

    // Constant time: a byte-by-byte comparison leaks how much of the proof was right, which is
    // enough to forge one a guess at a time.
    match hmac::verify(&key, &expected, received_proof) {
        Ok(()) => EnrollmentResult::Accepted,
        Err(_) => EnrollmentResult::WrongKey,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CODE: &str = "TESTKEY00000001";
    const ALICE: &str = "AAAA1111";
    const BOB: &str = "BBBB2222";

    fn nonces() -> ([u8; NONCE_SIZE], [u8; NONCE_SIZE]) {
        (create_nonce(), create_nonce())
    }

    #[test]
    fn both_sides_compute_the_same_proof_for_the_same_exchange() {
        let (initiator, responder) = nonces();
        let from_alice = compute_proof(CODE, ALICE, BOB, &initiator, &responder);
        let from_bobs_view = compute_proof(CODE, ALICE, BOB, &initiator, &responder);

        assert_eq!(from_alice, from_bobs_view);
        assert_eq!(EnrollmentResult::Accepted, verify(CODE, ALICE, BOB, &initiator, &responder, &from_alice));
    }

    #[test]
    fn the_wrong_code_is_refused() {
        let (initiator, responder) = nonces();
        let proof = compute_proof(CODE, ALICE, BOB, &initiator, &responder);
        assert_eq!(EnrollmentResult::WrongKey, verify("DIFFERENT", ALICE, BOB, &initiator, &responder, &proof));
    }

    #[test]
    fn a_proof_for_different_certificates_is_refused() {
        // The interceptor case: it must present its own certificate to each honest machine, so the
        // thumbprints it is observed with differ from those the proof was computed over.
        let (initiator, responder) = nonces();
        let genuine = compute_proof(CODE, ALICE, BOB, &initiator, &responder);
        const ATTACKER: &str = "CCCC3333";

        assert_eq!(EnrollmentResult::WrongKey, verify(CODE, ALICE, ATTACKER, &initiator, &responder, &genuine));
        assert_eq!(EnrollmentResult::WrongKey, verify(CODE, ATTACKER, BOB, &initiator, &responder, &genuine));
    }

    #[test]
    fn swapping_the_certificate_order_changes_the_proof() {
        // Direction must matter, or a proof computed by one side would validate as the other's.
        let (initiator, responder) = nonces();
        assert_ne!(
            compute_proof(CODE, ALICE, BOB, &initiator, &responder),
            compute_proof(CODE, BOB, ALICE, &initiator, &responder)
        );
    }

    #[test]
    fn replaying_a_proof_with_fresh_nonces_fails() {
        let (initiator, responder) = nonces();
        let proof = compute_proof(CODE, ALICE, BOB, &initiator, &responder);
        let (fresh_initiator, fresh_responder) = nonces();

        assert_eq!(EnrollmentResult::WrongKey, verify(CODE, ALICE, BOB, &fresh_initiator, &fresh_responder, &proof));
    }

    #[test]
    fn thumbprint_casing_does_not_change_the_outcome() {
        let (initiator, responder) = nonces();
        let proof = compute_proof(CODE, "abcd1234", "ef567890", &initiator, &responder);
        assert_eq!(EnrollmentResult::Accepted, verify(CODE, "ABCD1234", "EF567890", &initiator, &responder, &proof));
    }

    #[test]
    fn malformed_input_is_rejected_without_comparing() {
        let (initiator, responder) = nonces();
        assert_eq!(EnrollmentResult::Malformed, verify(CODE, ALICE, BOB, &initiator, &responder, &[0u8; 8]));
        assert_eq!(EnrollmentResult::Malformed, verify(CODE, ALICE, BOB, &[0u8; 4], &responder, &[0u8; 32]));
    }
}
