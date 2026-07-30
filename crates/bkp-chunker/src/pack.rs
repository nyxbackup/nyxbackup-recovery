// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! Pack file format.
//!
//! A pack bundles multiple encrypted chunks into a single remote object,
//! amortising per-object upload overhead.
//!
//! ## Binary layout
//!
//! ```text
//! ┌-------------- Header (22 bytes) -------------┐
//! │  0.. 4   magic: b"BKPK"                                                  │
//! │  4.. 6   version: u16 big-endian, currently 1                            │
//! │  6..22   pack_id: UUID (16 bytes, big-endian)                            │
//! ├-------------- Chunk entries (repeated) ----------┤
//! │  0.. 4   encrypted_size: u32 little-endian                               │
//! │  4.. N   encrypted_bytes  (ciphertext + 16-byte AES-GCM tag)            │
//! ├-------------- Footer index ----------------┤
//! │          CBOR-encoded Vec<PackIndexEntry>                                │
//! ├-------------- Trailer (8 bytes) -------------┤
//! │  0.. 8   footer_offset: u64 little-endian  (byte offset of footer start) │
//! └-------------------------------------┘
//! ```
//!
//! The footer offset in the trailer allows reading the index without
//! downloading the whole pack.

use bkp_types::{
    error::{Error, Result},
    snapshot::PackId,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const PACK_MAGIC: &[u8; 4] = b"BKPK";
const PACK_VERSION: u16 = 1;
const HEADER_LEN: usize = 22; // 4 (magic) + 2 (version) + 16 (pack_id)

// - PackIndexEntry ------------------------------

/// An entry in the pack index for one encrypted chunk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackIndexEntry {
    /// HMAC-SHA256 chunk-ID (32 raw bytes); see [`bkp_types::chunk::ChunkId`].
    pub chunk_id: [u8; 32],
    /// Byte offset within the pack file where the size-prefixed chunk data starts.
    /// The actual encrypted bytes begin at `offset + 4` (after the u32 size prefix).
    pub offset: u64,
    /// Byte length of the encrypted chunk (not including the 4-byte size prefix).
    pub size: u64,
}

// - Pack reading -------------------------------

/// Read the pack header and footer index from a complete pack blob.
///
/// Returns `(pack_id, index_entries)`.  The chunk data itself can then be
/// accessed via each entry's `offset` and `size`.
pub fn read_pack_index(data: &[u8]) -> Result<(PackId, Vec<PackIndexEntry>)> {
    if data.len() < HEADER_LEN + 8 {
        return Err(Error::Storage(format!(
            "pack too short: {} bytes (minimum {})",
            data.len(),
            HEADER_LEN + 8
        )));
    }

    // Verify magic.
    if &data[0..4] != PACK_MAGIC {
        return Err(Error::Storage(format!(
            "bad pack magic: {:02x?}",
            &data[0..4]
        )));
    }

    // Verify version.
    let version = u16::from_be_bytes([data[4], data[5]]);
    if version != PACK_VERSION {
        return Err(Error::Storage(format!(
            "unsupported pack version {version} (expected {PACK_VERSION})"
        )));
    }

    // Read pack_id.
    let id_bytes: [u8; 16] = data[6..22]
        .try_into()
        .map_err(|_| Error::Storage("pack_id slice error".into()))?;
    let pack_id = PackId::from_uuid(Uuid::from_bytes(id_bytes));

    // Read footer offset from the last 8 bytes.
    let trailer_start = data.len() - 8;
    let footer_offset = u64::from_le_bytes(
        data[trailer_start..]
            .try_into()
            .map_err(|_| Error::Storage("footer offset read error".into()))?,
    ) as usize;

    if footer_offset > trailer_start {
        return Err(Error::Storage(format!(
            "pack footer offset {footer_offset} exceeds data length {trailer_start}"
        )));
    }

    let cbor_data = &data[footer_offset..trailer_start];
    let index: Vec<PackIndexEntry> = ciborium::from_reader(cbor_data)
        .map_err(|e| Error::Serialization(format!("pack index CBOR decode: {e}")))?;

    Ok((pack_id, index))
}

/// Extract one encrypted chunk from a pack blob using an index entry.
///
/// Returns the raw encrypted bytes (without the 4-byte size prefix).
pub fn extract_chunk<'a>(pack_data: &'a [u8], entry: &PackIndexEntry) -> Result<&'a [u8]> {
    let start = entry.offset as usize;
    let end = start + 4 + entry.size as usize;
    if end > pack_data.len() {
        return Err(Error::Storage(format!(
            "chunk entry at offset {start} size {} extends beyond pack ({} bytes)",
            entry.size,
            pack_data.len()
        )));
    }
    // Skip the 4-byte size prefix.
    Ok(&pack_data[start + 4..end])
}

/// Parse the footer offset from the last 8 bytes of a pack file.
///
/// Enables efficient pack index reading via two range requests:
/// 1. `get_range(path, size - 8, size)` → pass result to this function.
/// 2. `get_range(path, footer_offset, size - 8)` → pass result to [`parse_pack_index_cbor`].
pub fn parse_pack_footer_offset(last_8_bytes: &[u8]) -> Result<u64> {
    if last_8_bytes.len() != 8 {
        return Err(Error::Storage(format!(
            "parse_pack_footer_offset: expected 8 bytes, got {}",
            last_8_bytes.len()
        )));
    }
    let arr: [u8; 8] = last_8_bytes
        .try_into()
        .map_err(|_| Error::Storage("footer offset slice error".into()))?;
    Ok(u64::from_le_bytes(arr))
}

/// Decode a CBOR-encoded pack index from the bytes between the footer offset and the
/// 8-byte trailer.  Pair with [`parse_pack_footer_offset`] for range-request based index reads.
pub fn parse_pack_index_cbor(cbor_bytes: &[u8]) -> Result<Vec<PackIndexEntry>> {
    ciborium::from_reader(cbor_bytes)
        .map_err(|e| Error::Serialization(format!("pack index CBOR decode: {e}")))
}
