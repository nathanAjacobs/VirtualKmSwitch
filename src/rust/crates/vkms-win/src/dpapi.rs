//! DPAPI: encrypting data to the account this process runs as.
//!
//! Used for the node's TLS private key. Under the service that account is LocalSystem, so the key on
//! disk can be decrypted only by code already running as SYSTEM -- which is the same audience a
//! machine key store would admit, without tying the key to a particular store implementation.

use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
};

use crate::{wide, Win32Error};

fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
    CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 }
}

fn take(output: CRYPT_INTEGER_BLOB) -> Vec<u8> {
    let bytes = unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) }.to_vec();
    unsafe { LocalFree(output.pbData as _) };
    bytes
}

/// Encrypts `data` so only this account on this machine can read it back.
pub fn protect(data: &[u8], description: &str) -> Result<Vec<u8>, Win32Error> {
    let input = blob(data);
    let description = wide(description);
    let mut output = CRYPT_INTEGER_BLOB { cbData: 0, pbData: null_mut() };

    let ok = unsafe {
        CryptProtectData(&input, description.as_ptr(), null(), null(), null(), CRYPTPROTECT_UI_FORBIDDEN, &mut output)
    };

    if ok == 0 {
        return Err(Win32Error::last("CryptProtectData failed"));
    }

    Ok(take(output))
}

/// Decrypts data produced by [`protect`] under the same account.
pub fn unprotect(data: &[u8]) -> Result<Vec<u8>, Win32Error> {
    let input = blob(data);
    let mut output = CRYPT_INTEGER_BLOB { cbData: 0, pbData: null_mut() };

    let ok = unsafe {
        CryptUnprotectData(&input, null_mut(), null(), null(), null(), CRYPTPROTECT_UI_FORBIDDEN, &mut output)
    };

    if ok == 0 {
        return Err(Win32Error::last("CryptUnprotectData failed"));
    }

    Ok(take(output))
}

#[cfg(test)]
mod tests {
    #[test]
    fn data_round_trips_and_is_not_stored_in_the_clear() {
        let secret = b"a private key, notionally";
        let sealed = super::protect(secret, "test").unwrap();

        assert!(!sealed.windows(secret.len()).any(|window| window == secret));
        assert_eq!(secret.to_vec(), super::unprotect(&sealed).unwrap());
    }

    #[test]
    fn tampered_data_is_refused() {
        let mut sealed = super::protect(b"payload", "test").unwrap();
        let last = sealed.len() - 1;
        sealed[last] ^= 0xFF;
        assert!(super::unprotect(&sealed).is_err());
    }
}
