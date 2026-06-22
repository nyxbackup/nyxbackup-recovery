// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! bkp-manifest - CBOR serialization and encrypted-envelope encoding for
//! manifests, snapshot indexes, and machine records.
//!
//! Every remote object produced by this crate passes through the encryption
//! envelope defined in `bkp-crypto::envelope` (AES-256-GCM with a 32-byte
//! AAD header).  CBOR (ciborium) is used for all binary payloads.
//!
//! # Remote object types
//!
//! | Type              | Key label               | Mutable? |
//! |-------------------|-------------------------|----------|
//! | `Manifest`        | `ManifestEncryption`    | No       |
//! | `SnapshotIndex`   | `SnapshotIndex`         | Yes      |
//! | `MachineRecord`   | `ManifestEncryption`    | Yes      |
//!
//! The `SnapshotIndex` is the *only* mutable remote object.  Writes must use
//! `StorageBackend::put_if_absent` for the first write and a lock on the local
//! side for subsequent updates.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(clippy::unwrap_used)]

use serde::{Deserialize, Serialize};

use bkp_crypto::{envelope, keys::SubKey};
use bkp_types::{
    backup_set::BackupSetId,
    error::{Error, Result},
    machine::{BootstrapRecord, MachineId},
    manifest::FileTree,
    snapshot::{SnapshotEntry, SnapshotId},
};

/// On-disk format version this build produces and accepts.
///
/// Bumped when the on-disk envelope, the manifest CBOR schema, the
/// chunk-ID width, or any signed-input layout changes in a way that
/// makes the new artifacts incompatible with the old code path.
///
/// History:
/// - **1** (initial release): pre-FIPS content-hash primitive for chunk IDs
///   and manifest authenticator.  No longer accepted.
/// - **2**: HMAC-SHA256 chunk IDs, SHA-256-family integrity.
///   Content addresses for the same plaintext bytes differ between v1 and
///   v2; a v2 build cannot read v1 manifests and vice versa.  Pre-release
///   migration was "reset every backup set"; a future cross-edition cut
///   would need a documented multi-month migration plan.
///
/// `decode_manifest` and `decode_snapshot_index` refuse any value other
/// than this constant, so a mixed-edition deployment fails fast with a
/// clear error rather than silently mis-decoding cross-edition data.
pub const CURRENT_FORMAT_VERSION: u32 = 2;

// - Manifest ---------------------------------

/// A complete backup snapshot manifest.
///
/// Serialised with CBOR and encrypted with `ManifestEncryption` subkey.
/// The HMAC over the ciphertext is stored in the corresponding `SnapshotEntry`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    /// On-disk format version; see [`CURRENT_FORMAT_VERSION`].
    pub format_version: u32,
    /// Snapshot this manifest belongs to.
    pub snapshot_id: SnapshotId,
    /// Backup set this snapshot is part of.
    pub backup_set_id: BackupSetId,
    /// Machine that produced this snapshot.
    pub machine_id: MachineId,
    /// Snapshot creation time as Unix nanoseconds.
    pub created_at_ns: u64,
    /// Hostname of the source machine (informational).
    pub hostname: String,
    /// User-visible backup-set name from the source machine's config
    /// (e.g. "Documents", "Photos").  `serde(default)` so older
    /// older manifests decode cleanly as `""`.
    /// The Recovery Tool surfaces this so users see their own set
    /// names instead of opaque "Set N" labels.
    #[serde(default)]
    pub set_name: String,
    /// Total number of files in this snapshot.
    pub files_total: u64,
    /// Total number of directories in this snapshot.
    pub dirs_total: u64,
    /// Total plaintext bytes across all files.
    pub bytes_total: u64,
    /// Total number of chunk references in this snapshot.
    pub chunks_total: u64,
    /// The file tree captured by this snapshot.
    pub file_tree: FileTree,
}

/// Decrypt and deserialise a manifest envelope.
///
/// Refuses any `format_version` other than [`CURRENT_FORMAT_VERSION`]
///.  Any future cross-edition cut (e.g. SHA-256 -> SHA-384
/// digest width) bumps the constant; the explicit check here means a
/// mismatched-edition build fails fast with a clear error instead of
/// silently mis-decoding the chunk-ID width.
pub fn decode_manifest(data: &[u8], key: &SubKey) -> Result<Manifest> {
    let plaintext = envelope::decode(key, data)?;
    let manifest: Manifest = cbor_decode(&plaintext)?;
    if manifest.format_version != CURRENT_FORMAT_VERSION {
        return Err(bkp_types::error::Error::Serialization(format!(
            "manifest format_version {} not supported by this build (expected {}); \
             this may indicate the file was written by a different Nyx Backup edition",
            manifest.format_version, CURRENT_FORMAT_VERSION
        )));
    }
    Ok(manifest)
}

// - Snapshot index ------------------------------

/// The snapshot index for one backup set.
///
/// Stored as a single remote object that accumulates one `SnapshotEntry` per
/// completed backup run.  Encrypted with the `SnapshotIndex` subkey.
///
/// This is the **only** mutable remote object in the format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotIndex {
    /// On-disk format version; see [`CURRENT_FORMAT_VERSION`].
    pub format_version: u32,
    /// Backup set this index belongs to.
    pub backup_set_id: BackupSetId,
    /// Machine that owns this index.
    pub machine_id: MachineId,
    /// All completed snapshots for this backup set, oldest first.
    pub entries: Vec<SnapshotEntry>,
}

impl SnapshotIndex {
    /// Return the most recent snapshot entry, if any.
    pub fn latest(&self) -> Option<&SnapshotEntry> {
        self.entries.last()
    }
}

/// Decrypt and deserialise a snapshot index envelope.
///
/// Refuses any `format_version` other than [`CURRENT_FORMAT_VERSION`]
/// - see `decode_manifest` doc for full rationale.
pub fn decode_snapshot_index(data: &[u8], key: &SubKey) -> Result<SnapshotIndex> {
    let plaintext = envelope::decode(key, data)?;
    let index: SnapshotIndex = cbor_decode(&plaintext)?;
    if index.format_version != CURRENT_FORMAT_VERSION {
        return Err(bkp_types::error::Error::Serialization(format!(
            "snapshot-index format_version {} not supported by this build (expected {})",
            index.format_version, CURRENT_FORMAT_VERSION
        )));
    }
    Ok(index)
}

// - Machine record ------------------------------

/// A per-machine metadata record stored at a well-known remote path.
///
/// Updated each time the daemon starts or a backup set is added/removed.
/// Encrypted with the `ManifestEncryption` subkey.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MachineRecord {
    /// On-disk format version; see [`CURRENT_FORMAT_VERSION`].
    pub format_version: u32,
    /// Stable machine identifier.
    pub machine_id: MachineId,
    /// Hostname at the time of last update.
    pub hostname: String,
    /// OS name, e.g. `"Linux"`, `"macOS"`, `"Windows"`.
    pub os_name: String,
    /// OS version string.
    pub os_version: String,
    /// Application version string.
    pub app_version: String,
    /// First-seen timestamp as Unix nanoseconds.
    pub created_at_ns: u64,
    /// Most recent heartbeat timestamp as Unix nanoseconds.
    pub last_seen_at_ns: u64,
    /// IDs of all backup sets configured on this machine.
    pub backup_set_ids: Vec<BackupSetId>,
}

/// Decrypt and deserialise a machine record envelope.
pub fn decode_machine_record(data: &[u8], key: &SubKey) -> Result<MachineRecord> {
    let plaintext = envelope::decode(key, data)?;
    cbor_decode(&plaintext)
}

// - Manifest HMAC -------------------------------

// - Remote path helpers ----------------------------

/// Remote path for a snapshot manifest object.
///
/// Pattern: `manifests/<backup_set_id>/<snapshot_id>.manifest`
pub fn manifest_remote_path(backup_set_id: &BackupSetId, snapshot_id: &SnapshotId) -> String {
    format!(
        "manifests/{}/{}.manifest",
        backup_set_id.as_uuid(),
        snapshot_id.as_uuid()
    )
}

/// Remote path for the snapshot index object for a backup set.
///
/// Pattern: `indexes/<backup_set_id>/snapshot-index`
pub fn snapshot_index_remote_path(backup_set_id: &BackupSetId) -> String {
    format!("indexes/{}/snapshot-index", backup_set_id.as_uuid())
}

/// Remote path for the machine record object.
///
/// Pattern: `machines/<machine_id>/machine-record`
pub fn machine_record_remote_path(machine_id: &MachineId) -> String {
    format!("machines/{}/machine-record", machine_id.as_uuid())
}

/// Remote path for the plaintext bootstrap record used for cross-machine recovery.
///
/// Pattern: `machines/<machine_id>/bootstrap`
pub fn bootstrap_record_remote_path(machine_id: &MachineId) -> String {
    format!("machines/{}/bootstrap", machine_id.as_uuid())
}

// - Bootstrap record (plaintext CBOR - no encryption) ------------

/// Deserialise a plaintext CBOR [`BootstrapRecord`].
pub fn decode_bootstrap_record(data: &[u8]) -> Result<BootstrapRecord> {
    cbor_decode(data)
}

// - CBOR helpers -------------------------------

fn cbor_decode<T: for<'de> Deserialize<'de>>(data: &[u8]) -> Result<T> {
    ciborium::from_reader(data).map_err(|e| Error::Serialization(format!("CBOR decode: {e}")))
}

// - Tests -----------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // Decode-path correctness (decode_manifest / decode_snapshot_index /
    // decode_machine_record) is intended to be covered by the planned
    // format-conformance fixture described in docs/DESIGN.md "Format parity"
    // (a TODO), not by encode->decode round-trips - this crate is decode-only
    // and has no encode path to round-trip against.

    #[test]
    fn remote_paths_are_stable() {
        let bsid = BackupSetId::new();
        let sid = SnapshotId::new();
        let mid = MachineId::new();
        // Just check the format - callers depend on the path pattern.
        let mp = manifest_remote_path(&bsid, &sid);
        assert!(mp.starts_with("manifests/"));
        assert!(mp.ends_with(".manifest"));
        let ip = snapshot_index_remote_path(&bsid);
        assert!(ip.starts_with("indexes/"));
        assert!(ip.ends_with("snapshot-index"));
        let rp = machine_record_remote_path(&mid);
        assert!(rp.starts_with("machines/"));
    }
}
