// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! AES-256-GCM authenticated encryption.
//!
//! AES-256-GCM is FIPS-approved (NIST SP 800-38D).  Routed through
//! [`crate::backend`], the pure-Rust (RustCrypto) implementation.
//! Used internally by `envelope` for decrypting objects, and exported
//! for any caller that needs raw decrypt without the full envelope
//! wrapper.
//!
//! See data format spec Section 3.

use bkp_types::error::Result;

use crate::keys::SubKey;

/// An encrypted blob: 12-byte random nonce followed by ciphertext-with-appended-tag.
///
/// The `aes-gcm` crate appends the 16-byte GCM authentication tag directly to
/// the ciphertext, so `ciphertext.len() == plaintext.len() + 16`.
#[derive(Debug, Clone)]
pub struct EncryptedBlob {
    /// 12-byte AES-GCM nonce (randomly generated per encryption).
    pub nonce: [u8; 12],
    /// Ciphertext with 16-byte GCM tag appended (`plaintext.len() + 16` bytes).
    pub ciphertext: Vec<u8>,
}

/// Decrypt `blob` under `key`.
pub fn decrypt(key: &SubKey, blob: &EncryptedBlob) -> Result<Vec<u8>> {
    decrypt_with_aad(key, blob, &[])
}

/// Decrypt `blob` under `key` with additional authenticated data (`aad`).
///
/// The `aad` must exactly match what was supplied to [`encrypt_with_aad`].
pub fn decrypt_with_aad(key: &SubKey, blob: &EncryptedBlob, aad: &[u8]) -> Result<Vec<u8>> {
    crate::backend::aes_gcm_decrypt(key.as_bytes(), &blob.nonce, aad, &blob.ciphertext)
}
