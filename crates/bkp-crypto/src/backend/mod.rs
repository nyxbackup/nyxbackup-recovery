// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! Pure-Rust crypto backend (RustCrypto).
//!
//! The recovery tool only decrypts and verifies, using standard algorithms:
//! AES-256-GCM (NIST SP 800-38D), SHA-256, HMAC-SHA256 (FIPS 198-1),
//! HKDF-SHA256 (RFC 5869), and PBKDF2-HMAC-SHA256 (RFC 8018).  These are
//! interoperable by definition with whatever module produced the backup
//! (CNG / CoreCrypto / AWS-LC on the writing side), so decryption is
//! byte-identical regardless of implementation.
//!
//! The main Nyx Backup product uses per-OS vendor-validated modules for its
//! FIPS positioning; the recovery tool deliberately does not - it is the free,
//! open, "always works" reader and has no FIPS claim to maintain.  Using
//! RustCrypto here keeps the build small and the implementation single and
//! auditable.

use bkp_types::error::{Error, Result};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

// - SHA-256 ------------------------------------------

/// SHA-256 hex digest of `data`.
pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// Raw 32-byte SHA-256 digest of `data`.
pub fn sha256_digest(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

// - HMAC-SHA256 --------------------------------------

/// HMAC-SHA256 over `data` with an arbitrary-byte key.
pub fn hmac_sha256_raw(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().into()
}

/// Constant-time check of lowercase-hex HMAC-SHA256 against `expected_hex`.
pub fn hmac_sha256_hex_verify(secret: &[u8], data: &[u8], expected_hex: &str) -> bool {
    let computed_hex = hex::encode(hmac_sha256_raw(secret, data));
    if computed_hex.len() != expected_hex.len() {
        return false;
    }
    computed_hex
        .bytes()
        .zip(expected_hex.bytes())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

// - AES-256-GCM AEAD (decrypt only) ------------------

/// AES-256-GCM decrypt.  `ciphertext` is the sealed bytes with the 16-byte
/// tag appended (the layout AES-GCM seal produces), and `aad` must match the
/// additional authenticated data supplied at seal time.
pub fn aes_gcm_decrypt(
    key: &[u8; 32],
    nonce: &[u8; 12],
    aad: &[u8],
    ciphertext: &[u8],
) -> Result<Vec<u8>> {
    use aes_gcm::aead::{Aead, KeyInit, Payload};
    use aes_gcm::{Aes256Gcm, Key, Nonce};
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    cipher
        .decrypt(
            Nonce::from_slice(nonce),
            Payload {
                msg: ciphertext,
                aad,
            },
        )
        .map_err(|_| {
            Error::Crypto("AES-GCM decrypt failed (wrong key, AAD, or corrupted data)".into())
        })
}

// - HKDF-SHA256 --------------------------------------

/// HKDF-SHA256.  A `salt` of `None` substitutes a 32-byte zero string per
/// RFC 5869.
pub fn hkdf_sha256(
    salt: Option<&[u8]>,
    ikm: &[u8],
    info: &[u8],
    out_len: usize,
) -> Result<Vec<u8>> {
    let salt_bytes: &[u8] = salt.unwrap_or(&[0u8; 32]);
    let hk = hkdf::Hkdf::<Sha256>::new(Some(salt_bytes), ikm);
    let mut out = vec![0u8; out_len];
    hk.expand(info, &mut out)
        .map_err(|e| Error::Crypto(format!("HKDF expand: {e}")))?;
    Ok(out)
}

// - PBKDF2-HMAC-SHA256 -------------------------------

/// PBKDF2 with an HMAC-SHA256 PRF (RFC 8018) - used to re-derive the master
/// key from a recovery passphrase.
pub fn pbkdf2_hmac_sha256(
    password: &[u8],
    salt: &[u8],
    iterations: u32,
    out_len: usize,
) -> Result<Vec<u8>> {
    if iterations == 0 {
        return Err(Error::Crypto("PBKDF2 iterations must be non-zero".into()));
    }
    let mut out = vec![0u8; out_len];
    pbkdf2::pbkdf2_hmac::<Sha256>(password, salt, iterations, &mut out);
    Ok(out)
}

#[cfg(test)]
mod conformance {
    //! Known-answer tests: the pure-Rust backend must match the published
    //! standards, which guarantees it decrypts data produced by any other
    //! conformant implementation (CNG / CoreCrypto / AWS-LC).
    use super::*;

    #[test]
    fn sha256_kat() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn hmac_sha256_kat_rfc4231() {
        // RFC 4231 test case 1.
        let key = [0x0bu8; 20];
        let tag = hmac_sha256_raw(&key, b"Hi There");
        assert_eq!(
            hex::encode(tag),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[test]
    fn hkdf_sha256_kat_rfc5869() {
        // RFC 5869 test case 1.
        let ikm = [0x0bu8; 22];
        let salt: Vec<u8> = (0..=0x0cu8).collect();
        let info: Vec<u8> = (0xf0u8..=0xf9u8).collect();
        let okm = hkdf_sha256(Some(&salt), &ikm, &info, 42).unwrap();
        assert_eq!(
            hex::encode(okm),
            "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865"
        );
    }

    #[test]
    fn pbkdf2_sha256_kat() {
        // P="password", S="salt", c=1, dkLen=32.
        let out = pbkdf2_hmac_sha256(b"password", b"salt", 1, 32).unwrap();
        assert_eq!(
            hex::encode(out),
            "120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b"
        );
    }

    #[test]
    fn aes_256_gcm_decrypt_kat() {
        // NIST AES-256-GCM: all-zero key + IV, empty AAD + plaintext -> tag only.
        let key = [0u8; 32];
        let nonce = [0u8; 12];
        let tag = hex::decode("530f8afbc74536b9a963b4f1c4cb738b").unwrap();
        let pt = aes_gcm_decrypt(&key, &nonce, &[], &tag).unwrap();
        assert!(pt.is_empty());
        // Wrong AAD must fail (authentication).
        assert!(aes_gcm_decrypt(&key, &nonce, b"x", &tag).is_err());
    }
}
