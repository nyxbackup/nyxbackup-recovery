// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! Encryption envelope encode/decode.
//!
//! Wraps remote objects (manifests, snapshot indexes, machine records,
//! pack indexes) with AES-256-GCM authenticated encryption.  The
//! envelope header doubles as AAD, binding the ciphertext to its
//! metadata (key label, nonce, plaintext length).
//!
//! AES-256-GCM is the only cipher.  No alternate AEAD path is used:
//! AES-NI / ARM Crypto Extensions are universal on every CPU shipping
//! since 2016, and only AES-GCM is in NIST's approved AEAD list
//! (SP 800-38D).
//!
//! All cryptography routes through [`crate::aead`], which dispatches
//! to the platform vendor's FIPS-validated AES-GCM via
//! [`crate::backend`] (BCrypt on Windows, CoreCrypto on macOS,
//! aws-lc-fips-sys on Linux).
//!
//! # Format
//!
//! Single v2 format.  v1 envelopes from earlier are not decoded;
//! cross-edition mis-decode is blocked by the bumped
//! `CURRENT_FORMAT_VERSION` machinery in `bkp-manifest`.
//!
//! ```text
//!  Offset  Width  Field
//!       0      4  magic: ASCII "BKEV"
//!       4      2  format_version: u16 big-endian = 2
//!       6      1  key_label_id: u8
//!       7      1  cipher_id: 0 = AES-256-GCM (only value supported)
//!       8     24  nonce: 24 bytes (AES-GCM uses bytes 0..12)
//!      32      4  plaintext_length: u32 big-endian
//!      36      4  ciphertext_length: u32 big-endian
//!  [40 bytes header total - used as AAD]
//!      40      *  ciphertext (AES-256-GCM)
//!       *     16  AES-GCM authentication tag
//! ```
//!
//! The trailing 12 bytes of the 24-byte nonce field are unused for
//! AES-GCM but retained in the layout for forward compatibility
//! (avoids a fourth format-version bump if a future cipher needs the
//! wider nonce).

use bkp_types::error::{Error, Result};

use crate::keys::SubKey;

const MAGIC: &[u8; 4] = b"BKEV";
const FORMAT_VERSION: u16 = 2;
const HEADER_V2_LEN: usize = 40;

/// Cipher identifier stored in v2 envelope header byte 7.
///
/// Only AES-256-GCM (cipher_id 0) is supported.  Objects written by an
/// earlier build with a different runtime-selected cipher (cipher_id 1
/// or 2) are refused; re-run a backup on the source machine to rewrite
/// them in the current format.  See `decode()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CipherId {
    /// AES-256-GCM (NIST SP 800-38D).  The only cipher supported in
    /// v2 envelopes.
    Aes256Gcm = 0,
}

impl CipherId {
    fn from_u8(b: u8) -> Result<Self> {
        match b {
            0 => Ok(Self::Aes256Gcm),
            _ => Err(Error::Crypto(format!(
                "unsupported cipher_id {b} (only AES-256-GCM is approved)"
            ))),
        }
    }
}

/// Decode an encryption envelope, verifying the authentication tag.
///
/// `key` must correspond to the `key_label_id` recorded in the header.
/// Only v2 / AES-256-GCM envelopes are accepted; v1 envelopes from
/// earlier builds and unknown cipher_id values are refused.
pub fn decode(key: &SubKey, data: &[u8]) -> Result<Vec<u8>> {
    if data.len() < 8 {
        return Err(Error::Crypto(format!(
            "envelope too short: {} bytes",
            data.len()
        )));
    }

    if &data[0..4] != MAGIC {
        return Err(Error::Crypto(format!(
            "bad envelope magic: {:02x?}",
            &data[0..4]
        )));
    }

    let version = u16::from_be_bytes([data[4], data[5]]);
    if version != FORMAT_VERSION {
        return Err(Error::Crypto(format!(
            "envelope format_version {version} not supported \
             (only v{FORMAT_VERSION})"
        )));
    }

    if data.len() < HEADER_V2_LEN + 16 {
        return Err(Error::Crypto(format!(
            "envelope too short: {} bytes (minimum {})",
            data.len(),
            HEADER_V2_LEN + 16
        )));
    }

    let header = &data[0..HEADER_V2_LEN];
    let cipher_id = CipherId::from_u8(header[7])?;
    let nonce24: [u8; 24] = header[8..32].try_into().expect("24 from 24");
    let plaintext_length =
        u32::from_be_bytes([header[32], header[33], header[34], header[35]]) as usize;

    let expected_remaining = plaintext_length + 16;
    let actual_remaining = data.len() - HEADER_V2_LEN;
    if actual_remaining != expected_remaining {
        return Err(Error::Crypto(format!(
            "envelope body length mismatch: expected {expected_remaining}, got {actual_remaining}"
        )));
    }

    // Only AES-256-GCM is supported.
    let CipherId::Aes256Gcm = cipher_id;
    let ciphertext_with_tag = &data[HEADER_V2_LEN..];
    let nonce12: [u8; 12] = nonce24[0..12].try_into().expect("12 bytes from 24");
    crate::backend::aes_gcm_decrypt(key.as_bytes(), &nonce12, header, ciphertext_with_tag)
}

/// Return the `key_label_id` byte from `data` without decrypting.
///
/// Useful for selecting the correct subkey before calling [`decode`].
pub fn peek_label_id(data: &[u8]) -> Result<u8> {
    if data.len() < 8 {
        return Err(Error::Crypto("envelope too short to read label".into()));
    }
    Ok(data[6])
}

/// Return the [`CipherId`] from a v2 envelope header without decrypting.
pub fn peek_cipher_id(data: &[u8]) -> Result<CipherId> {
    if data.len() < 8 {
        return Err(Error::Crypto("envelope too short".into()));
    }
    let version = u16::from_be_bytes([data[4], data[5]]);
    if version != FORMAT_VERSION {
        return Err(Error::Crypto(format!(
            "envelope format_version {version} not supported"
        )));
    }
    CipherId::from_u8(data[7])
}
