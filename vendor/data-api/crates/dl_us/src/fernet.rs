//! Fernet decryption for the `cypher_text` secrets `us` stores.
//!
//! A connection entry never carries its password in `data`: `unversioned_data.password` is
//! `{"key_id": "KEY", "key_kind": "local_fernet", "cypher_text": "gAAAAA..."}`. Python decrypts
//! it with `dl_core.utils.CryptoKeys` (the key is `CONTROL_API_CRYPTO_KEY`, base64-encoded), and
//! the data-api reproduces that here, because without the password the connection cannot be
//! opened at all.
//!
//! The format is the standard one (`cryptography.fernet`):
//!
//! ```text
//! token = base64url( 0x80 | timestamp:8 | iv:16 | ciphertext | hmac:32 )
//! key   = base64url( signing:16 | encryption:16 )        # 32 bytes
//! ```
//!
//! The HMAC-SHA256 covers everything before it and is verified *before* decrypting, so a
//! tampered token is rejected without touching AES.

use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use std::time::{SystemTime, UNIX_EPOCH};
use base64::engine::general_purpose::URL_SAFE;
use base64::Engine;
use hmac::{Hmac, Mac};
use sha2::Sha256;

type Aes128Cbc = cbc::Decryptor<aes::Aes128>;
type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;

/// Fernet's first byte, `Version`.
const VERSION: u8 = 0x80;
/// `timestamp (8) + iv (16)`.
const HEADER_LEN: usize = 1 + 8 + 16;
/// `hmac (32)`.
const MAC_LEN: usize = 32;

/// A 32-byte Fernet key, split the way the format requires.
#[derive(Clone, Copy)]
pub struct FernetKey {
    signing: [u8; 16],
    encryption: [u8; 16],
}

impl FernetKey {
    /// Decode a `CONTROL_API_CRYPTO_KEY` value (url-safe base64, 32 bytes).
    pub fn from_base64(encoded: &str) -> Option<Self> {
        let raw = URL_SAFE.decode(encoded.trim()).ok()?;
        Self::from_bytes(&raw)
    }

    /// The key a `us` secret was encrypted with.
    pub fn from_bytes(raw: &[u8]) -> Option<Self> {
        if raw.len() != 32 {
            return None;
        }
        let mut signing = [0u8; 16];
        let mut encryption = [0u8; 16];
        signing.copy_from_slice(&raw[..16]);
        encryption.copy_from_slice(&raw[16..]);
        Some(Self {
            signing,
            encryption,
        })
    }

    /// Encrypt one secret into the standard fernet token format. The IV comes from the OS
    /// CSPRNG and the timestamp is the current Unix time, so tokens are not deterministic.
    pub fn encrypt(&self, plaintext: &str) -> Result<String, FernetError> {
        let mut iv = [0u8; 16];
        getrandom::getrandom(&mut iv).map_err(|_| FernetError::Random)?;
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| FernetError::Random)?
            .as_secs();
        let mut body = Vec::with_capacity(HEADER_LEN + plaintext.len() + 16);
        body.push(VERSION);
        body.extend_from_slice(&timestamp.to_be_bytes());
        body.extend_from_slice(&iv);
        let ciphertext = Aes128CbcEnc::new((&self.encryption).into(), (&iv).into())
            .encrypt_padded_vec_mut::<Pkcs7>(plaintext.as_bytes());
        body.extend_from_slice(&ciphertext);
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.signing)
            .map_err(|_| FernetError::Malformed("invalid signing key"))?;
        mac.update(&body);
        let signature = mac.finalize().into_bytes();
        body.extend_from_slice(&signature);
        Ok(URL_SAFE.encode(body))
    }

    /// Decrypt one token; the plaintext is returned as UTF-8.
    pub fn decrypt(&self, token: &str) -> Result<String, FernetError> {
        let raw = URL_SAFE
            .decode(token.trim())
            .map_err(|_| FernetError::Malformed("token is not url-safe base64"))?;
        if raw.len() < HEADER_LEN + MAC_LEN + 16 {
            return Err(FernetError::Malformed("token is too short"));
        }
        if raw[0] != VERSION {
            return Err(FernetError::Malformed("unsupported token version"));
        }
        let (body, signature) = raw.split_at(raw.len() - MAC_LEN);
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.signing)
            .map_err(|_| FernetError::Malformed("invalid signing key"))?;
        mac.update(body);
        mac.verify_slice(signature)
            .map_err(|_| FernetError::BadSignature)?;

        let iv = &body[9..HEADER_LEN];
        let ciphertext = &body[HEADER_LEN..];
        let plaintext = Aes128Cbc::new((&self.encryption).into(), iv.into())
            .decrypt_padded_vec_mut::<Pkcs7>(ciphertext)
            .map_err(|_| FernetError::Malformed("ciphertext is not padded AES-CBC"))?;
        String::from_utf8(plaintext).map_err(|_| FernetError::Malformed("secret is not UTF-8"))
    }
}

impl std::fmt::Debug for FernetKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the key material.
        formatter.write_str("FernetKey(..)")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum FernetError {
    #[error("malformed fernet token: {0}")]
    Malformed(&'static str),
    #[error("fernet token signature does not match the key")]
    BadSignature,
    #[error("fernet nonce generation failed")]
    Random,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `CONTROL_API_CRYPTO_KEY` of the compose file.
    const KEY: &str = "ZGF0YWxlbnN1bnNlY3VyZWZlcm5ldGNyeXB0b2tleQo=";
    /// `Fernet(KEY).encrypt(b"postgres")`, the demo connection password.
    const TOKEN: &str = "gAAAAABq6isD_ZFhpnoozjxV5V7nSbzvf5fFsiBOnkGMgsc4FUvd3GOYAyeS_FMzGHTBfKE4QUbyvLPs5Y08zJ_ulicYEnkOgg==";

    #[test]
    fn a_secret_is_decrypted_with_the_compose_key() {
        let key = FernetKey::from_base64(KEY).unwrap();
        assert_eq!(key.decrypt(TOKEN).unwrap(), "postgres");
    }

    #[test]
    fn a_token_from_another_key_is_rejected() {
        let mut raw = URL_SAFE.decode(KEY).unwrap();
        // Flip a byte of the signing half: the HMAC must fail.
        raw[0] ^= 0x01;
        let key = FernetKey::from_bytes(&raw).unwrap();
        assert_eq!(key.decrypt(TOKEN), Err(FernetError::BadSignature));
    }

    #[test]
    fn a_tampered_token_is_rejected_before_decryption() {
        let key = FernetKey::from_base64(KEY).unwrap();
        let mut raw = URL_SAFE.decode(TOKEN).unwrap();
        let last = raw.len() - 1;
        raw[last] ^= 0x01;
        let tampered = URL_SAFE.encode(raw);
        assert_eq!(key.decrypt(&tampered), Err(FernetError::BadSignature));
    }

    #[test]
    fn a_key_of_the_wrong_length_is_not_a_fernet_key() {
        assert!(FernetKey::from_base64("c2hvcnQ=").is_none());
        assert!(FernetKey::from_base64("not base64!!").is_none());
    }

    #[test]
    fn encrypt_round_trips_and_uses_a_random_iv() {
        let key = FernetKey::from_base64(KEY).unwrap();
        let first = key.encrypt("postgres").unwrap();
        let second = key.encrypt("postgres").unwrap();
        assert_ne!(first, second);
        assert_eq!(key.decrypt(&first).unwrap(), "postgres");
        assert_eq!(key.decrypt(&second).unwrap(), "postgres");
    }
}
