// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! Chunk identity type.

use serde::{Deserialize, Serialize};

/// Content-addressing key for one chunk: 32 raw bytes from a SHA-256-family
/// digest of the plaintext.  Engine writes use HMAC-SHA256 under a per-set
/// HKDF subkey; unkeyed SHA-256 is reserved for cross-snapshot
/// probes where a per-set key would defeat the lookup.  Type is agnostic
/// to which form produced the bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChunkId(pub [u8; 32]);

impl ChunkId {
    /// Construct from a raw 32-byte hash.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Return the raw 32-byte hash.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Return the hash as a 64-character lowercase hex string.
    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }
}

impl std::fmt::Display for ChunkId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}
