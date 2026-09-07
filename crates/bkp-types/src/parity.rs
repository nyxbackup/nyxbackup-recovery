// Copyright (c) 2026 Nyx Software, LLC. All rights reserved.
// Nyx Backup - https://nyxbackup.com

//! Reed-Solomon parity geometry for pack objects.
//!
//! Parity adds erasure-coding redundancy to packs written to less-durable
//! self-hosted storage (a NAS reached over SMB/SFTP/AFP), so a partially
//! corrupted or bit-rotted pack is still readable.  Cloud object stores
//! replicate internally, so the main application leaves parity off for them.
//!
//! **This is the read-only half.**  The application's copy of this module also
//! carries `ParitySetting`, `default_parity_for` and `resolve_parity` - the
//! per-endpoint policy deciding *whether and how* to write parity.  The
//! Recovery Tool never writes a `.par` object, so none of that has meaning
//! here and it is deliberately absent rather than ported and left unused.
//!
//! The codec that consumes this geometry lives in `bkp_chunker::parity`.

/// How many data and parity shards a pack is split into.
///
/// A pack is divided into `data_shards` equal data shards (the pack bytes, the
/// last shard zero-padded) plus `parity_shards` Reed-Solomon parity shards.
/// The pack survives the loss or corruption of any `parity_shards` of the
/// `data_shards + parity_shards` total shards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParityGeometry {
    /// Number of data shards (K). At least 1.
    pub data_shards: u16,
    /// Number of parity shards (M). At least 1.
    pub parity_shards: u16,
}

impl ParityGeometry {
    /// Total shard count (`data_shards + parity_shards`).  Reed-Solomon over
    /// GF(2^8) allows at most 256 total shards.
    pub fn total_shards(&self) -> usize {
        self.data_shards as usize + self.parity_shards as usize
    }

    /// Validate the geometry: both counts at least 1 and total at most 256.
    pub fn validate(&self) -> Result<(), String> {
        if self.data_shards < 1 {
            return Err("data_shards must be at least 1".to_string());
        }
        if self.parity_shards < 1 {
            return Err("parity_shards must be at least 1".to_string());
        }
        if self.total_shards() > 256 {
            return Err(format!(
                "data_shards + parity_shards must be at most 256 (got {})",
                self.total_shards()
            ));
        }
        Ok(())
    }
}
