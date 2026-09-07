// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! SHA-256 chunk hashing.
//!
//! The chunk ID is the SHA-256 hash of the **plaintext** content - after
//! decompression, before encryption.  This is the content address used for
//! deduplication.  See data format spec Section 13 (manifest format v2).
//!
//! SHA-256 is FIPS 180-4 approved.  Hardware-accelerated everywhere that
//! matters (Intel + AMD SHA-NI since 2016/2017; ARMv8 Crypto Extensions
//! universal on modern phones, Apple Silicon, AWS Graviton).  Per-core
//! throughput is ~1.5-2 GB/s on commodity hardware, and FastCDC produces
//! independent ~4 MiB chunks we parallelise across cores anyway, so any
//! single-stream SIMD advantage of a non-validated alternative would not
//! translate to aggregate throughput here.

use bkp_types::chunk::ChunkId;

use crate::keys::SubKey;

/// Width of a content-hash output in bytes.
///
/// Used as a chunk ID width, manifest tag width, and authenticator width
/// throughout the format.  32 bytes - matches both SHA-256 (current) and
/// SHA-384 truncated to 32 (potential future).  Where the value sits inside
/// a fixed-size array literal (`[u8; 32]`), prefer routing through this
/// crate's public `ChunkId` / `MacTag` types when possible.
pub const HASH_LEN: usize = 32;

/// Hash `data` with SHA-256 and return it as a [`ChunkId`].
///
/// **Use [`keyed_chunk_id`] for new code.**  The plain (unkeyed) hash exposes
/// a "is file X in this backup?" oracle to anyone holding decrypted manifests
/// (see DESIGN.md §4.1).  This function is retained for tests and for places
/// where unkeyed hashing is genuinely required (cross-snapshot dedup probes,
/// some integrity checks).
pub fn sha256_hash(data: &[u8]) -> ChunkId {
    ChunkId::from_bytes(crate::backend::sha256_digest(data))
}

/// Compute a per-backup-set chunk identity using HMAC-SHA256.
///
/// `chunk_id_key` must be a 32-byte HKDF subkey derived with
/// [`KeyLabel::ChunkIdentity`](crate::keys::KeyLabel::ChunkIdentity).
///
/// Within one backup set the result is deterministic (so dedup works); across
/// sets the same plaintext produces different identities (so an attacker
/// cannot fingerprint files by hashing candidates and probing the manifest).
///
/// SHA-256 has no native keyed mode; HMAC-SHA256 is the FIPS-approved
/// equivalent and provides the same per-set-key collision resistance.
pub fn keyed_chunk_id(chunk_id_key: &SubKey, data: &[u8]) -> ChunkId {
    ChunkId::from_bytes(crate::backend::hmac_sha256_raw(
        chunk_id_key.as_bytes(),
        data,
    ))
}

/// SHA-256 hex digest of `data`.
///
/// Used by callers that need a NIST-approved digest *outside* the
/// content-addressing path - notably AWS SigV4 canonical-request hashing
/// (bkp-storage's S3 backend) and webhook signature verification
/// (bkp-webhook).
///
/// SHA-256 is FIPS 180-4 approved.  Routed through [`crate::backend`]
/// so the active build's backend (pure-Rust by default, aws-lc-rs under
/// the `fips` feature) is used.  See [`crate::backend`] for the full
/// dispatch story.
pub fn sha256_hex(data: &[u8]) -> String {
    crate::backend::sha256_hex(data)
}

/// Raw 32-byte SHA-256 digest of `data`.  See [`sha256_hex`] for rationale.
pub fn sha256_digest(data: &[u8]) -> [u8; 32] {
    crate::backend::sha256_digest(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_is_deterministic() {
        // Known SHA-256 of empty string.
        let id = sha256_hash(&[]);
        assert_eq!(
            hex::encode(id.as_bytes()),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn same_content_same_id() {
        assert_eq!(sha256_hash(b"hello world"), sha256_hash(b"hello world"));
    }

    #[test]
    fn different_content_different_id() {
        assert_ne!(sha256_hash(b"hello"), sha256_hash(b"world"));
    }

    fn k(byte: u8) -> SubKey {
        SubKey::from_bytes([byte; 32])
    }

    #[test]
    fn keyed_hash_is_deterministic_within_key() {
        // Same plaintext + same key → same chunk_id (so dedup works inside
        // a backup set).
        assert_eq!(
            keyed_chunk_id(&k(0x11), b"hello world"),
            keyed_chunk_id(&k(0x11), b"hello world"),
        );
    }

    #[test]
    fn keyed_hash_different_keys_give_different_ids() {
        // Same plaintext + different keys → different chunk_ids.  This is
        // the privacy property: an attacker without the per-set key cannot
        // hash a candidate plaintext and look it up in the manifest.
        assert_ne!(
            keyed_chunk_id(&k(0x11), b"sensitive document content"),
            keyed_chunk_id(&k(0x22), b"sensitive document content"),
        );
    }

    #[test]
    fn keyed_hash_differs_from_unkeyed() {
        // A keyed hash of the same plaintext must not coincide with the
        // unkeyed hash, otherwise migration from old backups would silently
        // accept stale chunk_ids.
        let pt = b"some chunk plaintext";
        assert_ne!(
            keyed_chunk_id(&k(0x33), pt).as_bytes(),
            sha256_hash(pt).as_bytes(),
        );
    }
}
