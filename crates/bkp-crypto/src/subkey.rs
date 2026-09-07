// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! HKDF-SHA256 subkey derivation.
//!
//! HKDF-SHA256 is FIPS-approved (NIST SP 800-56C).  Routed through
//! [`crate::backend`] so the active build's backend (pure-Rust by
//! default, aws-lc-rs under the `fips` feature) is used.
//!
//! Each subkey is bound to a purpose label and to the backup-set UUID,
//! so subkeys for different purposes or different backup sets are
//! always distinct even when derived from the same master key.
//!
//! HKDF info field:
//!   `info = label_bytes || 0x00 || backup_set_uuid_bytes`
//!
//! See data format spec Sections 3.2 and 4.

use bkp_types::backup_set::BackupSetId;
use bkp_types::error::Result;

use crate::keys::{KeyLabel, MasterKey, SubKey};

/// Derive a purpose-specific [`SubKey`] from `master` for the given `label`
/// and `backup_set_id`.
///
/// Two calls with identical arguments always produce the same subkey; calls
/// with any differing argument always produce a different subkey.
pub fn derive_subkey(
    master: &MasterKey,
    label: KeyLabel,
    backup_set_id: &BackupSetId,
) -> Result<SubKey> {
    // Construct HKDF info: label_bytes || 0x00 || backup_set_uuid_bytes (16 bytes)
    let label_bytes = label.as_str().as_bytes();
    let mut info = Vec::with_capacity(label_bytes.len() + 1 + 16);
    info.extend_from_slice(label_bytes);
    info.push(0x00);
    info.extend_from_slice(backup_set_id.as_bytes());

    let okm = crate::backend::hkdf_sha256(None, master.as_bytes(), &info, 32)?;
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&okm);
    Ok(SubKey::from_bytes(arr))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::MasterKey;
    use bkp_types::backup_set::BackupSetId;

    fn master() -> MasterKey {
        MasterKey::from_bytes([0x42u8; 32])
    }

    #[test]
    fn derivation_is_deterministic() {
        let set_id = BackupSetId::new();
        let k1 = derive_subkey(&master(), KeyLabel::ChunkEncryption, &set_id).unwrap();
        let k2 = derive_subkey(&master(), KeyLabel::ChunkEncryption, &set_id).unwrap();
        assert_eq!(k1.as_bytes(), k2.as_bytes());
    }

    #[test]
    fn different_labels_produce_different_keys() {
        let set_id = BackupSetId::new();
        let k1 = derive_subkey(&master(), KeyLabel::ChunkEncryption, &set_id).unwrap();
        let k2 = derive_subkey(&master(), KeyLabel::ManifestEncryption, &set_id).unwrap();
        assert_ne!(k1.as_bytes(), k2.as_bytes());
    }

    #[test]
    fn different_set_ids_produce_different_keys() {
        let id1 = BackupSetId::new();
        let id2 = BackupSetId::new();
        let k1 = derive_subkey(&master(), KeyLabel::ChunkEncryption, &id1).unwrap();
        let k2 = derive_subkey(&master(), KeyLabel::ChunkEncryption, &id2).unwrap();
        assert_ne!(k1.as_bytes(), k2.as_bytes());
    }
}
