// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! bkp-chunker - pack reading and zstd decompression (recovery path).
//!
//! The recovery tool reads packs and decompresses chunks; it does not chunk
//! files or build packs.  See the `pack` module for the pack-format reader
//! and `decompress` for zstd decompression.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(clippy::unwrap_used)]

pub mod pack;

use bkp_types::error::{Error, Result};

// - Decompression -----------------------------

/// Decompress a zstd-compressed chunk.
///
/// A 64 MiB output cap guards against decompression bombs; a single chunk
/// should never exceed the configured max chunk size (default 16 MiB).
pub fn decompress(data: &[u8]) -> Result<Vec<u8>> {
    const MAX_DECOMP: usize = 64 * 1024 * 1024;
    zstd::bulk::decompress(data, MAX_DECOMP)
        .map_err(|e| Error::Storage(format!("zstd decompress: {e}")))
}
