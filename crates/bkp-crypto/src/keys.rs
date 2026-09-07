// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! Cryptographic key types with zeroize-on-drop and memory locking.
//!
//! `MasterKey` and `SubKey` both hold 32 bytes of key material.  They clear
//! that memory when dropped (preventing key material from leaking into core
//! dumps or persistent swap space).
//!
//! # Memory locking
//!
//! `MasterKey` additionally locks its backing page(s) against swap using the
//! OS primitive:
//!
//! | Platform     | Primitive            | Effect                                       |
//! |-------------|----------------------|----------------------------------------------|
//! | Linux/macOS  | `mlock(2)`           | Pages pinned in RAM; excluded from core dumps on some kernels |
//! | Windows      | `VirtualLock`        | Pages pinned in physical memory; not written to pagefile |
//!
//! `mlock` / `VirtualLock` are best-effort: if the call fails (e.g. `mlock`
//! limit exhausted or insufficient privilege), a warning is logged and the key
//! remains in RAM but may be swapped.  The backup run is not aborted.
//!
//! # Heap allocation
//!
//! The key bytes are stored in a `Box<[u8; 32]>` to guarantee a stable heap
//! address.  Stack variables may be moved by the compiler; a heap allocation
//! always occupies the same physical address for its lifetime, which is
//! required for `mlock` to protect the right pages.
//!
//! # SubKey
//!
//! `SubKey` is not mlock'd - it is derived on demand per backup run and
//! dropped quickly.  High-churn allocations from the global allocator are
//! difficult to mlock reliably, and the master key (from which all subkeys
//! flow) is already protected.

#![allow(unsafe_code)]

use zeroize::{Zeroize, ZeroizeOnDrop};

// - MasterKey ---------------------------------

/// The master key derived from the user passphrase via Argon2id.
///
/// One master key exists per machine.  Per-backup-set subkeys are derived from
/// it via HKDF.  The key bytes are heap-allocated and mlock'd so they cannot
/// be swapped to disk while the daemon is running.
pub struct MasterKey {
    /// Heap-allocated key bytes - stable address required for mlock.
    bytes: Box<[u8; 32]>,
    /// Whether `mlock` / `VirtualLock` succeeded.  Tracked so we call the
    /// matching unlock primitive in `Drop`.
    mlocked: bool,
}

impl MasterKey {
    /// Wrap raw key bytes.  The caller must ensure the bytes come from a
    /// secure derivation (Argon2id or OS keyring).
    ///
    /// The key material is copied into a heap allocation, and the heap page(s)
    /// are locked against swap.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        let boxed = Box::new(bytes);
        let mlocked = platform::mlock(boxed.as_ptr(), 32);
        Self {
            bytes: boxed,
            mlocked,
        }
    }

    /// Generate a fresh master key from the OS random source.
    pub fn generate() -> Self {
        use rand::RngCore;
        let mut bytes = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        Self::from_bytes(bytes)
    }

    /// Encode the key as a human-readable recovery passphrase: 8 groups of
    /// 8 lowercase hex digits joined by hyphens (32 bytes = 64 hex chars).
    pub fn to_recovery_passphrase(&self) -> String {
        let bytes = self.bytes.as_ref();
        (0..8)
            .map(|i| {
                let chunk = &bytes[i * 4..(i + 1) * 4];
                format!(
                    "{:02x}{:02x}{:02x}{:02x}",
                    chunk[0], chunk[1], chunk[2], chunk[3]
                )
            })
            .collect::<Vec<_>>()
            .join("-")
    }

    /// Borrow the raw key bytes for cryptographic operations.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.bytes
    }
}

impl Drop for MasterKey {
    fn drop(&mut self) {
        // Zeroize first - most security-critical step.
        self.bytes.zeroize();
        // Release the memory lock so the OS can reclaim page-lock quota.
        if self.mlocked {
            platform::munlock(self.bytes.as_ptr(), 32);
        }
    }
}

impl std::fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MasterKey([REDACTED])")
    }
}

// - SubKey ----------------------------------

/// HKDF-derived subkey scoped to a specific purpose within one backup set.
///
/// Each subkey is bound to a label string and a backup-set UUID, so subkeys
/// for different purposes or different sets are always independent.
/// See data format spec Sections 3.2 and 4.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct SubKey([u8; 32]);

impl SubKey {
    /// Wrap raw key bytes.  The caller must ensure the bytes come from HKDF.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Borrow the raw key bytes for cryptographic operations.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl std::fmt::Debug for SubKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SubKey([REDACTED])")
    }
}

// - KeyLabel ---------------------------------

/// Symbolic label identifying the purpose of an HKDF-derived subkey.
///
/// String values and numeric IDs must match data format spec Section 3.2 and
/// the `key_label_id` table in Section 12.3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyLabel {
    /// AES-256-GCM key for encrypting chunk data (envelope ID 0).
    ChunkEncryption,
    /// AES-256-GCM key for encrypting manifests (envelope ID 1).
    ManifestEncryption,
    /// HMAC-SHA256 key for signing manifests (envelope ID 2).
    ManifestHmac,
    /// AES-256-GCM key for encrypting pack index blocks (envelope ID 3).
    PackIndexEncryption,
    /// AES-256-GCM key for encrypting snapshot index objects (envelope ID 4).
    SnapshotIndex,
    /// SHA-256 keyed-hash key for chunk identity (envelope ID 5).
    ///
    /// Per-backup-set keying makes chunk_ids opaque to anyone who lacks the
    /// master key - closes the "is file X in this backup?" oracle attack
    /// against leaked manifests.  Within one backup set the chunk_id is still
    /// deterministic, so dedup continues to work.
    ChunkIdentity,
}

impl KeyLabel {
    /// The ASCII label string used as HKDF `info` prefix.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ChunkEncryption => "chunk-encryption-v1",
            Self::ManifestEncryption => "manifest-encryption-v1",
            Self::ManifestHmac => "manifest-hmac-v1",
            Self::PackIndexEncryption => "pack-index-encryption-v1",
            Self::SnapshotIndex => "snapshot-index-v1",
            Self::ChunkIdentity => "chunk-identity-v1",
        }
    }

    /// The `key_label_id` byte stored in the encryption envelope header.
    pub fn id(self) -> u8 {
        match self {
            Self::ChunkEncryption => 0,
            Self::ManifestEncryption => 1,
            Self::ManifestHmac => 2,
            Self::PackIndexEncryption => 3,
            Self::SnapshotIndex => 4,
            Self::ChunkIdentity => 5,
        }
    }

    /// Reconstruct a `KeyLabel` from the envelope header `key_label_id` byte.
    pub fn from_id(id: u8) -> Option<Self> {
        match id {
            0 => Some(Self::ChunkEncryption),
            1 => Some(Self::ManifestEncryption),
            2 => Some(Self::ManifestHmac),
            3 => Some(Self::PackIndexEncryption),
            4 => Some(Self::SnapshotIndex),
            5 => Some(Self::ChunkIdentity),
            _ => None,
        }
    }
}

// - Platform mlock/munlock --------------------------

mod platform {
    use tracing::warn;

    // - Linux / macOS -----------------------------

    #[cfg(unix)]
    pub(super) fn mlock(ptr: *const u8, len: usize) -> bool {
        // SAFETY: `ptr` is valid for `len` bytes - it comes from `Box<[u8; 32]>`.
        // `mlock` does not read or write the memory; it only instructs the kernel
        // to pin the pages in RAM.
        let ret = unsafe { libc::mlock(ptr.cast::<libc::c_void>(), len) };
        if ret != 0 {
            warn!(
                "mlock failed - master key pages may be swappable: {}",
                std::io::Error::last_os_error()
            );
            false
        } else {
            true
        }
    }

    #[cfg(unix)]
    pub(super) fn munlock(ptr: *const u8, len: usize) {
        // SAFETY: `ptr` and `len` match the region that was passed to mlock.
        unsafe { libc::munlock(ptr.cast::<libc::c_void>(), len) };
    }

    // - Windows --------------------------------

    // Rust 2024 requires extern blocks to be `unsafe extern` (the items
    // inside are themselves unsafe to call; the unsafe keyword on the block
    // makes that visible at the declaration site).
    #[cfg(target_os = "windows")]
    unsafe extern "system" {
        fn VirtualLock(lpAddress: *const u8, dwSize: usize) -> i32;
        fn VirtualUnlock(lpAddress: *const u8, dwSize: usize) -> i32;
    }

    #[cfg(target_os = "windows")]
    pub(super) fn mlock(ptr: *const u8, len: usize) -> bool {
        // SAFETY: `ptr` is valid for `len` bytes from a `Box<[u8; 32]>`.
        let ret = unsafe { VirtualLock(ptr, len) };
        if ret == 0 {
            warn!(
                "VirtualLock failed - master key pages may be swappable: {}",
                std::io::Error::last_os_error()
            );
            false
        } else {
            true
        }
    }

    #[cfg(target_os = "windows")]
    pub(super) fn munlock(ptr: *const u8, len: usize) {
        // SAFETY: matches the VirtualLock call in `mlock`.
        unsafe { VirtualUnlock(ptr, len) };
    }

    // - Unsupported targets (WASM, etc.) -------------------

    #[cfg(not(any(unix, target_os = "windows")))]
    pub(super) fn mlock(_ptr: *const u8, _len: usize) -> bool {
        false
    }

    #[cfg(not(any(unix, target_os = "windows")))]
    pub(super) fn munlock(_ptr: *const u8, _len: usize) {}
}

// - Tests -----------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn master_key_round_trips() {
        let raw = [0x42u8; 32];
        let key = MasterKey::from_bytes(raw);
        assert_eq!(key.as_bytes(), &raw);
    }

    #[test]
    fn master_key_debug_redacted() {
        let key = MasterKey::from_bytes([0u8; 32]);
        assert_eq!(format!("{key:?}"), "MasterKey([REDACTED])");
    }

    #[test]
    fn sub_key_round_trips() {
        let raw = [0xABu8; 32];
        let key = SubKey::from_bytes(raw);
        assert_eq!(key.as_bytes(), &raw);
    }
}
