//! The code a person reads off one screen and types into another.
//!
//! Everything about the format serves that: the alphabet omits the characters that get confused
//! when read aloud or off a monitor, and the groups of four make a long code checkable at a glance.
//! A code nobody can transcribe reliably gets pasted into a chat window instead, which is a far
//! worse outcome than a slightly longer one.

use ring::rand::{SecureRandom, SystemRandom};

/// Thirty-two characters, with I, O, 0 and 1 removed. A power of two, so every random byte maps to
/// a character without bias and without a rejection loop.
const ALPHABET: &[u8; 32] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

const GROUPS: usize = 4;
const GROUP_LENGTH: usize = 4;

pub struct PairingCode;

impl PairingCode {
    /// Characters of entropy, five bits each: 80 bits in total.
    ///
    /// Long for a one-time code, deliberately. The pairing window advertises a hash of it and an
    /// accepted handshake reveals an HMAC over it, so anyone on the network can attempt to recover it
    /// offline while the window is open. At 80 bits that is not a race they can win.
    pub const SIGNIFICANT_CHARACTERS: usize = GROUPS * GROUP_LENGTH;

    /// Creates a fresh code, formatted as XXXX-XXXX-XXXX-XXXX.
    pub fn create() -> String {
        let mut random = [0u8; Self::SIGNIFICANT_CHARACTERS];
        SystemRandom::new().fill(&mut random).expect("the system random number generator failed");

        let mut formatted = String::with_capacity(Self::SIGNIFICANT_CHARACTERS + GROUPS - 1);
        for (index, byte) in random.iter().enumerate() {
            if index > 0 && index % GROUP_LENGTH == 0 {
                formatted.push('-');
            }

            // Masking is unbiased only because the alphabet length divides 256 exactly.
            formatted.push(ALPHABET[(byte & 0x1F) as usize] as char);
        }

        formatted
    }

    /// Reduces a typed code to its canonical form.
    ///
    /// Someone reading a code aloud will produce spaces, missing dashes and the wrong case, and none
    /// of those should be the difference between pairing and being told the code is wrong. Only
    /// genuinely different characters are allowed to fail.
    pub fn normalize(typed: &str) -> String {
        typed
            .chars()
            .filter(|c| *c != '-' && !c.is_whitespace())
            .take(64)
            .map(|c| c.to_ascii_uppercase())
            .collect()
    }

    /// The publicly advertisable id for a normalised code.
    ///
    /// A beacon carries this so the machine the code is typed into can find the machine showing it.
    /// It is a one-way function, so advertising it reveals nothing short of inverting SHA-256 over
    /// an 80-bit input.
    pub fn advertised_id(normalized: &str) -> String {
        let digest = ring::digest::digest(&ring::digest::SHA256, format!("vkms-pairing-v1:{normalized}").as_bytes());
        digest.as_ref()[..8].iter().map(|byte| format!("{byte:02X}")).collect()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn a_code_is_grouped_for_reading_aloud() {
        let code = PairingCode::create();
        assert_eq!("XXXX-XXXX-XXXX-XXXX".len(), code.len());

        let groups: Vec<_> = code.split('-').collect();
        assert_eq!(4, groups.len());
        assert!(groups.iter().all(|group| group.len() == 4));
    }

    #[test]
    fn the_alphabet_omits_characters_that_are_misread_off_a_screen() {
        for _ in 0..200 {
            let code = PairingCode::create();
            for excluded in ['I', 'O', '0', '1'] {
                assert!(!code.contains(excluded), "{code} contains {excluded}");
            }
        }
    }

    #[test]
    fn codes_do_not_repeat() {
        let mut seen = HashSet::new();
        for _ in 0..500 {
            assert!(seen.insert(PairingCode::create()));
        }
    }

    #[test]
    fn transcription_differences_do_not_change_the_code() {
        for typed in [
            "ABCD-EFGH-JKLM-NPQR",
            "abcd-efgh-jklm-npqr",
            "ABCDEFGHJKLMNPQR",
            "ABCD EFGH JKLM NPQR",
            "  ABCD-efgh JKLM-npqr  ",
        ] {
            assert_eq!("ABCDEFGHJKLMNPQR", PairingCode::normalize(typed), "{typed}");
        }
    }

    #[test]
    fn genuinely_different_codes_stay_different() {
        assert_ne!(PairingCode::normalize("ABCD-EFGH-JKLM-NPQR"), PairingCode::normalize("ABCD-EFGH-JKLM-NPQS"));
    }

    #[test]
    fn empty_input_normalises_to_empty_rather_than_failing() {
        assert_eq!("", PairingCode::normalize(""));
        assert_eq!("", PairingCode::normalize("   "));
    }

    #[test]
    fn the_advertised_id_is_stable_across_how_the_code_was_typed_and_hides_it() {
        let shown = PairingCode::advertised_id(&PairingCode::normalize("ABCD-EFGH-JKLM-NPQR"));
        let typed = PairingCode::advertised_id(&PairingCode::normalize("abcd efgh jklm npqr"));

        assert_eq!(shown, typed);
        assert_eq!(16, shown.len());
        assert!(!shown.contains("ABCD"));
        assert_ne!(shown, PairingCode::advertised_id("ABCDEFGHJKLMNPQS"));
    }
}
