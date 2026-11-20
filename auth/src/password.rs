//! Password hashing/verification compatible with the Node service.
//!
//! The Node code uses `crypto.scrypt(password, salt, 64)` with default parameters
//! (`N=16384, r=8, p=1, maxmem=32 MiB`), a 16-byte random salt, and stores
//! `base64url(salt) + ":" + base64url(derivedKey)`; comparison is `timingSafeEqual`.
//! The same derivation is reproduced here with the RustCrypto `scrypt` crate, and the
//! comparison is constant-time over the full 64-byte key.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use scrypt::{scrypt, Params};

const LOG_N: u8 = 14; // N = 16384
const R: u32 = 8;
const P: u32 = 1;
const KEY_LEN: usize = 64;
const SALT_LEN: usize = 16;

fn derive(password: &str, salt: &[u8]) -> Option<[u8; KEY_LEN]> {
    let params = Params::new(LOG_N, R, P).ok()?;
    let mut output = [0u8; KEY_LEN];
    scrypt(password.as_bytes(), salt, &params, &mut output).ok()?;
    Some(output)
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0u8;
    for (a, b) in left.iter().zip(right) {
        difference |= a ^ b;
    }
    difference == 0
}

/// Verify a stored `salt:key` hash (base64url, no padding) against a plaintext password.
pub fn verify_password(stored: &str, password: &str) -> bool {
    let Some((salt_b64, key_b64)) = stored.split_once(':') else {
        return false;
    };
    let (Ok(salt), Ok(expected)) = (URL_SAFE_NO_PAD.decode(salt_b64), URL_SAFE_NO_PAD.decode(key_b64))
    else {
        return false;
    };
    if expected.len() != KEY_LEN {
        return false;
    }
    let Some(derived) = derive(password, &salt) else {
        return false;
    };
    constant_time_eq(&derived, &expected)
}

/// Produce a new `salt:key` hash in the same format (used by `/signup`).
pub fn hash_password(password: &str) -> Option<String> {
    let mut salt = [0u8; SALT_LEN];
    getrandom::getrandom(&mut salt).ok()?;
    let derived = derive(password, &salt)?;
    Some(format!(
        "{}:{}",
        URL_SAFE_NO_PAD.encode(salt),
        URL_SAFE_NO_PAD.encode(derived)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Produced by the reference Node service:
    /// `crypto.scrypt('test-password', Buffer.from('0123456789abcdef'), 64)`.
    const NODE_VECTOR: &str = "MDEyMzQ1Njc4OWFiY2RlZg:P7LO0VyeojNF-PzPS0VF4tYr0K5xxUzsk6qRsYZmqd0h4IXcOx8fFxsMJMpIME1Q0ezJXvUEak7dsbIg5gnVBg";

    #[test]
    fn verifies_hash_produced_by_node() {
        assert!(verify_password(NODE_VECTOR, "test-password"));
        assert!(!verify_password(NODE_VECTOR, "test-passwore"));
        assert!(!verify_password(NODE_VECTOR, ""));
        assert!(!verify_password("not-a-hash", "test-password"));
        assert!(!verify_password("AAAA:AAAA", "test-password"));
    }

    #[test]
    fn generated_hash_round_trips() {
        let hash = hash_password("S3cret!pass").expect("hash");
        assert_eq!(hash.matches(':').count(), 1);
        assert!(verify_password(&hash, "S3cret!pass"));
        assert!(!verify_password(&hash, "S3cret!pas"));
    }
}
