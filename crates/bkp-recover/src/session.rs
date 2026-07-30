// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! Per-session state held in memory while the Recovery Tool is running.
//! Discarded on exit; nothing persisted here.  The checkpoint module is the
//! only place we touch on-disk state for an active restore.
//!
//! Wrapped in an `Arc<RwLock<...>>` and managed by Tauri as application
//! state for the GUI; the CLI builds one of these by hand and walks the
//! state machine sequentially.

use bkp_crypto::keys::MasterKey;
use bkp_storage::backend::StorageBackend;
use std::sync::Arc;
use tokio::sync::{RwLock, watch};

/// Coarse-grained state machine for the Recovery Tool workflow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, Default)]
pub enum Phase {
    /// User has not yet supplied endpoint credentials.
    #[default]
    Connect,
    /// Endpoint reachable; awaiting master key unlock.
    Unlock,
    /// Master key in memory; browsing snapshots.
    Browse,
    /// Files selected; restore actively running.
    Restoring,
    /// Restore completed (success or failure).
    Done,
}

/// In-memory session state.  Holds everything the GUI commands and the CLI
/// flow need to perform a restore without re-asking the user.
#[derive(Default)]
pub struct Session {
    pub phase: Phase,
    /// Currently-connected endpoint config.  Cleared on `rec_disconnect`.
    pub endpoint: Option<EndpointParams>,
    /// Built storage backend - reused across `list_snapshots`,
    /// `list_snapshot_files`, and the restore engine.  None until Connect
    /// succeeds; cleared on Disconnect.
    pub backend: Option<Arc<dyn StorageBackend>>,
    /// Master key, derived or pasted.  Cleared on session end / explicit
    /// disconnect.  `Zeroizing` semantics ride on `MasterKey`'s `Drop`.
    pub master_key: Option<MasterKey>,
    /// Last-fetched snapshot list (cached so the picker re-renders fast).
    pub snapshots: Vec<SnapshotSummary>,
    /// Live restore progress.  Populated by the spawned restore task while
    /// running; polled by the GUI every 500 ms.
    pub progress: Option<RestoreProgressView>,
    /// Pause / cancel watch senders, forwarded to `RestoreEngine` via
    /// `set_cancel_pause` so the GUI Pause / Resume / Cancel buttons
    /// take effect at the next file boundary.  Replaced on every
    /// `rec_start_restore`; cleared when the task finishes.
    pub cancel_tx: Option<watch::Sender<bool>>,
    pub pause_tx: Option<watch::Sender<bool>>,
    /// Mirror of the pause flag for the GUI poll (we don't expose the
    /// watch::Sender across the IPC boundary).
    pub paused: bool,
}

/// Snapshot of restore progress.  Replaces a streaming channel with a poll-
/// from-memory model so the GUI side stays simple (one Tauri command, no
/// event listeners).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct RestoreProgressView {
    /// "running", "complete", "error".
    pub status: String,
    pub files_done: u64,
    pub files_total: u64,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub current_file: String,
    /// Set when status == "error".
    pub error_detail: String,
    /// `true` while the user-pressed Pause is in effect.  The engine
    /// stops emitting per-file progress while paused so the running
    /// counter holds steady in the GUI.
    #[serde(default)]
    pub paused: bool,
}

/// Connection params used to instantiate a `StorageBackend`.  Per-backend
/// fields are stored as `Option<String>` so the same struct serves every
/// backend (s3 wants region; s3_compatible wants endpoint_url; azure_blob
/// wants account + container; sftp wants host/port/username; etc.).
/// `endpoint_to_toml` in `commands.rs` picks the right subset for each
/// `endpoint_type`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EndpointParams {
    pub endpoint_type: String,
    /// Bucket name / container name / folder ID / SFTP base path / local
    /// root, depending on backend type.
    pub url: String,
    pub key_id: String,
    #[serde(skip_serializing)]
    pub secret: String,
    /// Optional region (S3, S3-compatible).
    #[serde(default)]
    pub region: Option<String>,
    /// Optional endpoint URL (S3-compatible only).
    #[serde(default)]
    pub endpoint_url: Option<String>,
    /// Optional S3 region override for S3-compatible endpoints.  For s3_compat
    /// the `region` slot carries the endpoint URL (main-app convention), so a
    /// provider that needs a specific bucket region (some MinIO / Ceph setups)
    /// supplies it here; empty falls back to the `us-east-1` default that most
    /// S3-compatible providers accept.
    #[serde(default)]
    pub s3_region: Option<String>,
    /// Optional SFTP host.
    #[serde(default)]
    pub host: Option<String>,
    /// Optional SFTP port (default 22 when None).
    #[serde(default)]
    pub port: Option<u16>,
    /// Optional SFTP username.
    #[serde(default)]
    pub username: Option<String>,
    /// Optional local path to a PEM file: an SSH private key (SFTP key auth)
    /// or a TLS client certificate (WebDAV mutual-TLS).  One field serves both,
    /// mirroring the main app's `storage_private_key_path`.  For SFTP key auth
    /// the `secret` field carries the key *passphrase*, not a login password.
    #[serde(default)]
    pub private_key_path: Option<String>,
}

/// Minimal snapshot info surfaced to the GUI/CLI picker.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SnapshotSummary {
    pub snapshot_id: String,
    pub set_id: String,
    pub created_at: u64,
    pub files_total: u64,
    pub bytes_total: u64,
    /// User-visible backup-set name, read from the latest manifest of
    /// this set (Manifest.set_name).  Empty string for sets
    /// whose snapshots were all written by older daemons - the GUI
    /// falls back to hostname, then "Set N".
    #[serde(default)]
    pub set_name: String,
    /// Hostname of the source machine, read from the same manifest.
    /// Used as the GUI fallback label when `set_name` is empty.
    #[serde(default)]
    pub hostname: String,
}

/// Tauri-managed wrapper.  Read locks for snapshot/queries, write locks for
/// state transitions (phase changes, key store/clear, list refresh).
pub type SharedSession = Arc<RwLock<Session>>;

/// Build a fresh shared session.
pub fn new_shared() -> SharedSession {
    Arc::new(RwLock::new(Session::default()))
}
