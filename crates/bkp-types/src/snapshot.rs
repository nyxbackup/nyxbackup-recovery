// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! Snapshot and pack identity types, plus the snapshot index entry.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Unique identifier for a pack file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PackId(pub Uuid);

impl PackId {
    /// Generate a new random pack ID.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Construct from an existing UUID.
    pub fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    /// Return the inner UUID.
    pub fn as_uuid(&self) -> &Uuid {
        &self.0
    }

    /// Return the UUID as raw bytes (16 bytes, used in pack header and CBOR).
    pub fn as_bytes(&self) -> &[u8; 16] {
        self.0.as_bytes()
    }
}

impl Default for PackId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for PackId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Unique identifier for a backup snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SnapshotId(pub Uuid);

impl SnapshotId {
    /// Generate a new random snapshot ID.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Construct from an existing UUID.
    pub fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    /// Return the inner UUID.
    pub fn as_uuid(&self) -> &Uuid {
        &self.0
    }

    /// Return the UUID as raw bytes (16 bytes, used in manifests and CBOR).
    pub fn as_bytes(&self) -> &[u8; 16] {
        self.0.as_bytes()
    }
}

impl Default for SnapshotId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for SnapshotId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::str::FromStr for SnapshotId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(Uuid::parse_str(s)?))
    }
}

/// An entry in the remote snapshot index for one completed backup snapshot.
///
/// Corresponds to `SnapshotEntry` in data format spec Section 10.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotEntry {
    /// Unique identifier for this snapshot.
    pub snapshot_id: SnapshotId,
    /// Creation timestamp as Unix nanoseconds.
    pub created_at: u64,
    /// Remote path of the encrypted manifest, relative to the endpoint prefix.
    pub manifest_path: String,
    /// Byte size of the encrypted manifest object.
    pub manifest_size: u64,
    /// HMAC-SHA256 over `snapshot_id_bytes || manifest_ciphertext`.
    /// Stored in the index so integrity can be verified before downloading the manifest.
    pub manifest_hmac: [u8; 32],
    /// Total number of files recorded in this snapshot.
    pub files_total: u64,
    /// Total plaintext bytes across all files in this snapshot.
    pub bytes_total: u64,
    /// UUIDs of all pack files referenced by chunks in this snapshot.
    /// Used by GC to determine which packs are safe to delete.
    pub packs_referenced: Vec<PackId>,
}
