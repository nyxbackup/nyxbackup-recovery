// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! bkp-crypto - All cryptographic operations.
//!
//! Modules:
//! - `hash`    - SHA-256 / HMAC-SHA256 content hashing
//! - `kdf`     - Argon2id or PBKDF2-HMAC-SHA256 master-key derivation
//! - `subkey`  - HKDF-SHA256 subkey derivation per label and backup-set context
//! - `aead`    - AES-256-GCM encrypt/decrypt (raw, without envelope framing)
//! - `hmac`    - HMAC-SHA256 manifest signing
//! - `envelope`- AES-256-GCM-only envelope decode (v2 header); v1
//!   envelopes are refused
//! - `backend` - pure-Rust (RustCrypto) crypto primitive dispatch
//! - `keys`    - Key types with zeroize-on-drop
//!
//! # FIPS positioning
//!
//! Single codebase, single configuration per platform.  The active crypto
//! primitives live in `backend::*` and are selected at compile time by
//! `cfg(target_os = ...)` to match each platform's native FIPS-validated
//! module.  See `docs/FIPS.md` for the per-OS module citations.
//!
//! | Primitive            | Source                                              |
//! |----------------------|-----------------------------------------------------|
//! | SHA-256 / HMAC-SHA256 (`hash`) | Vendor module via `backend::sha256_*`     |
//! | Argon2id (`kdf`)     | RustCrypto (carved-out non-validated KDF)           |
//! | PBKDF2-HMAC-SHA256 (`kdf`) | Vendor module via `backend::pbkdf2_*`         |
//! | HKDF-SHA256 (`subkey`) | Vendor HMAC primitive per SP 800-56C              |
//! | HMAC-SHA256 (`hmac`) | Vendor module                                       |
//! | AES-256-GCM (`aead`, `envelope`) | Vendor module                           |
//!
//! Rules to keep this surface intact:
//!
//! 1. **No other crate in this workspace may import `aes_gcm`, `argon2`,
//!    `hkdf`, `hmac`, or `sha2` directly.**  Always go through this crate.
//!    Enforced by code review.
//! 2. **Do not hard-code `32` as a digest width** outside this crate.  Use
//!    `hash::HASH_LEN` or a generic parameter so any future digest-width
//!    change lands in one place.
//! 3. **Workspace stays on `aws-lc-sys` (rustls's aws-lc-rs provider), not
//!    `ring`**, because `aws-lc-sys` and `aws-lc-fips-sys` are the same C
//!    library at different feature levels.
//! 4. **The on-disk format-version field is 2** (post-content-hash swap);
//!    v1 envelopes and v1 manifests are refused at decode.

// unsafe_code is forbidden in all modules except `keys`, which needs it for
// mlock/VirtualLock to prevent master key pages from being swapped out.
#![deny(unsafe_code)]
#![deny(missing_docs)]
#![deny(clippy::unwrap_used)]

pub mod aead;
pub mod backend;
pub mod envelope;
pub mod hash;
pub mod hmac;
pub mod kdf;
pub mod keys;
pub mod subkey;
pub mod tls;
