// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! Machine identity and Argon2id parameter types.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::backup_set::BackupSetId;

/// Unique identifier for a machine, generated at install time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MachineId(pub Uuid);

impl MachineId {
    /// Generate a new random machine ID.
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

    /// Return the UUID as raw bytes (16 bytes, used in CBOR encoding).
    pub fn as_bytes(&self) -> &[u8; 16] {
        self.0.as_bytes()
    }
}

impl Default for MachineId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for MachineId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// KDF algorithm choice for master-key derivation.
///
/// Locked at install time via the machine record; switching after the first
/// successful backup would invalidate every backup encrypted under the old
/// master key.  Default is `Argon2id`; `Pbkdf2HmacSha256` is opted into at
/// install via `NYX_KDF_MODE=pbkdf2`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum KdfAlgorithm {
    /// Argon2id.  Stronger GPU brute-force resistance, but not currently
    /// on the FIPS-approved list (SP 800-132 Rev 2 candidate).  Routed
    /// through the RustCrypto `argon2` crate (documented carve-out -
    /// no vendor module exposes Argon2id).
    #[default]
    Argon2id,
    /// PBKDF2-HMAC-SHA256.  FIPS-approved (SP 800-132).  Routed through
    /// the platform's vendor-validated module: BCryptDeriveKeyPBKDF2 on
    /// Windows, CCKeyDerivationPBKDF on macOS, aws-lc-rs::pbkdf2 on Linux.
    Pbkdf2HmacSha256,
}

/// Master-key KDF parameters recorded with the machine.  Despite the legacy
/// name, this carries parameters for either KDF family (`algorithm` selects);
/// fields not relevant to the chosen algorithm are ignored.  Persisted in the
/// machine record so cross-machine recovery can re-derive the identical key.
/// See data format spec Sections 3.1 and 11.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Argon2Params {
    /// KDF algorithm choice.  Locked at install; switching invalidates
    /// the master key and every backup encrypted under it.
    #[serde(default)]
    pub algorithm: KdfAlgorithm,
    /// Memory cost in KiB (default 131072 = 128 MiB).  Ignored when
    /// `algorithm == Pbkdf2HmacSha256`.
    pub m_cost: u32,
    /// Number of iterations (Argon2: default 3; PBKDF2: 1,000,000+).
    pub t_cost: u32,
    /// Degree of parallelism (default 4).  Ignored when
    /// `algorithm == Pbkdf2HmacSha256`.
    pub p_cost: u32,
    /// Output length in bytes (default 32 = 256-bit master key).
    pub output_len: usize,
    /// Random 32-byte salt, generated once at install time and stored here.
    pub salt: [u8; 32],
}

/// Cross-machine recovery record stored as **plaintext** CBOR at a well-known
/// remote path (`machines/<machine_id>/bootstrap`).
///
/// Written by the daemon on first startup and updated whenever backup sets
/// are added or removed.  Because it is unencrypted, it must not contain
/// secrets - the Argon2id params (including salt) are intentionally non-secret;
/// only the passphrase itself must be kept private.
///
/// A new machine downloads this record, prompts the user for their passphrase,
/// and re-derives the master key from the stored Argon2 params.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootstrapRecord {
    /// Format version - currently 1.
    pub format_version: u32,
    /// Stable machine identifier generated at install time.
    pub machine_id: MachineId,
    /// Hostname at the time of last update (informational).
    pub hostname: String,
    /// Record creation time as Unix seconds.
    pub created_at: u64,
    /// Argon2id parameters used to derive the master key.
    ///
    /// The recovering machine must use the exact same parameters and salt to
    /// re-derive the identical master key from the user's passphrase.
    pub kdf_params: Argon2Params,
    /// IDs of all backup sets configured on this machine.
    ///
    /// Each ID is a UUID used to locate snapshot indexes and manifests
    /// in the remote storage.
    pub backup_set_ids: Vec<BackupSetId>,
}

impl Default for Argon2Params {
    /// Returns the spec-default parameters with an all-zero salt placeholder.
    /// The caller **must** replace `salt` with cryptographically random bytes
    /// before use.
    fn default() -> Self {
        Self {
            algorithm: KdfAlgorithm::default(),
            m_cost: 131072,
            t_cost: 3,
            p_cost: 4,
            output_len: 32,
            salt: [0u8; 32],
        }
    }
}
