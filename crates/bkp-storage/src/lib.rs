// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! bkp-storage - StorageBackend trait and all endpoint implementations.
//!
//! Modules:
//! - `backend`            - StorageBackend trait definition
//! - `backends::s3`       - AWS S3 (Standard / IA / Glacier IR / Glacier / Deep Archive); auto-tunes for Cloudflare R2
//! - `backends::s3_compat`- S3-compatible thin forwarder (Wasabi, MinIO, Storj, B2 S3 API)
//! - `backends::azure`    - Azure Blob (Hot / Cool / Cold / Archive); in-place re-tier via raw REST
//! - `backends::b2`       - Backblaze B2 native API
//! - `backends::gcs`      - Google Cloud Storage
//! - `backends::dropbox`  - Dropbox HTTP API (OAuth via bkp-oauth)
//! - `backends::googledrive` - Google Drive v3 API (OAuth via bkp-oauth)
//! - `backends::onedrive` - Microsoft Graph (BYO OAuth creds per set)
//! - `backends::sftp`     - SFTP via libssh2
//! - `backends::smb`      - SMB network share (OS-mounted path)
//! - `backends::webdav`   - Generic WebDAV (Nextcloud, Synology, Apache mod_dav)
//! - `backends::local`    - Local folder / external disk with atomic-rename writes
//! - `backends::oauth`    - Shared OAuth refresh-token helpers
//! - `registry`           - Builds a backend from an endpoint config entry
//! - `retry`              - Transient-error classifier + exponential-backoff wrapper (always applied)
//! - `rate_limited`       - Token-bucket bandwidth throttle (applied when up/down KBPS configured)
//! - `nice_net`           - Low-priority socket options (TOS scavenger + TCP_CONGESTION = lp)

// Most of the crate is safe Rust; the `nice_net` module needs a handful of
// `unsafe` calls to invoke libc::setsockopt directly (no safe wrapper in our
// dep graph).  Scope the relaxation to that module.
#![deny(unsafe_code)]
#![deny(missing_docs)]
#![deny(clippy::unwrap_used)]

pub mod backend;
pub mod backends;
#[allow(unsafe_code)]
pub mod nice_net;
pub mod rate_limited;
pub mod registry;
pub mod retry;
