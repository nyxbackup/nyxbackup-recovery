// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! HMAC-SHA256 manifest signing.
//!
//!
//! HMAC-SHA256 is FIPS-approved (NIST FIPS 198-1); the RustCrypto `hmac` crate used here is not FIPS-validated.  The FIPS fork replaces the underlying implementation with aws-lc-fips-sys.
//! Used to sign manifests: the tag is stored in the snapshot index entry so
//! integrity can be verified before downloading the full manifest.
//!
//! See data format spec Sections 3 and 9.2.

use bkp_types::error::{Error, Result};
use hmac::{Hmac, Mac};
// Both `Mac` and `KeyInit` define `new_from_slice`; we want the
// `Mac` variant.  Disambiguated with fully-qualified syntax at the
// call sites below, so we don't import `KeyInit` here.
use sha2::Sha256;

use crate::keys::SubKey;

type HmacSha256 = Hmac<Sha256>;

/// A 32-byte HMAC-SHA256 authentication tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HmacTag(pub [u8; 32]);

impl HmacTag {
    /// Return the raw tag bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Compute an HMAC-SHA256 tag over `data` using `key`.
pub fn sign(key: &SubKey, data: &[u8]) -> HmacTag {
    let mut mac =
        <HmacSha256 as Mac>::new_from_slice(key.as_bytes()).expect("HMAC accepts any key size");
    mac.update(data);
    let result = mac.finalize().into_bytes();
    let mut tag = [0u8; 32];
    tag.copy_from_slice(&result);
    HmacTag(tag)
}

/// Verify that `tag` is the correct HMAC-SHA256 of `data` under `key`.
///
/// Uses a constant-time comparison to prevent timing side-channels.
pub fn verify(key: &SubKey, data: &[u8], tag: &HmacTag) -> Result<()> {
    let mut mac =
        <HmacSha256 as Mac>::new_from_slice(key.as_bytes()).expect("HMAC accepts any key size");
    mac.update(data);
    mac.verify_slice(&tag.0)
        .map_err(|_| Error::Crypto("HMAC verification failed".into()))
}

/// HMAC-SHA256 over `data` with an arbitrary-byte key.  Returns the raw
/// 32-byte tag.
///
/// Used by callers that need HMAC-SHA256 outside the manifest-signing
/// `SubKey` path - specifically AWS SigV4's key-derivation chain and
/// webhook signature checks.  Routes through [`crate::backend`] so the
/// active build's backend (pure-Rust by default, aws-lc-rs under the
/// `fips` feature) is used.
pub fn hmac_sha256_raw(key: &[u8], data: &[u8]) -> [u8; 32] {
    crate::backend::hmac_sha256_raw(key, data)
}

/// Constant-time check that the lowercase-hex `expected_hex` matches
/// HMAC-SHA256(secret, data).  Returns false on any malformed input
/// rather than erroring, since this is meant for boundary-layer use
/// (webhook signature verification) where invalid hex equals "reject."
pub fn hmac_sha256_hex_verify(secret: &[u8], data: &[u8], expected_hex: &str) -> bool {
    crate::backend::hmac_sha256_hex_verify(secret, data, expected_hex)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::SubKey;

    fn key() -> SubKey {
        SubKey::from_bytes([0x11u8; 32])
    }

    #[test]
    fn sign_verify_roundtrip() {
        let tag = sign(&key(), b"manifest bytes");
        verify(&key(), b"manifest bytes", &tag).unwrap();
    }

    #[test]
    fn wrong_data_fails() {
        let tag = sign(&key(), b"manifest bytes");
        assert!(verify(&key(), b"other bytes", &tag).is_err());
    }

    #[test]
    fn wrong_key_fails() {
        let other = SubKey::from_bytes([0x22u8; 32]);
        let tag = sign(&key(), b"data");
        assert!(verify(&other, b"data", &tag).is_err());
    }
}
