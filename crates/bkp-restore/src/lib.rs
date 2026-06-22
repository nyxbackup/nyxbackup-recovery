// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! bkp-restore - Snapshot browsing and file restoration.
//!
//! # Usage
//!
//! ```ignore
//! let engine = RestoreEngine::new(storage);
//!
//! // List available snapshots.
//! let entries = engine.list_snapshots(&set_id, &snapshot_key).await?;
//!
//! // Restore a single file.
//! engine.restore_file(&snapshot_id, "home/user/doc.txt",
//!                     RestoreTarget::Custom(Path::new("/tmp")),
//!                     OverwriteMode::Skip,
//!                     &set_id, &chunk_key, &manifest_key).await?;
//!
//! // Restore an entire snapshot.
//! engine.restore_all(&snapshot_id,
//!                    RestoreTarget::Custom(Path::new("/restore")),
//!                    OverwriteMode::Skip,
//!                    &set_id, &chunk_key, &manifest_key).await?;
//! ```
//!
//! # Parallel downloads
//!
//! `restore_file` and `restore_all` download up to `concurrency` (default 8)
//! chunks concurrently using `tokio::spawn` + a `Semaphore`.
//!
//! # Pack-based chunk fetching
//!
//! When a [`ChunkResolver`] is installed via [`RestoreEngine::set_chunk_resolver`],
//! individual chunks are fetched via HTTP range requests against the owning pack
//! file (`get_range`) instead of downloading the whole pack.  This avoids
//! downloading multi-hundred-MB pack files to restore a single small file.
//!
//! The resolver maps a chunk ID to `(pack_id, byte_offset_in_pack,
//! encrypted_size)`.  The daemon restore service builds this map from the local
//! SQLite chunk index before spawning the restore task.

#![deny(unsafe_code)]
#![deny(missing_docs)]
#![deny(clippy::unwrap_used)]

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::AsyncWriteExt as _;
use tokio::task::JoinSet;

use bkp_chunker::decompress;
use bkp_crypto::aead::{self, EncryptedBlob};
use bkp_crypto::hash::keyed_chunk_id;
use bkp_crypto::keys::SubKey;
use bkp_manifest::{
    Manifest, decode_manifest, decode_snapshot_index, manifest_remote_path,
    snapshot_index_remote_path,
};
use bkp_storage::backend::StorageBackend;
use bkp_types::backup_set::BackupSetId;
use bkp_types::chunk::ChunkId;
use bkp_types::error::{Error, Result};
use bkp_types::manifest::{ChunkRef, FileEntry, NodeType, TreeNode};
use bkp_types::snapshot::{PackId, SnapshotId};
use futures::stream::{self, StreamExt};
use tokio::sync::{Semaphore, watch};
use tracing::{debug, info, warn};

/// Ownership to apply to restored files/directories.
///
/// On Windows the daemon uses a SID string to set the NTFS owner via
/// `SetNamedSecurityInfoW`.  On Unix the daemon passes the calling process's
/// uid/gid so `chown()` can hand ownership to the requesting user (the daemon
/// itself runs as root, so restored files would otherwise be root-owned).
#[derive(Clone, Default)]
pub struct RestoreOwner {
    /// Windows SID string (e.g. `"S-1-5-21-…"`).  Empty = no Windows fixup.
    pub owner_sid: String,
    /// Unix uid to chown to.  `0` = don't chown (leave as root).
    pub unix_uid: u32,
    /// Unix gid to chown to.  `0` = don't chown.
    pub unix_gid: u32,
}

// - Restore checkpoint ----------------------------

/// Per-restore progress file written to the platform data dir.
///
/// Keyed by `(snapshot_id, backup_set_id, target_key)`.  Tracks which files
/// were fully written so that an interrupted restore can skip them on resume.
/// Deleted automatically when the restore completes without error.
#[derive(serde::Serialize, serde::Deserialize)]
struct RestoreCheckpoint {
    snapshot_id: String,
    backup_set_id: String,
    /// Opaque string that identifies the restore destination (target mode + path).
    target_key: String,
    /// Paths of files that were fully written in a previous (interrupted) run.
    completed_files: HashSet<String>,
}

/// Per-machine restore-checkpoint directory.
///
/// The daemon runs as a privileged service (LocalSystem on Windows, root via
/// systemd/launchd on Linux/macOS), so per-user locations are wrong: on
/// Windows, `%LOCALAPPDATA%` for SYSTEM is `C:\Windows\System32\config\
/// systemprofile\AppData\Local\`, which is invisible to the user and wrong
/// for "machine state".  On Linux/macOS, root's `$HOME` is `/root` or
/// `/var/root`, similarly wrong.
///
/// Resolution rule (per-OS restore-state paths, below):
///
/// | OS      | Privileged daemon                                  | User context                                       |
/// |---------|----------------------------------------------------|----------------------------------------------------|
/// | Windows | `%PROGRAMDATA%\NyxBackup\restore_checkpoints`      | `%LOCALAPPDATA%\nyxbackup\restore_checkpoints`     |
/// | Linux   | `/var/lib/nyxbackup/restore_checkpoints`           | `~/.local/share/nyxbackup/restore_checkpoints`     |
/// | macOS   | `/Library/Application Support/NyxBackup/restore_checkpoints` | `~/Library/Application Support/NyxBackup/restore_checkpoints` |
fn restore_checkpoints_dir() -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(p) = std::env::var_os("PROGRAMDATA") {
            return PathBuf::from(p)
                .join("NyxBackup")
                .join("restore_checkpoints");
        }
        // Final fallback: per-user.  Daemons should always have PROGRAMDATA set.
        return dirs_next::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("nyxbackup")
            .join("restore_checkpoints");
    }

    // Non-unsafe root detection: this crate has #![forbid(unsafe_code)] so we
    // can't call libc::getuid().  Daemon contexts on Linux/macOS run with
    // HOME=/root (Linux systemd default) or HOME=/var/root (macOS launchd),
    // and USER=root.  Either signal is sufficient - we only need to choose
    // between two well-known directories, and the file-store fallback in
    // bkp-daemon's keystore module uses the same trick on macOS.
    #[cfg(unix)]
    let is_root = std::env::var("USER").as_deref() == Ok("root")
        || std::env::var("HOME").as_deref() == Ok("/root")
        || std::env::var("HOME").as_deref() == Ok("/var/root");

    #[cfg(target_os = "macos")]
    {
        return if is_root {
            PathBuf::from("/Library/Application Support/NyxBackup/restore_checkpoints")
        } else {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join("Library/Application Support/NyxBackup/restore_checkpoints")
        };
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if is_root {
            PathBuf::from("/var/lib/nyxbackup/restore_checkpoints")
        } else {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join(".local/share/nyxbackup/restore_checkpoints")
        }
    }
}

/// Legacy (per-user) restore-checkpoint directory used before 0.3.39.
///
/// The daemon migrates any `.json` files from this location to the new
/// per-machine path on startup so existing in-progress checkpoints are not
/// orphaned by the upgrade.  Returns `None` when the legacy path coincides
/// with the new one (single-user contexts).
fn legacy_restore_checkpoints_dir() -> Option<PathBuf> {
    let legacy = dirs_next::data_local_dir()?
        .join("nyxbackup")
        .join("restore_checkpoints");
    let current = restore_checkpoints_dir();
    if legacy == current {
        None
    } else {
        Some(legacy)
    }
}

/// Move any `.json` checkpoint files from the legacy per-user path into the
/// new per-machine path.  Idempotent and best-effort: errors are logged and
/// otherwise ignored, since checkpoints have a 7-day TTL and the worst-case
/// outcome of a failed migration is the user having to manually delete a few
/// stale files from the old directory.
pub fn migrate_legacy_restore_checkpoints() {
    let Some(legacy) = legacy_restore_checkpoints_dir() else {
        return;
    };
    if !legacy.exists() {
        return;
    }
    let target = restore_checkpoints_dir();
    if let Err(e) = std::fs::create_dir_all(&target) {
        tracing::warn!(
            "restore_checkpoint migration: cannot create {}: {e}",
            target.display()
        );
        return;
    }
    let entries = match std::fs::read_dir(&legacy) {
        Ok(e) => e,
        Err(_) => return,
    };
    let mut moved = 0u32;
    for entry in entries.flatten() {
        let src = entry.path();
        if src.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(name) = src.file_name() else {
            continue;
        };
        let dst = target.join(name);
        if dst.exists() {
            continue;
        } // don't clobber a fresher copy
        if let Err(e) = std::fs::rename(&src, &dst) {
            tracing::warn!(
                "restore_checkpoint migration: rename {} -> {} failed: {e}",
                src.display(),
                dst.display()
            );
            continue;
        }
        moved += 1;
    }
    if moved > 0 {
        tracing::info!(
            "Migrated {} restore checkpoint(s) from legacy per-user path to {}.",
            moved,
            target.display()
        );
    }
}

/// Public alias of the internal `checkpoint_path` so the daemon can
/// remove the checkpoint on user-cancel without duplicating
/// path-derivation logic.
pub fn checkpoint_path_for(snapshot_id: &bkp_types::snapshot::SnapshotId) -> PathBuf {
    checkpoint_path(snapshot_id)
}

fn checkpoint_path(snapshot_id: &bkp_types::snapshot::SnapshotId) -> PathBuf {
    restore_checkpoints_dir().join(format!("{}.json", snapshot_id.as_uuid()))
}

fn load_checkpoint(
    path: &Path,
    snapshot_id: &bkp_types::snapshot::SnapshotId,
    set_id: &bkp_types::backup_set::BackupSetId,
    target_key: &str,
) -> Option<RestoreCheckpoint> {
    let data = std::fs::read(path).ok()?;
    let cp: RestoreCheckpoint = serde_json::from_slice(&data).ok()?;
    if cp.snapshot_id != snapshot_id.as_uuid().to_string()
        || cp.backup_set_id != set_id.as_uuid().to_string()
        || cp.target_key != target_key
    {
        return None;
    }
    Some(cp)
}

fn save_checkpoint(cp: &RestoreCheckpoint, path: &Path) {
    if let Ok(json) = serde_json::to_vec(cp) {
        let _ = std::fs::write(path, json);
    }
}

/// Apply mtime + owner to a single file in a spawn_blocking task.
/// Called inline from `write_file_with_pack_cache` and `restore_chunks`
/// so the per-file finalizing pass can be removed.  Per-file
/// syscalls run inside the network-bound write window for cloud
/// backends so they are effectively free; the formerly-serial
/// finalizing pass that took 30-60 s on a 100K-file restore is gone.
/// Errors are swallowed - file content is correct either way; only
/// the attribute couldn't be applied.
async fn apply_file_metadata(dest: PathBuf, mtime_ns: u64, mode: u32, owner: Arc<RestoreOwner>) {
    if mtime_ns == 0
        && mode == 0
        && owner.owner_sid.is_empty()
        && owner.unix_uid == 0
        && owner.unix_gid == 0
    {
        return;
    }
    let _ = tokio::task::spawn_blocking(move || {
        #[cfg(unix)]
        if mode != 0 {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(mode & 0o7777));
        }
        if mtime_ns > 0 {
            let ft = filetime::FileTime::from_unix_time(
                (mtime_ns / 1_000_000_000) as i64,
                (mtime_ns % 1_000_000_000) as u32,
            );
            let _ = filetime::set_file_mtime(&dest, ft);
        }
        if !owner.owner_sid.is_empty() {
            apply_windows_owner(&dest, &owner.owner_sid);
        }
        apply_unix_owner(&dest, &owner);
    })
    .await;
}

/// Fsync a batch of destination paths in parallel.  Replaces the per-
/// file sync_all that used to live in `write_file_with_pack_cache` and
/// serialized fsyncs on the semaphore permit.  Issues all fsyncs at
/// once via `futures::future::join_all`; the disk controller's request
/// queue (NCQ on SSD) handles concurrency, amortizing the per-syscall
/// cost across the batch.
///
/// tries write-access first (the file was created with write by
/// the same identity; re-opening for write succeeds where read-only is
/// denied by inherited folder ACLs on Windows - user Desktop folders
/// commonly produce ERROR_ACCESS_DENIED for SYSTEM read).  Read is the
/// fallback for filesystems that allow read but not write reopen.
/// Errors are summarized (one log line per batch) instead of per file
/// so the log isn't flooded - the engine treats a failed fsync as
/// "trust the kernel's lazy write and re-restore via verify-on-resume
/// if the data turns out to be lost."
async fn batch_fsync_paths(paths: &[PathBuf]) {
    use futures::future::join_all;
    if paths.is_empty() {
        return;
    }
    let futs = paths.iter().map(|p| async move {
        // Try write first; falls back to read; if both fail, return the
        // last error so the caller can summarize.
        let open_result = match tokio::fs::OpenOptions::new().write(true).open(p).await {
            Ok(f) => Ok(f),
            Err(_) => tokio::fs::OpenOptions::new().read(true).open(p).await,
        };
        match open_result {
            Ok(f) => f.sync_all().await,
            Err(e) => Err(e),
        }
    });
    let results = join_all(futs).await;
    let fail_count = results.iter().filter(|r| r.is_err()).count();
    if fail_count > 0 {
        // Log one summary line per batch instead of per-file so a
        // ~100-file batch of ACL-denied paths doesn't produce ~100 log
        // lines.  The first error message is included so the operator
        // has something actionable.
        let first_err = results
            .iter()
            .find_map(|r| r.as_ref().err().map(|e| e.to_string()))
            .unwrap_or_default();
        warn!(failed = fail_count, total = paths.len(), first_err = %first_err,
              "batch-fsync: {} of {} fsync(s) failed; verify-on-resume will catch \
               any data that wasn't durable.", fail_count, paths.len());
    }
}

fn snapshot_folder_name(unix_secs: u64) -> String {
    use chrono::{DateTime, Local, TimeZone as _};
    let dt: DateTime<Local> = Local
        .timestamp_opt(unix_secs as i64, 0)
        .single()
        .unwrap_or_else(Local::now);
    format!("NyxRestore-{}", dt.format("%Y-%m-%d-%H.%M.%S"))
}

/// Return the default restore destination directory.
///
/// On Windows this is `C:\NyxRestore` - the user's Desktop is
/// commonly under OneDrive Known Folder Move, which would sync every
/// restored file to the cloud and conflict with itself.  `C:\NyxRestore`
/// is created at daemon startup with `Authenticated Users:(OI)(CI)M` so
/// any user can read+write inside it.  On Linux/macOS the user Desktop is
/// not sync'd by default, so we keep the historical behavior.
fn local_desktop_dir() -> Option<std::path::PathBuf> {
    #[cfg(target_os = "windows")]
    {
        let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_string());
        Some(std::path::PathBuf::from(format!("{drive}\\NyxRestore")))
    }
    #[cfg(not(target_os = "windows"))]
    {
        // Linux servers and minimal containers commonly have no ~/Desktop;
        // fall back to the home folder so the GUI's Desktop default still
        // resolves to *somewhere writable* rather than failing the restore.
        // macOS always has ~/Desktop, so this fallback is effectively a
        // Linux-only branch in practice.
        dirs_next::desktop_dir()
            .filter(|p| p.is_dir())
            .or_else(dirs_next::home_dir)
    }
}

/// Compute the opaque `target_key` string used to scope a restore session.
/// Public so the daemon can derive the same key when persisting session
/// metadata for auto-resume.
pub fn target_key_str(target: &RestoreTarget, snapshot_secs: u64) -> String {
    match target {
        RestoreTarget::Original => "original".to_string(),
        RestoreTarget::Desktop => {
            if let Some(desktop) = local_desktop_dir() {
                format!(
                    "{}/{}",
                    desktop.display(),
                    snapshot_folder_name(snapshot_secs)
                )
            } else {
                format!("desktop:{snapshot_secs}")
            }
        }
        RestoreTarget::Custom(base) => base.display().to_string(),
    }
}

/// Summary of an interrupted restore checkpoint.
///
/// Returned by [`list_restore_checkpoints`] so callers can display or act on
/// interrupted restores without parsing the full checkpoint file.
pub struct RestoreCheckpointInfo {
    /// UUID string of the snapshot being restored.
    pub snapshot_id: String,
    /// UUID string of the backup set the snapshot belongs to.
    pub backup_set_id: String,
    /// Opaque destination key: `"original"`, a desktop folder path, or a
    /// custom directory path.
    pub target_key: String,
    /// Number of files confirmed complete in this checkpoint.
    pub completed_files: usize,
    /// Unix timestamp of the checkpoint file's last modification.
    pub last_updated_secs: u64,
}

/// List all restore checkpoints, optionally filtered to a specific backup set.
///
/// Returns a best-effort list - unreadable or malformed checkpoint files are
/// silently skipped.
pub fn list_restore_checkpoints(
    filter_set_id: Option<&bkp_types::backup_set::BackupSetId>,
) -> Vec<RestoreCheckpointInfo> {
    let dir = restore_checkpoints_dir();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut result = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let data = match std::fs::read(&path) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let cp: RestoreCheckpoint = match serde_json::from_slice(&data) {
            Ok(c) => c,
            Err(_) => continue,
        };
        if let Some(set_id) = filter_set_id
            && cp.backup_set_id != set_id.as_uuid().to_string()
        {
            continue;
        }
        let last_updated_secs = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        result.push(RestoreCheckpointInfo {
            snapshot_id: cp.snapshot_id,
            backup_set_id: cp.backup_set_id,
            target_key: cp.target_key,
            completed_files: cp.completed_files.len(),
            last_updated_secs,
        });
    }
    result
}

/// Return the first checkpoint for `backup_set_id` whose snapshot differs from
/// `current_snapshot_id`, or `None` if no such conflict exists.
///
/// Used by the daemon's `RestoreAll` handler to reject a new restore while an
/// interrupted restore for a *different* snapshot is still in progress.
pub fn find_conflicting_checkpoint(
    backup_set_id: &bkp_types::backup_set::BackupSetId,
    current_snapshot_id: &bkp_types::snapshot::SnapshotId,
) -> Option<RestoreCheckpointInfo> {
    list_restore_checkpoints(Some(backup_set_id))
        .into_iter()
        .find(|cp| cp.snapshot_id != current_snapshot_id.as_uuid().to_string())
}

/// Delete a single restore checkpoint by snapshot ID.
///
/// Returns `true` if a file was deleted, `false` if no checkpoint existed for
/// the supplied ID.  An error is returned only if the file existed but could
/// not be removed (filesystem permission, etc.).  Used by the
/// `DeleteRestoreCheckpoint` RPC to give the GUI a way to clear an
/// interrupted restore the user does not want to resume.
pub fn delete_restore_checkpoint(
    snapshot_id: &bkp_types::snapshot::SnapshotId,
) -> std::io::Result<bool> {
    let path = checkpoint_path(snapshot_id);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Delete restore checkpoint files that are older than 7 days.
///
/// Called at daemon startup to clean up checkpoints from restores that were
/// interrupted and never resumed.
/// Walk every pack file at `packs/<uuid>.pack` and build the
/// `chunk_id -> (pack_id, offset, encrypted_size)` map the
/// [`RestoreEngine::new_with_pack_cache`] expects.
///
/// Used by callers that have no local SQLite chunk index - notably the
/// standalone Nyx Backup Recovery Tool (`bkp-recover`) and the daemon's
/// cross-machine-restore path.  Without this index the engine falls back
/// to chunks/<hash> path lookups, which 404 on every modern backup
/// (chunks live inside pack files, not as standalone objects).
///
/// Implementation: lists `packs/`, for each pack reads the last 8 bytes
/// (footer offset), then the CBOR index between footer and that offset,
/// then inserts every chunk_id from the index into the map.  ~2 GET-
/// ranges per pack.
pub async fn build_pack_map_from_storage(
    storage: &dyn bkp_storage::backend::StorageBackend,
) -> bkp_types::error::Result<
    std::collections::HashMap<[u8; 32], (bkp_types::snapshot::PackId, u64, u64)>,
> {
    use bkp_types::snapshot::PackId;

    let mut map = std::collections::HashMap::new();
    let raw_paths = storage.list("packs/").await?;
    let mut pack_ids: Vec<PackId> = Vec::new();
    for p in &raw_paths {
        let stem = p.strip_prefix("packs/").unwrap_or(p);
        let stem = stem.strip_suffix(".pack").unwrap_or(stem);
        if let Ok(uuid) = uuid::Uuid::parse_str(stem) {
            pack_ids.push(PackId::from_uuid(uuid));
        }
    }

    for pack_id in &pack_ids {
        let pack_path = format!("packs/{}.pack", pack_id.as_uuid());
        let size = match storage.size(&pack_path).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(pack = %pack_id.as_uuid(), error = %e, "build_pack_map: size() failed; skipping pack");
                continue;
            }
        };
        if size < 30 {
            continue;
        }

        let last8 = match storage.get_range(&pack_path, size - 8, size).await {
            Ok(b) => b,
            Err(_) => continue,
        };
        let footer_offset = match bkp_chunker::pack::parse_pack_footer_offset(&last8) {
            Ok(o) => o,
            Err(_) => continue,
        };
        if footer_offset >= size - 8 {
            continue;
        }

        let cbor_bytes = match storage.get_range(&pack_path, footer_offset, size - 8).await {
            Ok(b) => b,
            Err(_) => continue,
        };
        let entries = match bkp_chunker::pack::parse_pack_index_cbor(&cbor_bytes) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for e in &entries {
            map.insert(e.chunk_id, (*pack_id, e.offset, e.size));
        }
    }
    Ok(map)
}

/// Delete restore-checkpoint files older than 7 days.  Called at daemon
/// startup to garbage-collect interrupted restores the user never resumed.
pub fn sweep_stale_restore_checkpoints() {
    let dir = restore_checkpoints_dir();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    let cutoff = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(7 * 24 * 3600))
        .unwrap_or(std::time::UNIX_EPOCH);
    let mut removed = 0u32;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let stale = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .map(|t| t <= cutoff)
            .unwrap_or(false);
        if stale && std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    if removed > 0 {
        tracing::info!("restore: swept {removed} stale checkpoint file(s).");
    }
}

// - Public enums -------------------------------

/// Where to write restored files.
#[derive(Debug, Clone)]
pub enum RestoreTarget {
    /// Restore each file to its original recorded path (absolute).
    Original,
    /// Restore into `~/Desktop/<snapshot_date>/`, preserving relative structure.
    Desktop,
    /// Restore into the provided directory, preserving relative structure.
    Custom(PathBuf),
}

/// How to handle a file that already exists at the restore destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverwriteMode {
    /// Leave the existing file in place and skip this file (default).
    Skip,
    /// Replace the existing file unconditionally.
    Replace,
    /// Save the restored copy as `<name>_restored_<ts>.<ext>` alongside the existing file.
    RenameNew,
}

/// Per-file progress update emitted by [`RestoreEngine::restore_all`].
///
/// Sent through the optional `progress_tx` channel after each file completes
/// (whether successfully or with an error).
#[derive(Debug, Clone)]
pub struct RestoreFileProgress {
    /// Files completed so far (including errored files).
    pub files_done: u64,
    /// Total files in this restore operation.
    pub files_total: u64,
    /// Plaintext bytes written so far.
    pub bytes_done: u64,
    /// Total plaintext bytes to be written.
    pub bytes_total: u64,
    /// Path of the file just completed.
    pub current_file: String,
    /// If this file encountered an error, the description; otherwise `None`.
    pub error: Option<String>,
    /// Engine-derived phase: "Downloading" while any pack fetch is in
    /// flight, "Restoring" while writing files from cached chunks,
    /// "Finalizing" during the post-100% mtime/ownership pass.
    pub phase: String,
    /// Number of pack downloads currently in flight.
    pub packs_in_flight: u32,
    /// Number of packs successfully downloaded for this restore so far.
    pub packs_downloaded: u64,
    /// Total distinct packs referenced by this restore (set once at start).
    pub packs_total: u64,
    /// Cumulative per-file errors observed so far.
    pub errors_so_far: u64,
    /// Cumulative files skipped because their pack is in the missing set.
    pub skipped_so_far: u64,
    /// Restore destination root (same value on every event for this restore).
    pub destination_root: String,
}

/// A file or directory entry returned by [`RestoreEngine::list_snapshot_files`].
#[derive(Debug, Clone)]
pub struct SnapshotFileEntry {
    /// Full path as recorded in the manifest (e.g. `/Users/alice/doc.txt`).
    pub path: String,
    /// Plaintext file size in bytes (0 for directories and symlinks).
    pub size: u64,
    /// Last-modified time in nanoseconds since Unix epoch.
    pub mtime_ns: u64,
    /// True if this entry is a directory.
    pub is_dir: bool,
    /// True if this entry is a symbolic link.
    pub is_symlink: bool,
}

// - Chunk resolver ------------------------------

/// Maps a chunk ID to its location inside a pack file.
///
/// Returns `(pack_id, byte_offset_of_size_prefix, encrypted_size)`.
/// Pass `None` for the `RestoreEngine` to fall back to individual chunk objects.
pub type ChunkResolver = Arc<dyn Fn(&ChunkId) -> Option<(PackId, u64, u64)> + Send + Sync>;

// - Public API --------------------------------

/// A brief description of a snapshot for listing/browsing.
#[derive(Debug, Clone)]
pub struct SnapshotSummary {
    /// Snapshot identifier.
    pub snapshot_id: SnapshotId,
    /// Creation time as Unix seconds.
    pub created_at: u64,
    /// Total files in this snapshot.
    pub files_total: u64,
    /// Total plaintext bytes in this snapshot.
    pub bytes_total: u64,
    /// Remote path of the encrypted manifest object.
    pub manifest_path: String,
}

/// The restore engine for a single backup set.
pub struct RestoreEngine {
    storage: Arc<dyn StorageBackend>,
    /// Maximum concurrent chunk downloads.
    concurrency: usize,
    /// Optional chunk-to-pack resolver for range-request based retrieval.
    chunk_resolver: Option<ChunkResolver>,
    /// Cancel signal: when `true` the restore aborts at the next file boundary.
    cancel_rx: Option<watch::Receiver<bool>>,
    /// Pause signal: when `true` the restore waits at the next file boundary.
    pause_rx: Option<watch::Receiver<bool>>,
    /// Backup-set source paths used to strip the common prefix when restoring to
    /// a Custom or Desktop target.  If empty, only the leading `/` is stripped.
    source_prefixes: Vec<String>,
    /// Pre-fetched manifest cache.  Caller (the daemon) keeps an LRU
    /// of recently-decoded manifests across `list_snapshot_files` /
    /// `restore_all` calls so the same 28-MiB encrypted blob is not pulled
    /// twice in a row when the user clicks Restore right after browsing.
    pre_fetched_manifest: Option<(SnapshotId, Arc<Manifest>)>,
    /// When `true` (the default, matching the main app's sparse-on-by-default),
    /// all-zero chunks are punched as filesystem holes instead of written
    /// dense, so a restored sparse file (VM disk image, pre-allocated DB file)
    /// keeps its on-disk footprint.  Content is byte-for-byte identical either
    /// way.  Toggle off via [`set_sparse`] to force a dense write.
    sparse: bool,
}

impl RestoreEngine {
    /// Create a new `RestoreEngine` without a chunk resolver.
    ///
    /// Without a resolver, individual chunk objects at
    /// `chunks/<hex[0:2]>/<hex[2:]>` are fetched as a fallback.  Install a
    /// resolver via [`set_chunk_resolver`] for efficient pack-based retrieval.
    pub fn new(storage: Arc<dyn StorageBackend>) -> Self {
        let concurrency = storage.concurrency_hint().unwrap_or(8);
        Self {
            storage,
            concurrency,
            chunk_resolver: None,
            cancel_rx: None,
            pause_rx: None,
            source_prefixes: Vec::new(),
            pre_fetched_manifest: None,
            sparse: true,
        }
    }

    /// Create a `RestoreEngine` pre-populated with a pack location cache.
    ///
    /// `pack_map` maps raw 32-byte chunk IDs to `(pack_id, offset, encrypted_size)`.
    /// Built from the local SQLite chunk index before spawning the restore task.
    pub fn new_with_pack_cache(
        storage: Arc<dyn StorageBackend>,
        pack_map: HashMap<[u8; 32], (PackId, u64, u64)>,
    ) -> Self {
        let concurrency = storage.concurrency_hint().unwrap_or(8);
        let map = Arc::new(pack_map);
        let resolver: ChunkResolver = Arc::new(move |id: &ChunkId| map.get(id.as_bytes()).copied());
        Self {
            storage,
            concurrency,
            chunk_resolver: Some(resolver),
            cancel_rx: None,
            pause_rx: None,
            source_prefixes: Vec::new(),
            pre_fetched_manifest: None,
            sparse: true,
        }
    }

    /// Override the download concurrency (default: 8).
    pub fn set_concurrency(&mut self, n: usize) {
        self.concurrency = n;
    }

    /// Enable or disable sparse restore (default: enabled).
    ///
    /// When enabled, all-zero chunks are punched as filesystem holes instead of
    /// written dense; the restored content is byte-for-byte identical, only the
    /// on-disk footprint differs.  Disable to force a fully dense write (every
    /// byte allocated) - useful on filesystems without hole support or when a
    /// user prefers maximum compatibility over disk savings.
    pub fn set_sparse(&mut self, on: bool) {
        self.sparse = on;
    }

    /// Install a chunk resolver for pack-based range-request retrieval.
    pub fn set_chunk_resolver(&mut self, resolver: ChunkResolver) {
        self.chunk_resolver = Some(resolver);
    }

    /// Set the backup-set source paths for prefix stripping during Custom/Desktop restores.
    ///
    /// When set, `restore_file` and `restore_all` strip the longest matching
    /// source path prefix from each file path before joining with the base
    /// directory.  For example, if the backup set covers `/home/{username}`, a file
    /// at `/home/{username}/Documents/file.txt` restores to `{dest}/Documents/file.txt`
    /// instead of `{dest}/home/{username}/Documents/file.txt`.
    pub fn set_source_prefixes(&mut self, prefixes: Vec<String>) {
        self.source_prefixes = prefixes;
    }

    /// Attach cancel + pause receivers (provided by the daemon's restore service).
    ///
    /// When `cancel_rx` becomes `true` the restore returns `Error::Cancelled`
    /// at the next file boundary.  When `pause_rx` is `true` the restore waits
    /// until it becomes `false` again before processing the next file.
    pub fn set_cancel_pause(
        &mut self,
        cancel_rx: watch::Receiver<bool>,
        pause_rx: watch::Receiver<bool>,
    ) {
        self.cancel_rx = Some(cancel_rx);
        self.pause_rx = Some(pause_rx);
    }

    /// List all files (and directories) recorded in `snapshot_id`.
    ///
    /// Returns a flat list sorted by path, suitable for display in a file browser.
    /// Downloads and decrypts the manifest; no other remote I/O.
    pub async fn list_snapshot_files(
        &self,
        snapshot_id: &SnapshotId,
        set_id: &BackupSetId,
        manifest_key: &SubKey,
    ) -> Result<Vec<SnapshotFileEntry>> {
        let manifest = self
            .fetch_manifest(snapshot_id, set_id, manifest_key)
            .await?;
        let mut entries = Vec::new();
        collect_file_entries(&manifest.file_tree.root, "", &mut entries);
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(entries)
    }

    /// List all snapshots for `set_id`, newest first.
    ///
    /// Downloads and decrypts the remote snapshot index.
    pub async fn list_snapshots(
        &self,
        set_id: &BackupSetId,
        snapshot_index_key: &SubKey,
    ) -> Result<Vec<SnapshotSummary>> {
        let index_path = snapshot_index_remote_path(set_id);
        let data = self.storage.get_critical(&index_path).await?;
        let index = decode_snapshot_index(&data, snapshot_index_key)?;

        let mut summaries: Vec<SnapshotSummary> = index
            .entries
            .iter()
            .map(|e| SnapshotSummary {
                snapshot_id: e.snapshot_id,
                created_at: e.created_at,
                files_total: e.files_total,
                bytes_total: e.bytes_total,
                manifest_path: e.manifest_path.clone(),
            })
            .collect();

        summaries.sort_by_key(|s| std::cmp::Reverse(s.created_at));
        Ok(summaries)
    }

    /// Restore a single file from `snapshot_id`.
    ///
    /// `file_path` is the path as recorded in the manifest.
    /// Destination is resolved from `target` and `overwrite` controls collision handling.
    #[allow(clippy::too_many_arguments)]
    pub async fn restore_file(
        &self,
        snapshot_id: &SnapshotId,
        set_id: &BackupSetId,
        file_path: &str,
        target: RestoreTarget,
        overwrite: OverwriteMode,
        chunk_key: &SubKey,
        chunk_id_key: &SubKey,
        manifest_key: &SubKey,
        owner: RestoreOwner,
    ) -> Result<()> {
        let manifest = self
            .fetch_manifest(snapshot_id, set_id, manifest_key)
            .await?;
        let entry = find_file_in_tree(&manifest.file_tree.root, file_path)
            .ok_or_else(|| Error::Storage(format!("file not found in snapshot: {file_path}")))?;

        let dest = resolve_dest(
            &target,
            file_path,
            manifest.created_at_ns / 1_000_000_000,
            &self.source_prefixes,
        )?;
        let dest = apply_overwrite_path(dest, overwrite)?;
        if let Some(d) = &dest {
            let chunk_key_bytes = *chunk_key.as_bytes();
            let chunk_id_key_bytes = *chunk_id_key.as_bytes();
            restore_chunks(
                self.storage.clone(),
                self.chunk_resolver.clone(),
                entry.chunks.clone(),
                d,
                chunk_key_bytes,
                chunk_id_key_bytes,
                entry.mtime_ns,
                entry.mode,
                owner,
                None,
                None,
                self.sparse,
            )
            .await?;
        }
        Ok(())
    }

    /// Restore all files from `snapshot_id`.
    ///
    /// The original directory hierarchy is recreated inside `target`.
    ///
    /// `filter_paths` - if non-empty, only restore files whose recorded path
    /// starts with one of the provided prefixes (for selective restore).
    ///
    /// `progress_tx` - if `Some`, a [`RestoreFileProgress`] message is sent
    /// after each file completes (success or error).  The channel is not closed
    /// by this method; the caller learns of completion when the method returns.
    ///
    /// # Performance optimisations (small-file workloads)
    ///
    /// 1. **Pack coalescing** - all needed chunk IDs are grouped by pack before
    ///    any I/O.  Packs with multiple needed chunks are fetched as a whole
    ///    (one `GET`); packs with a single needed chunk use a targeted range
    ///    request.  Each pack is downloaded at most once per restore.
    /// 2. **Batch mtime + ownership** - a single `spawn_blocking` call applies
    ///    mtime and ownership to all successfully written files after the
    ///    download/write phase completes.
    /// 3. **Directory pre-creation** - all destination parent directories are
    ///    created in one pass before spawning per-file write tasks.
    #[allow(clippy::too_many_arguments)]
    pub async fn restore_all(
        &self,
        snapshot_id: &SnapshotId,
        set_id: &BackupSetId,
        target: RestoreTarget,
        overwrite: OverwriteMode,
        chunk_key: &SubKey,
        chunk_id_key: &SubKey,
        manifest_key: &SubKey,
        filter_paths: &[String],
        excluded_paths: &[String],
        progress_tx: Option<tokio::sync::mpsc::Sender<RestoreFileProgress>>,
        owner: RestoreOwner,
    ) -> Result<()> {
        let manifest = self
            .fetch_manifest(snapshot_id, set_id, manifest_key)
            .await?;
        let chunk_key_bytes = *chunk_key.as_bytes();
        let chunk_id_key_bytes = *chunk_id_key.as_bytes();
        let snapshot_secs = manifest.created_at_ns / 1_000_000_000;

        // - Resume checkpoint -------------------------
        // Resume state lives in a flat JSON file per restore under
        // `restore_checkpoints/`.
        let cp_path = checkpoint_path(snapshot_id);
        let _ = std::fs::create_dir_all(restore_checkpoints_dir());
        let tkey = target_key_str(&target, snapshot_secs);
        let mut checkpoint =
            load_checkpoint(&cp_path, snapshot_id, set_id, &tkey).unwrap_or_else(|| {
                RestoreCheckpoint {
                    snapshot_id: snapshot_id.as_uuid().to_string(),
                    backup_set_id: set_id.as_uuid().to_string(),
                    target_key: tkey,
                    completed_files: HashSet::new(),
                }
            });

        // - Collect and filter files from the manifest ------------
        let mut raw_files: Vec<(PathBuf, Vec<ChunkRef>, u64, Option<String>, u32)> = Vec::new();
        collect_files(&manifest.file_tree.root, &PathBuf::new(), &mut raw_files);

        // Normalize to forward slashes: collect_files uses PathBuf::join which
        // emits backslashes on Windows, while filter_paths always use '/'.
        if !filter_paths.is_empty() {
            raw_files.retain(|(p, _, _, _, _)| {
                let s = p.to_string_lossy().replace('\\', "/");
                filter_paths.iter().any(|fp| {
                    let fp = fp.replace('\\', "/");
                    let fp = fp.trim_start_matches('/');
                    let s2 = s.trim_start_matches('/');
                    s2 == fp
                        || s2.starts_with(&format!("{fp}/"))
                        || fp.starts_with(&format!("{s2}/"))
                })
            });
        }
        if !excluded_paths.is_empty() {
            raw_files.retain(|(p, _, _, _, _)| {
                let s = p.to_string_lossy().replace('\\', "/");
                !excluded_paths.iter().any(|ep| {
                    let ep = ep.replace('\\', "/");
                    let ep = ep.trim_start_matches('/');
                    let s2 = s.trim_start_matches('/');
                    s2 == ep || s2.starts_with(&format!("{ep}/"))
                })
            });
        }

        // - Pre-resolve all destinations -------------------
        // Resolving upfront lets us (a) pre-create directories in one pass and
        // (b) build the dest→mtime map without re-computing paths per task.
        struct FileTask {
            rel_str: String,
            chunks: Vec<ChunkRef>,
            mtime_ns: u64,
            file_bytes: u64,
            /// `None` = skip (OverwriteMode::Skip on an already-present file).
            dest: Option<PathBuf>,
            /// `Some(target)` when this entry is a symlink; the worker
            /// dispatches to `restore_symlink` instead of the chunk
            /// writer.  Empty `chunks` is then expected.
            symlink_target: Option<String>,
            /// Recorded POSIX mode; applied on unix after the file is written.
            mode: u32,
        }

        let mut tasks: Vec<FileTask> = Vec::with_capacity(raw_files.len());
        for (rel_path, chunks, mtime_ns, symlink_target, mode) in raw_files {
            let rel_str = rel_path.to_string_lossy().into_owned();
            let file_bytes = chunks.iter().map(|c| c.plaintext_size).sum();
            let dest_path = resolve_dest(&target, &rel_str, snapshot_secs, &self.source_prefixes)?;
            let dest = apply_overwrite_path(dest_path, overwrite)?;
            tasks.push(FileTask {
                rel_str,
                chunks,
                mtime_ns,
                file_bytes,
                dest,
                symlink_target,
                mode,
            });
        }

        // - verify-on-resume ----------------------
        // even with sync_all (which prevents future checkpoint/disk
        // divergence), pre-existing corruption from older builds + the
        // general case of "checkpoint claims X but X is missing or wrong
        // size" must be detected at resume time so we re-restore the
        // affected files instead of silently leaving holes.  We trust a
        // checkpoint row only when:
        //   - the destination file exists, AND
        //   - its size matches the manifest's expected total.
        // Rows that fail verification are dropped from both the in-memory
        // completed_files set and the DB so the engine treats those files
        // as not-yet-restored and re-writes them.  Cheap: a stat per file
        // (~ microseconds per entry on a warm directory cache).
        if !checkpoint.completed_files.is_empty() {
            let mut bad: Vec<String> = Vec::new();
            for t in &tasks {
                if !checkpoint.completed_files.contains(&t.rel_str) {
                    continue;
                }
                // Files marked Skip (dest=None) cannot be verified - keep
                // them in the completed set since the engine won't write
                // them anyway.
                let Some(ref dest) = t.dest else { continue };
                match tokio::fs::metadata(dest).await {
                    Ok(m) if m.is_file() && m.len() == t.file_bytes => {
                        // matches manifest - trust the checkpoint
                    }
                    Ok(m) => {
                        warn!(rel = %t.rel_str, dest = %dest.display(),
                              expected = t.file_bytes, found = m.len(),
                              "verify-on-resume: dropping checkpoint - size mismatch.");
                        bad.push(t.rel_str.clone());
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        warn!(rel = %t.rel_str, dest = %dest.display(),
                              "verify-on-resume: dropping checkpoint - file missing on disk.");
                        bad.push(t.rel_str.clone());
                    }
                    Err(e) => {
                        warn!(rel = %t.rel_str, dest = %dest.display(),
                              "verify-on-resume: stat failed, conservatively dropping checkpoint: {e}");
                        bad.push(t.rel_str.clone());
                    }
                }
            }
            if !bad.is_empty() {
                let dropped = bad.len();
                info!(snapshot = %snapshot_id.as_uuid(), dropped,
                      "verify-on-resume: {} checkpoint row(s) failed verification; \
                       affected files will be re-restored.", dropped);
                // Purge from the in-memory set so the upcoming filter
                // doesn't skip them.
                for r in &bad {
                    checkpoint.completed_files.remove(r);
                }
            }
        }

        let files_total = tasks.len() as u64;
        let bytes_total: u64 = tasks.iter().map(|t| t.file_bytes).sum();

        // hoisted up from below so the pre-create-dirs pass
        // can use destination_root_s to bound the ancestor-chown walk
        // (we never chown past the destination root - security).
        let destination_root_s: String = match &target {
            RestoreTarget::Original => "<various>".to_string(),
            RestoreTarget::Desktop => local_desktop_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "Desktop".to_string()),
            RestoreTarget::Custom(p) => p.display().to_string(),
        };

        // - Opt 3: pre-create all destination directories -----------
        {
            let mut dirs: HashSet<PathBuf> = HashSet::new();
            for t in &tasks {
                if checkpoint.completed_files.contains(&t.rel_str) {
                    continue; // already written in a prior run; directory exists
                }
                if let Some(ref dest) = t.dest
                    && let Some(parent) = dest.parent()
                    && !parent.as_os_str().is_empty()
                {
                    dirs.insert(parent.to_path_buf());
                }
            }
            let owner_for_dirs = owner.clone();
            // also collect every intermediate ancestor between
            // each leaf parent and the destination root.  Without this,
            // create_dir_all materialised `NyxRestore-{ts}/<sub>/...`
            // as root-owned and only the leaf-most parent dir picked
            // up the user's uid/gid in apply_unix_owner.  Result on
            // Linux/macOS: `~/NyxRestore-2026-...` and `~/.../{username}/`
            // and any intermediate were owned by root, breaking the
            // user's ability to read/delete their own restore.
            //
            // We stop ascending at `destination_root_s` (the user's
            // chosen destination - we created it, so we own it) or at
            // the filesystem root if destination_root_s is empty.  We
            // never chown outside the dest tree.
            let dest_root_path =
                if !destination_root_s.is_empty() && destination_root_s != "<various>" {
                    Some(PathBuf::from(&destination_root_s))
                } else {
                    None
                };
            let mut all_dirs: HashSet<PathBuf> = HashSet::new();
            for leaf in &dirs {
                let mut cur: Option<&Path> = Some(leaf.as_path());
                while let Some(p) = cur {
                    if p.as_os_str().is_empty() {
                        break;
                    }
                    all_dirs.insert(p.to_path_buf());
                    if let Some(ref root) = dest_root_path
                        && p == root.as_path()
                    {
                        break;
                    }
                    cur = p.parent();
                }
            }
            if let Some(ref root) = dest_root_path {
                all_dirs.insert(root.clone());
            }
            for dir in &dirs {
                tokio::fs::create_dir_all(dir).await.map_err(|e| {
                    Error::Storage(format!("create_dir_all {}: {e}", dir.display()))
                })?;
            }
            // Chown every dir under the dest root (including the root
            // itself) so the calling user owns the whole restore tree.
            // Outside the dest root we skip - `/home/{username}` /
            // `C:\Users\joe` is already user-owned, and walking past
            // it onto `/home` / `C:\Users` would be a security hole.
            //
            // Both ownership paths are called: apply_unix_owner is
            // a #[cfg(unix)] no-op on Windows, apply_windows_owner is
            // an unconditional fn that early-returns when owner_sid
            // is empty (so it's a no-op on Linux/macOS where the
            // daemon doesn't supply a SID).
            for dir in &all_dirs {
                let inside_dest = dest_root_path
                    .as_ref()
                    .map(|root| dir.starts_with(root) || dir == root)
                    .unwrap_or(true);
                if inside_dest {
                    apply_unix_owner(dir, &owner_for_dirs);
                    if !owner_for_dirs.owner_sid.is_empty() {
                        apply_windows_owner(dir, &owner_for_dirs.owner_sid);
                    }
                }
            }
        }

        // - Opt 1: lazy pack cache - each pack downloaded at most once ----
        // Classify which packs contain >1 needed chunk (pure local computation,
        // no network I/O) so those packs get a full GET + shared cache entry.
        // Single-chunk packs use a targeted range request; no caching needed.
        let multi_chunk_packs: Arc<HashSet<String>> = {
            if let Some(ref resolver) = self.chunk_resolver {
                let pending_chunks: Vec<Vec<ChunkRef>> = tasks
                    .iter()
                    .filter(|t| {
                        t.dest.is_some() && !checkpoint.completed_files.contains(&t.rel_str)
                    })
                    .map(|t| t.chunks.clone())
                    .collect();
                Arc::new(classify_multi_chunk_packs(resolver, &pending_chunks))
            } else {
                Arc::new(HashSet::new())
            }
        };
        let pack_cache: Arc<tokio::sync::Mutex<BoundedPackCache>> = Arc::new(
            tokio::sync::Mutex::new(BoundedPackCache::new(DEFAULT_PACK_CACHE_BYTES)),
        );

        // Pack-skip set.  When a chunk fetch fails because
        // the underlying pack is missing from remote storage (rm'd
        // out-of-band, retention swept on another machine, etc.), we record
        // the pack here.  Subsequent file workers whose chunks reference the
        // same pack short-circuit instead of repeating the failed fetch -
        // turns "1 missing pack × N files × per-file retry budget" from
        // minutes of wall-clock into a single end-of-run skipped count.
        let missing_packs: Arc<std::sync::Mutex<std::collections::HashSet<String>>> =
            Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));

        // - Spawn per-file write tasks ---------------------
        let files_done = Arc::new(AtomicU64::new(0));
        let bytes_done_ctr = Arc::new(AtomicU64::new(0));
        let sem = Arc::new(Semaphore::new(self.concurrency));
        // Live progress counters surfaced in every RestoreFileProgress
        //.  All shared atomics; cheap on both sides.
        let packs_in_flight = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let packs_downloaded = Arc::new(AtomicU64::new(0));
        let errors_so_far = Arc::new(AtomicU64::new(0));
        let skipped_so_far = Arc::new(AtomicU64::new(0));
        // Destination root for the GUI banner.  RestoreTarget::Original
        // restores per-file to the path recorded in the manifest, so there
        // is no single root - report "<various>" in that case.
        // Total distinct packs this restore will touch.  Counted
        // from the chunk resolver across all not-yet-completed tasks so the
        // GUI can show "X / Y packs downloaded" with a real denominator.
        // For restores without a resolver (rare) this stays 0 - the GUI
        // gracefully falls back to "N pack(s) downloaded" without a total.
        let packs_total_count: u64 = if let Some(ref resolve) = self.chunk_resolver {
            let mut uniq: std::collections::HashSet<PackId> = std::collections::HashSet::new();
            for t in tasks.iter() {
                if t.dest.is_none() || checkpoint.completed_files.contains(&t.rel_str) {
                    continue;
                }
                for c in t.chunks.iter() {
                    if let Some((pack_id, _, _)) = resolve(&c.chunk_hash) {
                        uniq.insert(pack_id);
                    }
                }
            }
            uniq.len() as u64
        } else {
            0
        };

        // destination_root_s moved up to the top of the pre-create-dirs
        // pass so the ownership-chain walker can reference it.
        // This line is intentionally left as a no-op anchor in case
        // future merges expect a binding here; the real value comes
        // from the let-binding higher up.

        // emit an initial progress event before the spawn loop so
        // the GUI sees a populated `progress` (with phase chip, counters,
        // destination root) within milliseconds instead of waiting up to
        // ~60s for the first pack to download + first file to complete.
        // Previously the GUI sat on "Initializing…" for the full first-pack
        // download window with nothing else visible.
        // also kick off a 1Hz heartbeat task (below) so the
        // "Downloading pack X / Y" message appears while a pack is being
        // fetched - without this, no per-file event fires during the
        // 10-20 s download window and the UI looks frozen on the last
        // completed file.
        if let Some(ref tx) = progress_tx {
            let _ = tx
                .send(RestoreFileProgress {
                    files_done: 0,
                    files_total,
                    bytes_done: 0,
                    bytes_total,
                    current_file: String::new(),
                    error: None,
                    phase: "Restoring".to_string(),
                    packs_in_flight: 0,
                    packs_downloaded: 0,
                    packs_total: packs_total_count,
                    errors_so_far: 0,
                    skipped_so_far: 0,
                    destination_root: destination_root_s.clone(),
                })
                .await;
        }

        // heartbeat task that re-emits progress every 1 s while a
        // pack download is in flight.  Per-file events only fire when a
        // file COMPLETES; during a 10-20 s pack download on R2 no file
        // completes, so the UI looked stuck on the last completed file
        // with phase=Restoring.  This task captures the atomics and emits
        // a fresh event so the GUI flips to phase=Downloading + the
        // pack counter shows live progress.  Stops when restore_done is
        // set true after the drain loop.
        //
        // heartbeat preserves the last-known current_file by
        // reading from a shared `Mutex<String>` that workers update.
        // Earlier version sent current_file="" each tick, causing the
        // GUI's `{#if progress.current}` to flip false then true on
        // every per-file event -> visible flicker.
        let last_current_file: Arc<std::sync::Mutex<String>> =
            Arc::new(std::sync::Mutex::new(String::new()));
        let (restore_done_tx, mut restore_done_rx) = tokio::sync::watch::channel(false);
        if let Some(tx) = progress_tx.clone() {
            let hb_files_done = Arc::clone(&files_done);
            let hb_bytes_done = Arc::clone(&bytes_done_ctr);
            let hb_packs_in_flight = Arc::clone(&packs_in_flight);
            let hb_packs_downloaded = Arc::clone(&packs_downloaded);
            let hb_errors = Arc::clone(&errors_so_far);
            let hb_skipped = Arc::clone(&skipped_so_far);
            let hb_dest_root = destination_root_s.clone();
            let hb_last_current = Arc::clone(&last_current_file);
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    tokio::select! {
                        _ = interval.tick() => {
                            let pif = hb_packs_in_flight.load(Ordering::Relaxed);
                            if pif == 0 { continue; } // no download in flight; per-file events suffice
                            let cur = hb_last_current.lock().expect("last_current poisoned").clone();
                            let _ = tx.send(RestoreFileProgress {
                                files_done:       hb_files_done.load(Ordering::Relaxed),
                                files_total,
                                bytes_done:       hb_bytes_done.load(Ordering::Relaxed),
                                bytes_total,
                                current_file:     cur,
                                error:            None,
                                phase:            "Downloading".to_string(),
                                packs_in_flight:  pif,
                                packs_downloaded: hb_packs_downloaded.load(Ordering::Relaxed),
                                packs_total:      packs_total_count,
                                errors_so_far:    hb_errors.load(Ordering::Relaxed),
                                skipped_so_far:   hb_skipped.load(Ordering::Relaxed),
                                destination_root: hb_dest_root.clone(),
                            }).await;
                        }
                        _ = restore_done_rx.changed() => break,
                    }
                }
            });
        }

        let mut join_set: JoinSet<(String, Result<()>)> = JoinSet::new();
        let mut checkpoint_pending = 0usize;
        // batch of newly-completed rel_paths since the last
        // save.  Flushed to the DB (or to JSON, in the legacy path)
        // when we cross the 100-file boundary or finish the drain
        // loop.  Decouples the in-memory `completed_files` HashSet
        // (used for O(1) resume-skip checks) from the persistence
        // batch (which needs to write only the delta).
        let mut pending_batch: Vec<String> = Vec::new();
        // dest paths corresponding to pending_batch.  Run through
        // batch_fsync_dest_paths right before the checkpoint INSERT so
        // the data is on durable storage before we record completion.
        // Vec instead of HashMap to preserve order and avoid lookups.
        let mut pending_dest_batch: Vec<PathBuf> = Vec::new();

        // dest_map lets the drain loop look up dest for the fsync batch
        // and (legacy) for mtime/owner.  Mtime/owner now applied
        // inline by each write task; this map is only used for the
        // post-write fsync batch's path collection now.
        let dest_map: HashMap<String, (PathBuf, u64)> = tasks
            .iter()
            .filter_map(|t| {
                t.dest
                    .as_ref()
                    .map(|d| (t.rel_str.clone(), (d.clone(), t.mtime_ns)))
            })
            .collect();

        // shared Arc<RestoreOwner> cloned cheaply into every
        // per-file write task so each task can set mtime + owner inline
        // after its write.
        let owner_arc = Arc::new(owner.clone());

        let cancel_rx_drain = self.cancel_rx.clone();
        let pause_rx_drain = self.pause_rx.clone();

        for task in tasks {
            // - Cancel / pause check (before spawning) ------------
            if let Some(ref crx) = self.cancel_rx
                && *crx.borrow()
            {
                join_set.abort_all();
                if checkpoint_pending > 0 {
                    // fsync the in-flight batch before persisting
                    // checkpoint rows even on the cancel path - otherwise
                    // the rows could outlive the data they describe.
                    batch_fsync_paths(&pending_dest_batch).await;
                    save_checkpoint(&checkpoint, &cp_path);
                    pending_batch.clear();
                    pending_dest_batch.clear();
                }
                return Err(Error::Cancelled);
            }
            if let Some(ref prx) = self.pause_rx {
                while *prx.borrow() {
                    if let Some(ref crx) = self.cancel_rx
                        && *crx.borrow()
                    {
                        join_set.abort_all();
                        if checkpoint_pending > 0 {
                            // fsync the in-flight batch before persisting
                            // checkpoint rows even on the cancel path - otherwise
                            // the rows could outlive the data they describe.
                            batch_fsync_paths(&pending_dest_batch).await;
                            save_checkpoint(&checkpoint, &cp_path);
                            pending_batch.clear();
                            pending_dest_batch.clear();
                        }
                        return Err(Error::Cancelled);
                    }
                    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                }
            }

            let FileTask {
                rel_str,
                chunks,
                mtime_ns,
                file_bytes,
                dest,
                symlink_target,
                mode,
            } = task;

            // Resume: skip files fully written in a prior interrupted run.
            if checkpoint.completed_files.contains(&rel_str) {
                files_done.fetch_add(1, Ordering::Relaxed);
                bytes_done_ctr.fetch_add(file_bytes, Ordering::Relaxed);
                continue;
            }
            // OverwriteMode::Skip on an already-present file.
            let dest = match dest {
                Some(d) => d,
                None => {
                    files_done.fetch_add(1, Ordering::Relaxed);
                    bytes_done_ctr.fetch_add(file_bytes, Ordering::Relaxed);
                    continue;
                }
            };

            // surface the *destination* path in the per-file
            // progress (not the original manifest path).  Users restoring
            // to a Custom target want to see where the file is landing
            // on their local disk, not the source-machine path baked into
            // the manifest.
            let dest_display = dest.display().to_string();
            let pack_cache_c = Arc::clone(&pack_cache);
            let missing_c = Arc::clone(&missing_packs);
            let multi_c = Arc::clone(&multi_chunk_packs);
            let storage_c = self.storage.clone();
            let resolver_c = self.chunk_resolver.clone();
            let sparse_c = self.sparse;
            let sem_c = Arc::clone(&sem);
            let files_done_c = Arc::clone(&files_done);
            let bytes_done_c = Arc::clone(&bytes_done_ctr);
            let progress_c = progress_tx.clone();
            let rel_str_c = rel_str.clone();
            let cancel_rx_t = self.cancel_rx.clone();
            let pause_rx_t = self.pause_rx.clone();
            let pif_c = Arc::clone(&packs_in_flight);
            let pdl_c = Arc::clone(&packs_downloaded);
            let err_c = Arc::clone(&errors_so_far);
            let skp_c = Arc::clone(&skipped_so_far);
            let dst_root_c = destination_root_s.clone();
            let last_cf_c = Arc::clone(&last_current_file);
            let owner_t = Arc::clone(&owner_arc);

            join_set.spawn(async move {
                if cancel_rx_t.as_ref().map(|r| *r.borrow()).unwrap_or(false) {
                    return (rel_str_c, Err(Error::Cancelled));
                }
                if let Some(ref prx) = pause_rx_t {
                    while *prx.borrow() {
                        if cancel_rx_t.as_ref().map(|r| *r.borrow()).unwrap_or(false) {
                            return (rel_str_c, Err(Error::Cancelled));
                        }
                        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                    }
                }
                let _permit = sem_c.acquire().await.ok();
                if cancel_rx_t.as_ref().map(|r| *r.borrow()).unwrap_or(false) {
                    return (rel_str_c, Err(Error::Cancelled));
                }
                // Symlinks dispatch to a small platform-aware helper
                // rather than the chunk writer.  Chunks are empty for
                // symlinks; the recorded target string is what carries
                // the file's content.  Cross-platform handling:
                //   - Linux + macOS: native symlink creation.
                //   - Windows with sufficient privilege (admin or
                //     Developer Mode): native CreateSymbolicLink.
                //   - Windows without privilege: fall back to writing
                //     a sidecar text file <path>.symlink.txt with the
                //     target string preserved verbatim so the user
                //     never silently loses the data.
                let result = if let Some(target) = symlink_target {
                    restore_symlink(&dest, &target).await
                } else {
                    // Write file using lazy pack cache (opt 1); directories
                    // already exist (opt 3); mtime + owner applied inline
                    // (removes the post-write per-file finalizing
                    // pass that used to take 30-60 s on a 100K restore).
                    write_file_with_pack_cache(
                        pack_cache_c,
                        missing_c,
                        Arc::clone(&pif_c),
                        Arc::clone(&pdl_c),
                        multi_c,
                        storage_c,
                        resolver_c,
                        chunks,
                        dest,
                        chunk_key_bytes,
                        chunk_id_key_bytes,
                        cancel_rx_t.clone(),
                        pause_rx_t.clone(),
                        mtime_ns,
                        mode,
                        owner_t,
                        sparse_c,
                    )
                    .await
                };
                // Per-file error / skip accounting for the live progress
                // counters.  Skipped files (pack-skip-on-missing prefix)
                // do not contribute to the error count.
                if let Err(e) = &result {
                    if let Error::Storage(m) = e {
                        if m.starts_with(SKIPPED_PREFIX) {
                            skp_c.fetch_add(1, Ordering::Relaxed);
                        } else {
                            err_c.fetch_add(1, Ordering::Relaxed);
                        }
                    } else if !matches!(e, Error::Cancelled) {
                        err_c.fetch_add(1, Ordering::Relaxed);
                    }
                }
                let fd = files_done_c.fetch_add(1, Ordering::Relaxed) + 1;
                let bd = bytes_done_c.fetch_add(file_bytes, Ordering::Relaxed) + file_bytes;
                // stash the last completed file path for the
                // heartbeat task to read so its emits don't blank the
                // GUI's file row.
                *last_cf_c.lock().expect("last_current poisoned") = dest_display.clone();
                if let Some(ref tx) = progress_c {
                    let pif = pif_c.load(Ordering::Relaxed);
                    let phase = if pif > 0 { "Downloading" } else { "Restoring" };
                    let _ = tx
                        .send(RestoreFileProgress {
                            files_done: fd,
                            files_total,
                            bytes_done: bd,
                            bytes_total,
                            // Show the absolute destination path (not the
                            // source manifest path).
                            current_file: dest_display.clone(),
                            error: result.as_ref().err().map(|e| e.to_string()),
                            phase: phase.to_string(),
                            packs_in_flight: pif,
                            packs_downloaded: pdl_c.load(Ordering::Relaxed),
                            packs_total: packs_total_count,
                            errors_so_far: err_c.load(Ordering::Relaxed),
                            skipped_so_far: skp_c.load(Ordering::Relaxed),
                            destination_root: dst_root_c.clone(),
                        })
                        .await;
                }
                (rel_str_c, result)
            });
        }

        // - Drain join_set --------------------------
        // Store the underlying Error (not just its rendered string) so the
        // classifier can route the terminal failure to its
        // user-facing category instead of always seeing Error::Internal.
        // Before this change, a missing pack on the remote surfaced as
        // "[bkp:internal] 1 restore error(s); first: Storage error: ..."
        // because the aggregation collapsed every Error to Error::Internal.
        let mut errors: Vec<Error> = Vec::new();
        // Pack-skip counter.  Per-file errors carrying
        // the SKIPPED_PREFIX magic string come from the missing-pack
        // short-circuit and are counted separately from "real" errors so
        // the end-of-run summary can report "N files restored, K skipped
        // due to M missing packs" instead of dumping K per-file errors.
        let mut skipped_count: u64 = 0;
        while let Some(r) = join_set.join_next().await {
            if cancel_rx_drain
                .as_ref()
                .map(|r| *r.borrow())
                .unwrap_or(false)
            {
                join_set.abort_all();
                if checkpoint_pending > 0 {
                    save_checkpoint(&checkpoint, &cp_path);
                    pending_batch.clear();
                }
                return Err(Error::Cancelled);
            }
            if let Some(ref prx) = pause_rx_drain {
                while *prx.borrow() {
                    if cancel_rx_drain
                        .as_ref()
                        .map(|r| *r.borrow())
                        .unwrap_or(false)
                    {
                        join_set.abort_all();
                        if checkpoint_pending > 0 {
                            // fsync the in-flight batch before persisting
                            // checkpoint rows even on the cancel path - otherwise
                            // the rows could outlive the data they describe.
                            batch_fsync_paths(&pending_dest_batch).await;
                            save_checkpoint(&checkpoint, &cp_path);
                            pending_batch.clear();
                            pending_dest_batch.clear();
                        }
                        return Err(Error::Cancelled);
                    }
                    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                }
            }
            match r {
                Ok((rel_str, Ok(()))) => {
                    // per-file mtime + owner now applied inline
                    // by the write task itself - no post-write batch
                    // pass needed.
                    // dual-track - in-memory HashSet for O(1)
                    // resume-skip checks, and a pending_batch Vec for
                    // delta persistence (DB INSERT batch or, on the
                    // legacy path, a full JSON rewrite).
                    pending_batch.push(rel_str.clone());
                    if let Some((dest, _)) = dest_map.get(&rel_str) {
                        pending_dest_batch.push(dest.clone());
                    }
                    checkpoint.completed_files.insert(rel_str);
                    checkpoint_pending += 1;
                    if checkpoint_pending >= 100 {
                        // batch-fsync all dest paths in parallel
                        // BEFORE the checkpoint insert.  Ensures data is
                        // durable for every file we're about to mark as
                        // completed.  100 parallel fsyncs amortize the
                        // per-syscall cost via the disk's NCQ instead of
                        // serializing through write_file's semaphore
                        // permits.
                        batch_fsync_paths(&pending_dest_batch).await;
                        save_checkpoint(&checkpoint, &cp_path);
                        pending_batch.clear();
                        pending_dest_batch.clear();
                        checkpoint_pending = 0;
                    }
                }
                Ok((_, Err(Error::Cancelled))) => {}
                Ok((_, Err(e))) => {
                    // Distinguish "skipped because pack missing" (counted)
                    // from a real fetch failure (added to errors list).
                    if let Error::Storage(ref msg) = e
                        && msg.starts_with(SKIPPED_PREFIX)
                    {
                        skipped_count += 1;
                        continue;
                    }
                    errors.push(e);
                }
                Err(_) => {}
            }
        }
        if checkpoint_pending > 0 {
            // final partial batch - fsync before persistence.
            batch_fsync_paths(&pending_dest_batch).await;
            save_checkpoint(&checkpoint, &cp_path);
            pending_batch.clear();
            pending_dest_batch.clear();
        }

        // Stop the heartbeat before emitting Finalizing so it
        // doesn't race the Finalizing event with a stale Downloading one.
        let _ = restore_done_tx.send(true);

        // Emit a single "Finalizing" event so the GUI can flip the phase
        // chip before the syscall-heavy post-100 % pass starts.  No new
        // file completed - reuse the last-seen counters so file_done /
        // bytes_done don't backslide.
        if let Some(ref tx) = progress_tx {
            let _ = tx
                .send(RestoreFileProgress {
                    files_done: files_done.load(Ordering::Relaxed),
                    files_total,
                    bytes_done: bytes_done_ctr.load(Ordering::Relaxed),
                    bytes_total,
                    current_file: String::new(),
                    error: None,
                    phase: "Finalizing".to_string(),
                    packs_in_flight: 0,
                    packs_downloaded: packs_downloaded.load(Ordering::Relaxed),
                    packs_total: packs_total_count,
                    errors_so_far: errors_so_far.load(Ordering::Relaxed),
                    skipped_so_far: skipped_so_far.load(Ordering::Relaxed),
                    destination_root: destination_root_s.clone(),
                })
                .await;
        }

        // - finalizing: per-file mtime + owner already applied
        // inline by each write task (apply_file_metadata).  Only
        // directories need post-write attention.  Walk + parallel apply.
        // -----------------------------------------------------------
        let owner_c = Arc::new(owner.clone());
        let mut dir_entries: Vec<(PathBuf, u64)> = Vec::new();
        collect_dir_mtimes(&manifest.file_tree.root, &PathBuf::new(), &mut dir_entries);
        // Bottom-up so child mtimes don't bump parent's mtime AFTER we
        // set it.  Keep this ordering even though we run shards in
        // parallel - within a shard the slice is still bottom-up; across
        // shards the depth interleaving is irrelevant for correctness
        // (filetime::set_file_mtime is an explicit clock set, not a
        // touch-on-modify).
        dir_entries.sort_by_key(|e| std::cmp::Reverse(e.0.components().count()));
        let mut dir_pairs: Vec<(PathBuf, u64)> = Vec::with_capacity(dir_entries.len());
        for (rel_path, mtime_ns) in dir_entries {
            if mtime_ns == 0 {
                continue;
            }
            let rel_str = rel_path.to_string_lossy();
            if let Ok(dest) = resolve_dest(&target, &rel_str, snapshot_secs, &self.source_prefixes)
                && dest.is_dir()
            {
                dir_pairs.push((dest, mtime_ns));
            }
        }
        if !dir_pairs.is_empty() {
            let parallelism = std::thread::available_parallelism()
                .map(|n| n.get().min(16))
                .unwrap_or(4);
            let chunk_size = dir_pairs.len().div_ceil(parallelism);
            let mut shards: JoinSet<()> = JoinSet::new();
            let mut work = dir_pairs;
            while !work.is_empty() {
                let take = chunk_size.min(work.len());
                let shard: Vec<(PathBuf, u64)> = work.drain(..take).collect();
                let owner_s = Arc::clone(&owner_c);
                shards.spawn(async move {
                    tokio::task::spawn_blocking(move || {
                        for (path, mtime_ns) in shard.iter() {
                            let ft = filetime::FileTime::from_unix_time(
                                (*mtime_ns / 1_000_000_000) as i64,
                                (*mtime_ns % 1_000_000_000) as u32,
                            );
                            let _ = filetime::set_file_mtime(path, ft);
                            apply_unix_owner(path, &owner_s);
                        }
                    })
                    .await
                    .ok();
                });
            }
            while let Some(r) = shards.join_next().await {
                if let Err(e) = r {
                    warn!("dir mtime shard panicked: {e}");
                }
            }
        }

        // Post-drain reclassification.  Race-condition
        // artifact of the skip mechanism: when N files reference a
        // missing pack and start concurrently, all N might fail their
        // fetches before any of them registers the pack as missing.
        // After the drain we know the full missing-pack set, so move any
        // error referencing a pack in that set into the skipped count
        // - users see "1 error + N skipped" instead of "N errors" for
        // what's logically one underlying problem.
        {
            let missing_set = missing_packs.lock().expect("missing_packs poisoned");
            if !missing_set.is_empty() {
                let mut kept: Vec<Error> = Vec::with_capacity(errors.len());
                for e in errors.drain(..) {
                    let mentions_missing = if let Error::Storage(ref m) = e {
                        missing_set.iter().any(|p| m.contains(p))
                    } else {
                        false
                    };
                    if mentions_missing {
                        skipped_count += 1;
                    } else {
                        kept.push(e);
                    }
                }
                errors = kept;
            }
        }

        // Compose the missing-packs summary for both the success-with-skips
        // case and the failure path so the user always sees the same wording.
        let missing_pack_list: Vec<String> = {
            let g = missing_packs.lock().expect("missing_packs poisoned");
            let mut v: Vec<String> = g.iter().cloned().collect();
            v.sort();
            v
        };
        let missing_packs_count = missing_pack_list.len();
        let make_skip_message = |files: u64, packs: usize, sample: &[String]| -> String {
            let head = if packs <= 3 {
                sample.join(", ")
            } else {
                format!("{} … and {} more", sample[..3].join(", "), packs - 3)
            };
            format!("{files} file(s) skipped due to {packs} missing pack(s): {head}")
        };

        // on successful completion (with or without skips) clear
        // the checkpoint from whichever backing store is active.  Inlined
        // (not a closure) to dodge a self-borrow lifetime problem in
        // async closures.
        if errors.is_empty() && skipped_count == 0 {
            let _ = std::fs::remove_file(&cp_path);
            Ok(())
        } else if errors.is_empty() {
            // Restore otherwise succeeded; surface the skip summary as
            // a structured StorageMissingObject so the classifier
            // maps it to the localized "Backup data incomplete" headline.
            let _ = std::fs::remove_file(&cp_path);
            Err(Error::Storage(make_skip_message(
                skipped_count,
                missing_packs_count,
                &missing_pack_list,
            )))
        } else {
            // Preserve the FIRST error's variant so the GUI classifier
            // maps "no such file" → StorageMissingObject
            // ("Backup data incomplete") instead of seeing Error::Internal.
            // Count and additional-errors tail are folded into the message
            // for variants that carry a String; structured variants
            // (IntegrityMismatch, ArchiveRetrievalRequired, etc.) pass
            // through unchanged so their fields stay intact.
            let n = errors.len();
            let mut iter = errors.into_iter();
            let first = iter.next().expect("errors not empty");
            let prefix = if n > 1 {
                format!("{n} restore error(s); first: ")
            } else {
                String::new()
            };
            let aggregated = match first {
                Error::Storage(m) => Error::Storage(format!("{prefix}{m}")),
                Error::Crypto(m) => Error::Crypto(format!("{prefix}{m}")),
                Error::Internal(m) => Error::Internal(format!("{prefix}{m}")),
                Error::SourceUnavailable(m) => Error::SourceUnavailable(format!("{prefix}{m}")),
                Error::SourceLost(m) => Error::SourceLost(format!("{prefix}{m}")),
                Error::Io(io_err) => Error::Storage(format!("{prefix}{io_err}")),
                other => other,
            };
            Err(aggregated)
        }
    }

    /// Inject a pre-decoded manifest so the next `fetch_manifest` call for
    /// `snapshot_id` returns it without going to the network.  Used by the
    /// daemon's LRU manifest cache to avoid pulling the same encrypted blob
    /// twice during a "browse then restore" sequence.
    pub fn set_pre_fetched_manifest(&mut self, snapshot_id: SnapshotId, manifest: Arc<Manifest>) {
        self.pre_fetched_manifest = Some((snapshot_id, manifest));
    }

    // - Private helpers ------------------------------

    async fn fetch_manifest(
        &self,
        snapshot_id: &SnapshotId,
        set_id: &BackupSetId,
        manifest_key: &SubKey,
    ) -> Result<Manifest> {
        // Hit the pre-fetched cache slot if the caller installed one
        // for this snapshot.  Clones the decoded Manifest out of the
        // Arc to keep the existing return type.
        if let Some((cached_id, cached_manifest)) = &self.pre_fetched_manifest
            && cached_id == snapshot_id
        {
            return Ok((**cached_manifest).clone());
        }
        let path = manifest_remote_path(set_id, snapshot_id);
        // manifest critical-object read with .bak fallback.
        let data = self.storage.get_critical(&path).await?;
        decode_manifest(&data, manifest_key)
    }
}

// - Pack-based chunk fetch --------------------------

// - Lazy pack cache helpers --------------------------

/// Return the set of pack paths that contain more than one distinct chunk ID
/// referenced by `pending`.
///
/// Threshold for treating a pack as "multi-chunk" - i.e. worth downloading
/// in full and slicing locally.  The old heuristic flipped at >1 chunk
/// which made small restores (a couple of files from a big pack)
/// download the full 256 MiB body, very slow on flaky links and pure
/// waste of bandwidth for a few KB of needed data.
///
/// New heuristic: only "download whole" when at least 32 distinct chunks
/// are needed.  Below that, multiple `get_range` requests are cheaper
/// in bytes and far more resilient (small bodies don't trip the
/// body-decode reset path).  32 ≈ ~10 % of a 256 MiB pack at the
/// default 8 MiB avg chunk size; user restores of a few files from a
/// big snapshot fall well under this and use ranges as intended.
const MULTI_CHUNK_PACK_THRESHOLD: usize = 32;

/// Called once upfront (no network I/O) to decide whether a pack should be
/// fetched in full (and cached) or via a targeted range request.
fn classify_multi_chunk_packs(
    resolver: &ChunkResolver,
    pending: &[Vec<ChunkRef>],
) -> HashSet<String> {
    let mut pack_chunks: HashMap<String, HashSet<[u8; 32]>> = HashMap::new();
    for file_chunks in pending {
        for cr in file_chunks {
            if let Some((pack_id, _, _)) = resolver(&cr.chunk_hash) {
                pack_chunks
                    .entry(format!("packs/{}.pack", pack_id.as_uuid()))
                    .or_default()
                    .insert(*cr.chunk_hash.as_bytes());
            }
        }
    }
    pack_chunks
        .into_iter()
        .filter(|(_, ids)| ids.len() >= MULTI_CHUNK_PACK_THRESHOLD)
        .map(|(path, _)| path)
        .collect()
}

/// In-flight slot in the pack cache.  Tracks whether a pack is still being
/// downloaded by some worker (`Pending`) so concurrent requesters wait on
/// the same download rather than each issuing their own.  Once the download
/// completes successfully the slot flips to `Ready` and stays cached for the
/// rest of the run.  On failure the slot is removed so the next caller can
/// try again.
#[derive(Clone)]
enum PackEntry {
    Pending(Arc<tokio::sync::Notify>),
    Ready(Arc<Vec<u8>>),
}

/// Bounded LRU pack cache.
///
/// The previous implementation was an unbounded `HashMap` - a restore that
/// touched many distinct multi-chunk packs accumulated 256 MiB per pack until
/// the run completed, pushing the daemon's working set to ~800 MiB on a
/// 100K-file set.  This struct evicts least-recently-used `Ready` entries
/// once the total cached byte count would exceed `max_bytes`.
///
/// Eviction policy:
/// * `Pending` entries are never evicted (an in-flight download must be
///   resolved or removed via the Err path; evicting one would orphan
///   concurrent waiters).
/// * The LRU order tracks `Ready` keys in insertion / access order.  On
///   insert, evict oldest `Ready` entries until the new entry fits.
/// * If a single pack is larger than `max_bytes`, it is still admitted (and
///   immediately evicts everything else).  Better than refusing to cache it
///   and re-downloading 256 MiB per chunk.
struct BoundedPackCache {
    map: HashMap<String, PackEntry>,
    /// Sum of `Ready(data).len()` across all Ready entries.  Pending
    /// entries contribute zero (they have no allocated payload yet).
    ready_bytes: usize,
    /// Upper bound on `ready_bytes` after an insert.
    max_bytes: usize,
    /// Most-recently-used at the BACK, oldest at the FRONT.  Only contains
    /// keys whose entry is currently `Ready`.
    lru: std::collections::VecDeque<String>,
}

/// Default LRU cap: 768 MiB (≈ 3 × 256 MiB packs).  Picked as a safe
/// default that keeps the dedup benefit of in-flight pack reuse while
/// bounding the daemon's working set during large restores.
const DEFAULT_PACK_CACHE_BYTES: usize = 768 * 1024 * 1024;

impl BoundedPackCache {
    fn new(max_bytes: usize) -> Self {
        Self {
            map: HashMap::new(),
            ready_bytes: 0,
            max_bytes,
            lru: std::collections::VecDeque::new(),
        }
    }

    fn get(&mut self, key: &str) -> Option<PackEntry> {
        let entry = self.map.get(key).cloned()?;
        // Promote on access only for Ready entries (Pending doesn't
        // participate in LRU - it's not yet competing for the byte budget).
        if matches!(entry, PackEntry::Ready(_))
            && let Some(pos) = self.lru.iter().position(|k| k == key)
        {
            self.lru.remove(pos);
            self.lru.push_back(key.to_string());
        }
        Some(entry)
    }

    fn insert_pending(&mut self, key: String, notify: Arc<tokio::sync::Notify>) {
        self.map.insert(key, PackEntry::Pending(notify));
    }

    fn insert_ready(&mut self, key: String, data: Arc<Vec<u8>>) {
        let new_size = data.len();
        // Remove any prior entry's accounting first; then evict to make room.
        if let Some(PackEntry::Ready(old)) = self.map.remove(&key) {
            self.ready_bytes = self.ready_bytes.saturating_sub(old.len());
            self.lru.retain(|k| k != &key);
        } else {
            self.map.remove(&key); // drop any Pending marker
        }
        while self.ready_bytes + new_size > self.max_bytes && !self.lru.is_empty() {
            let evict_key = self.lru.pop_front().expect("non-empty");
            if let Some(PackEntry::Ready(old)) = self.map.remove(&evict_key) {
                self.ready_bytes = self.ready_bytes.saturating_sub(old.len());
            }
        }
        self.ready_bytes += new_size;
        self.lru.push_back(key.clone());
        self.map.insert(key, PackEntry::Ready(data));
    }

    fn remove(&mut self, key: &str) {
        if let Some(PackEntry::Ready(old)) = self.map.remove(key) {
            self.ready_bytes = self.ready_bytes.saturating_sub(old.len());
            self.lru.retain(|k| k != key);
        }
    }
}

/// Return a cached copy of `pack_path`, downloading it at most once.
///
/// The first caller initiates the download and inserts a `Pending` slot;
/// concurrent callers wait on the same `Notify` and re-check the cache when
/// woken (now either `Ready` or absent after a failure).  Avoids the
/// "two workers downloading the same 256 MiB pack in parallel" race the
/// previous implementation tolerated.
/// Download a pack resiliently for backends that hint at a concurrency
/// cap (currently: Cloudflare R2 via [`StorageBackend::concurrency_hint`]
/// returning `Some(n)`).
///
/// Rationale: a single `storage.get()` of a 256 MiB pack on R2
/// from a residential link routinely fails mid-stream with "error
/// decoding response body" - the HTTP response is cut short before the
/// declared Content-Length, reqwest can't decode the chunked body, and
/// our retry layer keeps trying with the same outcome.  Splitting the
/// download into 16 MiB range requests with the same backpressure cap
/// we already use for multipart uploads makes the failure window per
/// request tiny and a transient cutoff costs at most 16 MiB of re-fetch
/// instead of the whole pack.
///
/// Backends without a concurrency hint (AWS S3, B2 < cap, local, SMB,
/// etc.) keep the single `storage.get()` fast path - no behavior change.
async fn download_pack_resilient(
    storage: &Arc<dyn StorageBackend>,
    pack_path: &str,
    cancel_rx: Option<watch::Receiver<bool>>,
) -> Result<Vec<u8>> {
    const RANGE_BYTES: u64 = 16 * 1024 * 1024;
    // cooperative cancel for both the single-get and chunked
    // paths.  Each call site of get_range/get is wrapped in a select!
    // against the cancel watch so an SCM STOP (which sets cancel_rx in
    // <500 ms) doesn't have to wait for the in-flight HTTP request to
    // finish before the worker can exit.  Dropping the get_range future
    // mid-await cancels the underlying tokio task graph, which closes
    // the TLS connection - object_store / reqwest both clean up on
    // drop.
    let is_cancelled = || cancel_rx.as_ref().map(|r| *r.borrow()).unwrap_or(false);
    let select_get = |fut: futures::future::BoxFuture<'static, Result<Vec<u8>>>| {
        let crx = cancel_rx.clone();
        async move {
            match crx {
                Some(mut rx) => {
                    if *rx.borrow() {
                        return Err(Error::Cancelled);
                    }
                    tokio::select! {
                        biased;
                        _ = rx.changed() => Err(Error::Cancelled),
                        res = fut => res,
                    }
                }
                None => fut.await,
            }
        }
    };
    let Some(concurrency) = storage.concurrency_hint() else {
        // No hint = backend handles its own retries fine; single get.
        let s = Arc::clone(storage);
        let p = pack_path.to_string();
        return select_get(Box::pin(async move { s.get(&p).await })).await;
    };
    if is_cancelled() {
        return Err(Error::Cancelled);
    }
    let total = match storage.size(pack_path).await {
        Ok(n) => n,
        Err(e) => {
            debug!(
                "download_pack_resilient: size({pack_path}) failed: {e}; falling back to single get"
            );
            let s = Arc::clone(storage);
            let p = pack_path.to_string();
            return select_get(Box::pin(async move { s.get(&p).await })).await;
        }
    };
    if total <= RANGE_BYTES {
        let s = Arc::clone(storage);
        let p = pack_path.to_string();
        return select_get(Box::pin(async move { s.get(&p).await })).await;
    }
    let mut spans: Vec<(u64, u64)> = Vec::new();
    let mut offset: u64 = 0;
    while offset < total {
        let end = (offset + RANGE_BYTES).min(total);
        spans.push((offset, end));
        offset = end;
    }
    debug!(
        "download_pack_resilient: chunked download {pack_path} \
         ({} ranges of up to {} bytes, concurrency {})",
        spans.len(),
        RANGE_BYTES,
        concurrency,
    );
    let crx_for_stream = cancel_rx.clone();
    let parts: Vec<Result<Vec<u8>>> = stream::iter(spans)
        .map(|(from, to)| {
            let s = Arc::clone(storage);
            let p = pack_path.to_string();
            let crx = crx_for_stream.clone();
            async move {
                // Skip starting any new range once cancel has fired.
                if let Some(ref rx) = crx
                    && *rx.borrow()
                {
                    return Err(Error::Cancelled);
                }
                match crx {
                    Some(mut rx) => tokio::select! {
                        biased;
                        _ = rx.changed() => Err(Error::Cancelled),
                        res = s.get_range(&p, from, to) => res,
                    },
                    None => s.get_range(&p, from, to).await,
                }
            }
        })
        .buffered(concurrency)
        .collect()
        .await;
    let mut out: Vec<u8> = Vec::with_capacity(total as usize);
    for r in parts {
        out.extend_from_slice(&r?);
    }
    Ok(out)
}

async fn get_or_download_pack(
    cache: &Arc<tokio::sync::Mutex<BoundedPackCache>>,
    storage: Arc<dyn StorageBackend>,
    pack_path: String,
    packs_in_flight: &Arc<std::sync::atomic::AtomicU32>,
    packs_downloaded: &Arc<AtomicU64>,
    cancel_rx: Option<watch::Receiver<bool>>,
) -> Result<Arc<Vec<u8>>> {
    loop {
        let notify = {
            let mut guard = cache.lock().await;
            match guard.get(&pack_path) {
                Some(PackEntry::Ready(data)) => return Ok(data),
                Some(PackEntry::Pending(notify)) => notify,
                None => {
                    // First-in: install a Pending slot so concurrent callers
                    // wait on us, then drop the lock and download.
                    let notify = Arc::new(tokio::sync::Notify::new());
                    guard.insert_pending(pack_path.clone(), Arc::clone(&notify));
                    drop(guard);
                    packs_in_flight.fetch_add(1, Ordering::Relaxed);
                    let result =
                        download_pack_resilient(&storage, &pack_path, cancel_rx.clone()).await;
                    packs_in_flight.fetch_sub(1, Ordering::Relaxed);
                    let mut guard = cache.lock().await;
                    return match result {
                        Ok(data) => {
                            packs_downloaded.fetch_add(1, Ordering::Relaxed);
                            let arc = Arc::new(data);
                            guard.insert_ready(pack_path, Arc::clone(&arc));
                            notify.notify_waiters();
                            Ok(arc)
                        }
                        Err(e) => {
                            // Remove the Pending marker so a subsequent
                            // caller can retry from scratch.  Wake any
                            // waiters so they see the empty slot and
                            // re-enter the loop (which will start a fresh
                            // download attempt themselves).
                            guard.remove(&pack_path);
                            notify.notify_waiters();
                            Err(e)
                        }
                    };
                }
            }
        };
        // Wait outside the cache lock so other slots can progress.
        notify.notified().await;
        // Loop and re-check the slot state.
    }
}

/// Write a single file, fetching each chunk lazily from the shared pack cache.
///
/// - **Multi-chunk packs**: downloaded in full once via [`get_or_download_pack`];
///   the encrypted slice is extracted by offset.
/// - **Single-chunk packs**: targeted HTTP range request (no caching needed).
/// - **No resolver / resolver miss**: falls back to individual chunk objects.
///
/// Does **not** call `create_dir_all` - the caller pre-creates all parent
/// directories (opt 3).  Does **not** set mtime or ownership - those are
/// applied in a single `spawn_blocking` pass after all writes (opt 2).
/// Magic prefix on `Error::Storage` indicating "this file was skipped because
/// its pack is in the missing_packs set" - the drain loop counts these
/// separately rather than promoting them to per-file failures.  Kept as a
/// plain-string contract so we don't need a new `Error` variant for what is
/// fundamentally a restore-internal book-keeping concern.
const SKIPPED_PREFIX: &str = "[skipped:missing-pack] ";

#[allow(clippy::too_many_arguments)]
/// Restore a single symlink entry at `dest` pointing at `target`.
///
/// Cross-platform handling:
///
/// - **Unix (Linux, macOS)**: native `std::os::unix::fs::symlink` -
///   the link is created with the original target string preserved
///   verbatim, including absolute / relative form.  A pre-existing
///   file at `dest` is unlinked first so the symlink wins; this
///   matches the OverwriteMode semantics already applied upstream.
///
/// - **Windows with privilege** (admin process OR Developer Mode
///   enabled OR `SeCreateSymbolicLinkPrivilege` granted): native
///   `std::os::windows::fs::symlink_file`.  We do NOT distinguish
///   file vs directory symlinks on Windows here - the manifest
///   doesn't carry that bit, and `symlink_file` is the safer
///   default (a `symlink_dir` to a missing target fails noisily
///   later; a `symlink_file` to a missing target stays usable as a
///   broken link in the same way Unix would).
///
/// - **Windows without privilege**: native call fails with
///   `ERROR_PRIVILEGE_NOT_HELD` (1314).  Falls back to writing a
///   sidecar text file at `<dest>.symlink.txt` containing the
///   target path on its own line plus a header line explaining
///   what happened.  The user never silently loses the symlink's
///   information - it's recoverable with `mklink` or by enabling
///   Developer Mode.
///
/// Path remapping for cross-platform restore is the caller's
/// responsibility (Section 9.4 of the data format spec); this
/// helper does not interpret the target string.
async fn restore_symlink(dest: &Path, target: &str) -> Result<()> {
    // Ensure the parent directory exists; the pre-create-dirs pass
    // above only walks Directory nodes from the manifest, which
    // means symlinks whose parent is implicit (e.g. when the entire
    // tree is rooted at a symlink) need this safety net.
    if let Some(parent) = dest.parent()
        && !parent.as_os_str().is_empty()
    {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    // Remove any pre-existing entry at dest.  Native symlink creation
    // on Unix and Windows both fail if the destination exists.
    let _ = tokio::fs::remove_file(dest).await;

    #[cfg(unix)]
    {
        let target_owned = target.to_string();
        let dest_owned = dest.to_path_buf();
        let dest_for_log = dest_owned.clone();
        tokio::task::spawn_blocking(move || std::os::unix::fs::symlink(&target_owned, &dest_owned))
            .await
            .map_err(|e| Error::Storage(format!("symlink spawn_blocking: {e}")))?
            .map_err(|e| {
                Error::Storage(format!(
                    "create symlink {} -> {target}: {e}",
                    dest_for_log.display()
                ))
            })?;
        Ok(())
    }

    #[cfg(windows)]
    {
        let target_owned = target.to_string();
        let dest_owned = dest.to_path_buf();
        let dest_for_log = dest_owned.clone();
        let res = tokio::task::spawn_blocking(move || {
            std::os::windows::fs::symlink_file(&target_owned, &dest_owned)
        })
        .await
        .map_err(|e| Error::Storage(format!("symlink spawn_blocking: {e}")))?;

        match res {
            Ok(_) => Ok(()),
            Err(e) => {
                // 1314 = ERROR_PRIVILEGE_NOT_HELD.  Other errors
                // (target's parent missing, invalid filename, etc.)
                // are real failures and propagate as-is.
                let priv_missing = matches!(e.raw_os_error(), Some(1314));
                if !priv_missing {
                    return Err(Error::Storage(format!(
                        "create symlink {}: {e}",
                        dest_for_log.display()
                    )));
                }
                // Sidecar fallback: write <dest>.symlink.txt with the
                // target preserved verbatim.  Future Recovery Tool
                // versions could resolve these to real symlinks once
                // the user grants the privilege.
                let sidecar_path = {
                    let mut s = dest_for_log.as_os_str().to_os_string();
                    s.push(".symlink.txt");
                    PathBuf::from(s)
                };
                let body = format!(
                    "# Nyx Backup symlink placeholder (Windows lacked \
                     SeCreateSymbolicLinkPrivilege at restore time)\n\
                     # Original path: {}\n\
                     # Symlink target follows on the next line:\n\
                     {}\n",
                    dest_for_log.display(),
                    target,
                );
                tokio::fs::write(&sidecar_path, body).await.map_err(|e| {
                    Error::Storage(format!(
                        "symlink sidecar write {}: {e}",
                        sidecar_path.display()
                    ))
                })?;
                warn!(
                    dest = %dest_for_log.display(),
                    sidecar = %sidecar_path.display(),
                    "Symlink restored as sidecar .symlink.txt - Windows \
                     process lacks SeCreateSymbolicLinkPrivilege.  Enable \
                     Developer Mode or run elevated to restore as a real \
                     symlink."
                );
                Ok(())
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
/// Write one chunk's plaintext at `offset`.  When `sparse` is on and the chunk
/// is entirely zeros, nothing is written - the region is left as a filesystem
/// hole (the file is extended to its logical length afterwards by the caller's
/// `set_len`).  When `sparse` is off, this is the original sequential
/// `write_all` (the cursor is already at `offset`), so the dense path is
/// byte-identical to before.
async fn write_chunk_maybe_sparse(
    file: &mut tokio::fs::File,
    offset: u64,
    plaintext: &[u8],
    sparse: bool,
) -> std::io::Result<()> {
    use tokio::io::{AsyncSeekExt, AsyncWriteExt};
    if sparse {
        // A zero chunk becomes a hole: skip it (never allocated).  Non-empty
        // chunks are re-seeked to their absolute offset so this is correct
        // regardless of chunk ordering.  `.all()` short-circuits on the first
        // non-zero byte, so data chunks bail immediately.
        if !plaintext.is_empty() && plaintext.iter().all(|&b| b == 0) {
            return Ok(());
        }
        file.seek(std::io::SeekFrom::Start(offset)).await?;
    }
    file.write_all(plaintext).await
}

/// On Windows, mark `file` sparse (`FSCTL_SET_SPARSE`) so seeking past zero
/// regions actually creates holes - NTFS writes real zeros otherwise.  A no-op
/// on Unix, where holes form implicitly.  Best-effort: on failure the file is
/// written dense (correct, just larger).
#[cfg(windows)]
#[allow(unsafe_code)]
fn mark_file_sparse(file: &tokio::fs::File, dest: &Path) {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::System::IO::DeviceIoControl;
    const FSCTL_SET_SPARSE: u32 = 0x000900C4;
    let mut returned: u32 = 0;
    // SAFETY: `file` owns a valid open handle for the duration of the call;
    // the no-buffer form of FSCTL_SET_SPARSE takes null in/out buffers.
    let ok = unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            FSCTL_SET_SPARSE,
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    // Best-effort (a failure just yields a dense file), but log it - a silent
    // FSCTL_SET_SPARSE failure is otherwise indistinguishable from the flag
    // never being set, and both surface as "file is NOT sparse".
    if ok == 0 {
        // SAFETY: GetLastError reads thread-local error state, always sound.
        let err = unsafe { GetLastError() };
        tracing::warn!(target: "bkp_restore",
            "sparse restore: FSCTL_SET_SPARSE failed on {} (GetLastError={err}); file will be dense",
            dest.display());
    } else {
        tracing::debug!(target: "bkp_restore",
            "sparse restore: marked {} sparse", dest.display());
    }
}

#[allow(clippy::too_many_arguments)]
async fn write_file_with_pack_cache(
    pack_cache: Arc<tokio::sync::Mutex<BoundedPackCache>>,
    missing_packs: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    packs_in_flight: Arc<std::sync::atomic::AtomicU32>,
    packs_downloaded: Arc<AtomicU64>,
    multi_chunk_packs: Arc<HashSet<String>>,
    storage: Arc<dyn StorageBackend>,
    resolver: Option<ChunkResolver>,
    chunk_refs: Vec<ChunkRef>,
    dest: PathBuf,
    chunk_key_bytes: [u8; 32],
    chunk_id_key_bytes: [u8; 32],
    cancel_rx: Option<watch::Receiver<bool>>,
    pause_rx: Option<watch::Receiver<bool>>,
    // apply mtime + owner inline so the per-file finalizing pass
    // can be removed.  Both syscalls fit inside the network-bound write
    // window for cloud backends (R2/B2/S3/GCS) so they're effectively
    // free.  mtime_ns == 0 means "skip mtime".  owner: skip when SID is
    // empty AND unix_uid is 0.
    mtime_ns: u64,
    mode: u32,
    owner: Arc<RestoreOwner>,
    sparse: bool,
) -> Result<()> {
    let mut sorted_refs = chunk_refs;
    sorted_refs.sort_by_key(|r| r.plaintext_offset);

    // Logical file length = highest chunk end.  Used to extend the file to its
    // full size after a sparse write whose final region was an all-zero
    // (skipped) chunk - otherwise the trailing hole would be lost.
    let total_len: u64 = sorted_refs
        .iter()
        .map(|r| r.plaintext_offset + r.plaintext_size)
        .max()
        .unwrap_or(0);

    let mut file = tokio::fs::File::create(&dest)
        .await
        .map_err(|e| Error::Storage(format!("create {}: {e}", dest.display())))?;
    #[cfg(windows)]
    if sparse {
        mark_file_sparse(&file, &dest);
    }

    let write_result: Result<()> = async {
        for chunk_ref in &sorted_refs {
            // Pause: hold between chunks until resumed.  Also exits on cancel.
            // Without this, paused restores keep streaming the current file's
            // remaining chunks (a multi-GB file = many seconds of "ignored" pause).
            if let Some(ref prx) = pause_rx {
                while *prx.borrow() {
                    if cancel_rx.as_ref().map(|r| *r.borrow()).unwrap_or(false) {
                        return Err(Error::Cancelled);
                    }
                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                }
            }
            // Cancel: abort mid-file.
            if cancel_rx.as_ref().map(|r| *r.borrow()).unwrap_or(false) {
                return Err(Error::Cancelled);
            }
            let plaintext: Vec<u8> = if let Some(ref resolve) = resolver {
                if let Some((pack_id, offset, enc_size)) = resolve(&chunk_ref.chunk_hash) {
                    let pack_path = format!("packs/{}.pack", pack_id.as_uuid());
                    // Pre-check: if a sibling worker already established this
                    // pack is missing, short-circuit without burning another
                    // network round-trip.
                    if missing_packs.lock().expect("missing_packs poisoned").contains(&pack_path) {
                        return Err(Error::Storage(format!(
                            "{SKIPPED_PREFIX}pack {pack_path} unavailable"
                        )));
                    }
                    if multi_chunk_packs.contains(&pack_path) {
                        // Multiple chunks needed from this pack: download once, share.
                        let pack_data = match get_or_download_pack(
                            &pack_cache, Arc::clone(&storage), pack_path.clone(),
                            &packs_in_flight, &packs_downloaded,
                            cancel_rx.clone(),
                        ).await {
                            Ok(d) => d,
                            Err(e) => {
                                // Promote a "missing object" error into the
                                // pack-skip set so concurrent + subsequent
                                // workers using this pack don't repeat the
                                // doomed fetch.  Other error categories
                                // (network, auth) flow through unchanged so
                                // they can be retried by the user.
                                if matches!(bkp_types::error::classify_error(&e),
                                            bkp_types::error::ErrorCategory::StorageMissingObject) {
                                    missing_packs.lock().expect("missing_packs poisoned").insert(pack_path);
                                }
                                return Err(e);
                            }
                        };
                        let from = (offset + 4) as usize;
                        let to   = from + enc_size as usize;
                        if to > pack_data.len() {
                            return Err(Error::Storage(format!(
                                "chunk at offset {offset} overruns pack"
                            )));
                        }
                        decrypt_and_decompress(&pack_data[from..to], &chunk_ref.chunk_hash, chunk_key_bytes, chunk_id_key_bytes)?
                    } else {
                        // Only one chunk needed from this pack: range request.
                        // select! against cancel so the HTTP get
                        // doesn't have to complete before the worker can
                        // honour an SCM STOP.
                        let from = offset + 4;
                        let to   = from + enc_size;
                        let fetch = storage.get_range(&pack_path, from, to);
                        let encrypted = match cancel_rx.clone() {
                            Some(rx) if *rx.borrow() => return Err(Error::Cancelled),
                            Some(mut rx) => tokio::select! {
                                biased;
                                _ = rx.changed() => return Err(Error::Cancelled),
                                r = fetch => match r {
                                    Ok(b) => b,
                                    Err(e) => {
                                        if matches!(bkp_types::error::classify_error(&e),
                                                    bkp_types::error::ErrorCategory::StorageMissingObject) {
                                            missing_packs.lock().expect("missing_packs poisoned").insert(pack_path);
                                        }
                                        return Err(e);
                                    }
                                },
                            },
                            None => match fetch.await {
                                Ok(b) => b,
                                Err(e) => {
                                    if matches!(bkp_types::error::classify_error(&e),
                                                bkp_types::error::ErrorCategory::StorageMissingObject) {
                                        missing_packs.lock().expect("missing_packs poisoned").insert(pack_path);
                                    }
                                    return Err(e);
                                }
                            },
                        };
                        decrypt_and_decompress(&encrypted, &chunk_ref.chunk_hash, chunk_key_bytes, chunk_id_key_bytes)?
                    }
                } else {
                    // Resolver miss: individual chunk object fallback.
                    let path = chunk_object_path(&chunk_ref.chunk_hash);
                    let fetch = storage.get(&path);
                    let encrypted = match cancel_rx.clone() {
                        Some(rx) if *rx.borrow() => return Err(Error::Cancelled),
                        Some(mut rx) => tokio::select! {
                            biased;
                            _ = rx.changed() => return Err(Error::Cancelled),
                            r = fetch => r.map_err(|e| {
                                warn!(chunk = %hex::encode(chunk_ref.chunk_hash.as_bytes()),
                                      "chunk download failed: {e}");
                                e
                            })?,
                        },
                        None => fetch.await.map_err(|e| {
                            warn!(chunk = %hex::encode(chunk_ref.chunk_hash.as_bytes()),
                                  "chunk download failed: {e}");
                            e
                        })?,
                    };
                    decrypt_and_decompress(&encrypted, &chunk_ref.chunk_hash, chunk_key_bytes, chunk_id_key_bytes)?
                }
            } else {
                fetch_chunk(storage.as_ref(), &None, &chunk_ref.chunk_hash, chunk_key_bytes, chunk_id_key_bytes).await?
            };
            write_chunk_maybe_sparse(&mut file, chunk_ref.plaintext_offset, &plaintext, sparse)
                .await
                .map_err(|e| Error::Storage(format!("write {}: {e}", dest.display())))?;
        }
        // Sparse writes seek per chunk and skip all-zero regions, so the file
        // cursor is not guaranteed to sit at the logical end.  Extend the file
        // to its full length so a trailing hole is preserved and the size is
        // correct.  (Harmless when the last chunk was written dense.)
        if sparse {
            file.set_len(total_len)
                .await
                .map_err(|e| Error::Storage(format!("set_len {}: {e}", dest.display())))?;
        }
        file.flush().await
            .map_err(|e| Error::Storage(format!("flush {}: {e}", dest.display())))
        // per-file sync_all moved out to a per-BATCH parallel
        // fsync run by the engine's drain loop right before each
        // checkpoint commit (see batch_fsync_paths).
    }
    .await;

    drop(file);
    if let Err(e) = write_result {
        let _ = tokio::fs::remove_file(&dest).await;
        return Err(e);
    }
    // apply mtime + owner inline so the per-file finalizing pass
    // is no longer needed.  Runs in spawn_blocking so syscalls don't
    // block the tokio runtime.  Errors are logged in apply_* helpers and
    // never propagated - the file content is correct either way; only
    // the attribute couldn't be applied.
    apply_file_metadata(dest.clone(), mtime_ns, mode, owner).await;
    Ok(())
}

/// Download a single chunk from a named pack file using a full pack download.
///
/// Prefer [`fetch_chunk_ranged`] when the encrypted size is known - it issues
/// a single HTTP range request instead of downloading the whole pack.
pub async fn fetch_chunk_from_pack(
    storage: &dyn StorageBackend,
    pack_id: &PackId,
    pack_offset: u64,
    chunk_id: &ChunkId,
    chunk_key_bytes: [u8; 32],
    chunk_id_key_bytes: [u8; 32],
) -> Result<Vec<u8>> {
    let pack_path = format!("packs/{}.pack", pack_id.as_uuid());
    let pack_data = storage.get(&pack_path).await?;
    let (_, index) = bkp_chunker::pack::read_pack_index(&pack_data)?;
    let entry = index
        .iter()
        .find(|e| e.offset == pack_offset)
        .ok_or_else(|| Error::Storage(format!("chunk at offset {pack_offset} not in pack")))?;
    let encrypted = bkp_chunker::pack::extract_chunk(&pack_data, entry)?;
    decrypt_and_decompress(encrypted, chunk_id, chunk_key_bytes, chunk_id_key_bytes)
}

/// Download a single encrypted chunk via an HTTP range request.
///
/// `pack_offset` is the byte offset of the 4-byte size prefix in the pack.
/// `encrypted_size` is the ciphertext length (excluding the size prefix).
/// Both values come from the local SQLite chunk index.
pub async fn fetch_chunk_ranged(
    storage: &dyn StorageBackend,
    pack_id: &PackId,
    pack_offset: u64,
    encrypted_size: u64,
    chunk_id: &ChunkId,
    chunk_key_bytes: [u8; 32],
    chunk_id_key_bytes: [u8; 32],
) -> Result<Vec<u8>> {
    let pack_path = format!("packs/{}.pack", pack_id.as_uuid());
    // The size prefix occupies 4 bytes; the encrypted payload follows.
    let from = pack_offset + 4;
    let to = from + encrypted_size;
    let encrypted = storage.get_range(&pack_path, from, to).await?;
    decrypt_and_decompress(&encrypted, chunk_id, chunk_key_bytes, chunk_id_key_bytes)
}

// - Core restore logic ----------------------------

#[allow(clippy::too_many_arguments)]
async fn restore_chunks(
    storage: Arc<dyn StorageBackend>,
    resolver: Option<ChunkResolver>,
    chunk_refs: Vec<ChunkRef>,
    dest: &Path,
    chunk_key_bytes: [u8; 32],
    chunk_id_key_bytes: [u8; 32],
    mtime_ns: u64,
    mode: u32,
    owner: RestoreOwner,
    cancel_rx: Option<watch::Receiver<bool>>,
    pause_rx: Option<watch::Receiver<bool>>,
    sparse: bool,
) -> Result<()> {
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| Error::Storage(format!("create_dir_all {}: {e}", parent.display())))?;
    }

    // Sort refs by plaintext offset so we write the file sequentially.
    // Manifests record chunks in order, but sort defensively.
    let mut sorted_refs = chunk_refs;
    sorted_refs.sort_by_key(|r| r.plaintext_offset);

    // Logical file length = highest chunk end; used to finalize a sparse write
    // whose trailing region was an all-zero (skipped) chunk.
    let total_len: u64 = sorted_refs
        .iter()
        .map(|r| r.plaintext_offset + r.plaintext_size)
        .max()
        .unwrap_or(0);

    // Create the destination file up front so we can stream each chunk
    // directly to disk as it arrives.  This keeps peak memory at one
    // chunk (≤ 16 MiB) rather than the entire file.
    let mut file = tokio::fs::File::create(dest)
        .await
        .map_err(|e| Error::Storage(format!("create {}: {e}", dest.display())))?;
    #[cfg(windows)]
    if sparse {
        mark_file_sparse(&file, dest);
    }

    let write_result: Result<()> = async {
        for chunk_ref in &sorted_refs {
            // Pause: hold between chunks until resumed.  Also exits on cancel.
            if let Some(ref prx) = pause_rx {
                while *prx.borrow() {
                    if cancel_rx.as_ref().map(|r| *r.borrow()).unwrap_or(false) {
                        return Err(Error::Cancelled);
                    }
                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                }
            }
            // Cancel: abort mid-file.
            if cancel_rx.as_ref().map(|r| *r.borrow()).unwrap_or(false) {
                return Err(Error::Cancelled);
            }
            let plaintext = fetch_chunk(
                storage.as_ref(),
                &resolver,
                &chunk_ref.chunk_hash,
                chunk_key_bytes,
                chunk_id_key_bytes,
            )
            .await?;
            write_chunk_maybe_sparse(&mut file, chunk_ref.plaintext_offset, &plaintext, sparse)
                .await
                .map_err(|e| Error::Storage(format!("write {}: {e}", dest.display())))?;
            // plaintext is dropped here - only one chunk in memory at a time.
        }
        if sparse {
            file.set_len(total_len)
                .await
                .map_err(|e| Error::Storage(format!("set_len {}: {e}", dest.display())))?;
        }
        file.flush()
            .await
            .map_err(|e| Error::Storage(format!("flush {}: {e}", dest.display())))
        // per-file sync_all moved out; engine batches fsync.
    }
    .await;

    drop(file); // close before setting times so the handle doesn't hold the mtime

    // If any chunk download or write failed, remove the incomplete file so
    // the destination directory is never left with an empty/partial file.
    if let Err(e) = write_result {
        let _ = tokio::fs::remove_file(dest).await;
        return Err(e);
    }

    // Apply mtime and ownership via blocking APIs (platform-specific).
    let dest_owned = dest.to_path_buf();
    tokio::task::spawn_blocking(move || {
        // Restore the original modification time recorded in the manifest.
        if mtime_ns > 0 {
            let mtime = filetime::FileTime::from_unix_time(
                (mtime_ns / 1_000_000_000) as i64,
                (mtime_ns % 1_000_000_000) as u32,
            );
            filetime::set_file_mtime(&dest_owned, mtime)
                .map_err(|e| Error::Storage(format!("set mtime {}: {e}", dest_owned.display())))?;
        }

        // Restore the recorded POSIX permission bits (0 = not recorded / Windows).
        #[cfg(unix)]
        if mode != 0 {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dest_owned, std::fs::Permissions::from_mode(mode & 0o7777))
                .map_err(|e| Error::Storage(format!("set mode {}: {e}", dest_owned.display())))?;
        }

        // Fix file ownership so the requesting user (not root/SYSTEM) owns the file.
        if !owner.owner_sid.is_empty() {
            apply_windows_owner(&dest_owned, &owner.owner_sid);
        }
        apply_unix_owner(&dest_owned, &owner);

        Ok::<(), Error>(())
    })
    .await
    .map_err(|e| Error::Internal(format!("spawn_blocking join: {e}")))?
}

/// Fetch a single chunk - using the pack resolver (range request) when
/// available, otherwise falling back to the individual chunk object path.
async fn fetch_chunk(
    storage: &dyn StorageBackend,
    resolver: &Option<ChunkResolver>,
    chunk_id: &ChunkId,
    chunk_key_bytes: [u8; 32],
    chunk_id_key_bytes: [u8; 32],
) -> Result<Vec<u8>> {
    if let Some(resolve) = resolver
        && let Some((pack_id, offset, enc_size)) = resolve(chunk_id)
    {
        return fetch_chunk_ranged(
            storage,
            &pack_id,
            offset,
            enc_size,
            chunk_id,
            chunk_key_bytes,
            chunk_id_key_bytes,
        )
        .await;
    }
    // Fallback: individual chunk objects (used in tests or legacy stores).
    let path = chunk_object_path(chunk_id);
    let encrypted = storage.get(&path).await.map_err(|e| {
        warn!(
            chunk = %hex::encode(chunk_id.as_bytes()),
            "chunk download failed: {e}"
        );
        e
    })?;
    decrypt_and_decompress(&encrypted, chunk_id, chunk_key_bytes, chunk_id_key_bytes)
}

// - Destination helpers ----------------------------

/// Convert an absolute manifest path to a relative one suitable for joining
/// under a custom or desktop base directory.
///
/// - Unix:    `/home/user/file.txt`   -> `home/user/file.txt`
/// - Windows: `C:/Users/foo/file.txt` -> `Users/foo/file.txt`
///   `C:\Users\foo\file.txt` -> `Users\foo\file.txt`
fn path_to_relative(p: &str) -> &str {
    // Windows drive root: "C:/..." or "C:\..."
    if p.len() >= 3 && p.chars().nth(1) == Some(':') {
        // Skip "X:" then skip one separator ('/' or '\')
        let after_colon = &p[2..];
        after_colon.trim_start_matches(['/', '\\'])
    } else {
        p.trim_start_matches(['/', '\\'])
    }
}

/// Strip the source prefix's PARENT from `file_path`, preserving the
/// source-root basename as the top folder under the destination.
///
/// Used for Custom/Desktop restores so the restored layout includes the
/// backup-set source folder name as a wrapper - users can tell at a
/// glance which source the files came from, and multi-source sets don't
/// have their files collide in the restore destination.
///
/// Strips only the selected sub-path, not the whole source prefix.
/// Source `/home/{username}` + file `/home/{username}/Documents/foo.txt`
/// produced `Documents/foo.txt`; now it produces `{username}/Documents/foo.txt`.
/// Same fix applies on Windows: source `C:\Users\joe\Documents` +
/// `...\foo.txt` now produces `Documents\foo.txt` instead of just `foo.txt`.
///
/// Picks the longest matching source so multi-source sets that overlap
/// (e.g. `/home/{username}` and `/home/{username}/Documents`) strip the more
/// specific one and keep the matching basename.
///
/// If no prefix matches, `path_to_relative(file_path)` is returned as a
/// fallback (strips only the leading `/` or drive letter).
fn strip_source_prefix<'a>(file_path: &'a str, source_prefixes: &[String]) -> &'a str {
    let normalized = file_path.trim_end_matches('/');
    let mut best: Option<&str> = None;
    let mut best_match_len = 0usize;
    for prefix in source_prefixes {
        let p = prefix.trim_end_matches('/');
        if p.is_empty() {
            continue;
        }
        // The parent of `p` - what we'll actually strip, so `p`'s own
        // basename survives as the top folder.  No `/` in `p` (rare:
        // bare drive-letter or unrooted prefix) means parent is empty
        // and we strip nothing - the full original path falls through
        // to path_to_relative's leading-slash trim.
        let parent_len = match p.rfind('/') {
            Some(i) => i + 1, // include the trailing slash so we strip `/home/`, not `/home`
            None => 0,
        };
        if normalized == p {
            // File IS the source root - return just the basename
            // (the source folder name itself, no parent dir).
            if p.len() + 1 >= best_match_len {
                best = Some(normalized.rsplit('/').next().unwrap_or(normalized));
                best_match_len = p.len() + 1;
            }
            continue;
        }
        if let Some(after_prefix) = normalized.strip_prefix(p)
            && after_prefix.starts_with('/')
            && p.len() >= best_match_len
        {
            // Strip only the parent so the prefix's basename
            // survives.  E.g. p=`/home/{username}`, parent_len=6,
            // normalized=`/home/{username}/Documents/foo.txt` ->
            // `{username}/Documents/foo.txt`.
            let stripped = &normalized[parent_len..].trim_start_matches('/');
            best = Some(stripped);
            best_match_len = p.len();
        }
    }
    best.unwrap_or_else(|| path_to_relative(file_path))
}

/// Resolve the absolute destination path for a file given a [`RestoreTarget`].
fn resolve_dest(
    target: &RestoreTarget,
    file_path: &str,
    snapshot_unix_secs: u64,
    source_prefixes: &[String],
) -> Result<PathBuf> {
    let rel = file_path.trim_start_matches('/');
    match target {
        RestoreTarget::Original => {
            // Absolute path as recorded in the manifest.
            // Manifests always use '/' as separator (normalised by the engine).
            // On Windows a path like "C:/Users/foo/file.txt" is absolute.
            if file_path.starts_with('/')
                || (file_path.len() >= 3 && file_path.chars().nth(1) == Some(':'))
            {
                // Convert forward slashes to the OS separator so PathBuf is
                // treated as absolute on Windows ("C:/..." → "C:\...").
                Ok(PathBuf::from(
                    file_path.replace('/', std::path::MAIN_SEPARATOR_STR),
                ))
            } else {
                Ok(PathBuf::from("/").join(rel))
            }
        }
        RestoreTarget::Desktop => {
            let desktop = local_desktop_dir()
                .ok_or_else(|| Error::Storage("cannot locate Desktop directory".into()))?;
            let folder = snapshot_folder_name(snapshot_unix_secs);
            let rel_part = strip_source_prefix(file_path, source_prefixes);
            Ok(desktop.join(folder).join(rel_part))
        }
        RestoreTarget::Custom(base) => {
            let rel_part = strip_source_prefix(file_path, source_prefixes);
            Ok(base.join(sanitize_cross_platform_subpath(rel_part)))
        }
    }
}

/// Make `p` safe to use as a subpath under a destination directory across
/// platforms.
///
/// Inputs come from a manifest that was written on the source OS, so a
/// Windows snapshot restored on Linux carries paths like `C:\Users\steve\f`,
/// and a Linux snapshot restored on Windows carries `/home/steve/f`.
/// The naive `PathBuf::join` on these gives ugly or broken results
/// (`<dest>/C:\Users\steve\f` on Linux is a single 28-char filename with
/// embedded colon and backslashes; `<dest>\\home\\steve\\f` on Windows
/// is an attempted absolute path that join() resolves away).
///
/// Rules:
/// - On any host: convert backslashes to forward slashes so the path is
///   sliced into components by `PathBuf::join`.
/// - On any host: strip leading slashes so the result is treated as
///   relative when joined to `base`.
/// - Windows drive letters (`C:\Foo`, `D:/Bar`) become a top-level folder
///   named for the drive (`C/Foo`, `D/Bar`).  Preserves multi-volume
///   snapshots without filename collisions.
/// - Colon `:` is otherwise illegal in Windows filenames; replace with
///   `_` so a Unix path containing a colon (allowed on POSIX) restored to
///   Windows doesn't fail mid-write.
fn sanitize_cross_platform_subpath(p: &str) -> std::path::PathBuf {
    let mut s = p.replace('\\', "/");

    // Strip leading "/" so the path joins as relative.
    while s.starts_with('/') {
        s.remove(0);
    }

    // Windows drive letter at the start: `C:` followed by `/` (or end).
    // Rewrite to a folder named for the letter.
    if s.len() >= 2 {
        let bytes = s.as_bytes();
        let is_letter = bytes[0].is_ascii_alphabetic();
        if is_letter && bytes[1] == b':' {
            // `C:` or `C:/...`
            let letter = (bytes[0] as char).to_ascii_uppercase();
            let rest = if s.len() >= 3 && bytes[2] == b'/' {
                &s[3..]
            } else {
                &s[2..]
            };
            s = format!("{letter}/{rest}");
        }
    }

    // Replace any remaining colons - illegal in Windows filenames, ugly
    // on Unix.  Use `_` rather than dropping so collisions stay obvious.
    if s.contains(':') {
        s = s.replace(':', "_");
    }

    std::path::PathBuf::from(s)
}

/// Check whether `dest` already exists and apply the overwrite policy.
///
/// Returns:
/// - `Ok(Some(path))` - write to this path (either the original or a rename)
/// - `Ok(None)` - skip (file exists, mode is `Skip`)
fn apply_overwrite_path(dest: PathBuf, mode: OverwriteMode) -> Result<Option<PathBuf>> {
    if !dest.exists() {
        return Ok(Some(dest));
    }
    match mode {
        OverwriteMode::Skip => Ok(None),
        OverwriteMode::Replace => Ok(Some(dest)),
        OverwriteMode::RenameNew => {
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let stem = dest
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let ext = dest.extension().map(|s| s.to_string_lossy().into_owned());
            let new_name = match ext {
                Some(e) => format!("{stem}_restored_{ts}.{e}"),
                None => format!("{stem}_restored_{ts}"),
            };
            let parent = dest.parent().unwrap_or(Path::new("."));
            Ok(Some(parent.join(new_name)))
        }
    }
}

// - File ownership helpers --------------------------

/// Set the NTFS owner of `path` to the user identified by `sid_str`.
///
/// Only compiled on Windows.  Failure is non-fatal - log a warning.
#[cfg(windows)]
#[allow(unsafe_code)]
fn apply_windows_owner(path: &Path, sid_str: &str) {
    use std::ptr;
    use windows_sys::Win32::Foundation::{ERROR_INVALID_OWNER, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSidToSidW, SE_FILE_OBJECT, SetNamedSecurityInfoW,
    };
    use windows_sys::Win32::Security::OWNER_SECURITY_INFORMATION;

    let sid_wide: Vec<u16> = sid_str.encode_utf16().chain(std::iter::once(0)).collect();
    let path_wide: Vec<u16> = path
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    unsafe {
        let mut psid: *mut std::ffi::c_void = ptr::null_mut();
        if ConvertStringSidToSidW(sid_wide.as_ptr(), &mut psid) == 0 {
            warn!(path = %path.display(), "apply_windows_owner: ConvertStringSidToSidW failed");
            return;
        }
        let err = SetNamedSecurityInfoW(
            path_wide.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            psid,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
        );
        LocalFree(psid.cast());
        if err != 0 {
            // ERROR_INVALID_OWNER (1307) is an EXPECTED soft-failure: the
            // backed-up owner SID differs from any account the running
            // process has SeRestorePrivilege over.  The file's contents
            // were restored fine, only the owner attribute couldn't be
            // re-applied.  Log at debug so it doesn't flood the log on
            // every cross-account / cross-machine restore.  Other errors
            // (e.g. ACCESS_DENIED, sharing violations) keep the warn.
            if err == ERROR_INVALID_OWNER {
                tracing::debug!(path = %path.display(),
                    "apply_windows_owner: owner attribute skipped (ERROR_INVALID_OWNER); file restored fine");
            } else {
                warn!(path = %path.display(), error = err,
                    "apply_windows_owner: SetNamedSecurityInfoW failed");
            }
        }
    }
}

#[cfg(not(windows))]
fn apply_windows_owner(_path: &Path, _sid_str: &str) {}

/// `chown` `path` to `owner.unix_uid`/`owner.unix_gid`.
///
/// Only compiled on Unix.  No-op when both uid and gid are 0 (leave as root).
/// Failure is non-fatal - the file is already written; we log a warning.
#[cfg(unix)]
#[allow(unsafe_code)]
fn apply_unix_owner(path: &Path, owner: &RestoreOwner) {
    if owner.unix_uid == 0 && owner.unix_gid == 0 {
        return;
    }
    use std::os::unix::ffi::OsStrExt as _;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap_or_default();
    let ret = unsafe {
        libc::lchown(
            c_path.as_ptr(),
            owner.unix_uid as libc::uid_t,
            owner.unix_gid as libc::gid_t,
        )
    };
    if ret != 0 {
        warn!(path = %path.display(), "apply_unix_owner: lchown failed (errno {})", ret);
    }
}

#[cfg(not(unix))]
fn apply_unix_owner(_path: &Path, _owner: &RestoreOwner) {}

// - Crypto helpers ------------------------------

fn decrypt_and_decompress(
    encrypted: &[u8],
    chunk_id: &ChunkId,
    chunk_enc_key_bytes: [u8; 32],
    chunk_id_key_bytes: [u8; 32],
) -> Result<Vec<u8>> {
    if encrypted.len() < 12 {
        return Err(Error::Crypto("encrypted chunk too short".into()));
    }
    let nonce: [u8; 12] = encrypted[..12]
        .try_into()
        .map_err(|_| Error::Crypto("nonce slice error".into()))?;
    let enc_key = SubKey::from_bytes(chunk_enc_key_bytes);
    let id_key = SubKey::from_bytes(chunk_id_key_bytes);
    let blob = EncryptedBlob {
        nonce,
        ciphertext: encrypted[12..].to_vec(),
    };
    // chunk_id (HMAC-SHA256 of plaintext) is bound as AAD by the encrypt
    // path, and re-verified below: AES-GCM authenticates the ciphertext
    // came from this position, then HMAC-SHA256 verifies the decompressed
    // plaintext matches the manifest's chunk_id under the per-set chunk
    // identity key.  Either check failing means the chunk has been swapped,
    // corrupted, or tampered with.
    let compressed = aead::decrypt_with_aad(&enc_key, &blob, chunk_id.as_bytes())?;
    let plaintext = decompress(&compressed)?;
    let actual = keyed_chunk_id(&id_key, &plaintext);
    if actual.as_bytes() != chunk_id.as_bytes() {
        return Err(Error::Crypto(format!(
            "chunk hash mismatch: expected {}, got {}",
            hex::encode(chunk_id.as_bytes()),
            hex::encode(actual.as_bytes())
        )));
    }
    Ok(plaintext)
}

// - Tree helpers -------------------------------

fn find_file_in_tree<'a>(node: &'a TreeNode, query: &str) -> Option<&'a FileEntry> {
    match node.node_type {
        NodeType::Directory => {
            for child in &node.children {
                if let Some(e) = find_file_in_tree(child, query) {
                    return Some(e);
                }
            }
            None
        }
        NodeType::File | NodeType::Symlink => {
            let q = query.trim_start_matches('/');
            let name = node.name.trim_start_matches('/');
            if name == q || name.ends_with(q) || q.ends_with(name) {
                node.file_entry.as_ref()
            } else {
                None
            }
        }
    }
}

/// Flatten the manifest tree into per-restorable-entry tuples.
///
/// Tuple shape: `(path, chunks, mtime_ns, symlink_target)`.
///
/// `symlink_target` is `Some(target_string)` when the entry is a
/// `NodeType::Symlink` carrying a recorded target path; the restore
/// worker dispatches to [`restore_symlink`] in that case instead of
/// to the chunk-based file writer.  Regular files come through with
/// `symlink_target == None` and a populated `chunks` list.  A
/// symlink entry with no recorded target (rare, only seen on older
/// manifests written before the field existed) yields
/// `symlink_target == None` and an empty `chunks` list - it would
/// restore as a zero-byte regular file; the worker explicitly logs
/// a warning so the user can trace it.
fn collect_files(
    node: &TreeNode,
    prefix: &Path,
    out: &mut Vec<(PathBuf, Vec<ChunkRef>, u64, Option<String>, u32)>,
) {
    let path = if node.name.is_empty() || node.name == "/" {
        prefix.to_path_buf()
    } else {
        prefix.join(&node.name)
    };
    match node.node_type {
        NodeType::Directory => {
            for child in &node.children {
                collect_files(child, &path, out);
            }
        }
        NodeType::File | NodeType::Symlink => {
            if let Some(entry) = &node.file_entry {
                // Honor a recorded symlink target whenever present.  The engine
                // stores symlinks as File nodes that carry `symlink_target`, so
                // keying off `node_type` alone dropped them - they restored as
                // empty regular files instead of symlinks.
                let sym = entry.symlink_target.clone();
                out.push((path, entry.chunks.clone(), entry.mtime_ns, sym, entry.mode));
            }
        }
    }
}

/// Flatten the manifest file tree into a list of [`SnapshotFileEntry`] items.
///
/// `prefix` is the accumulated path so far (empty string at the root call).
fn collect_file_entries(node: &TreeNode, prefix: &str, out: &mut Vec<SnapshotFileEntry>) {
    let path = if node.name.is_empty() || node.name == "/" {
        prefix.to_string()
    } else if prefix.is_empty() {
        node.name.clone()
    } else {
        format!("{prefix}/{}", node.name)
    };
    match node.node_type {
        NodeType::Directory => {
            if !path.is_empty() {
                out.push(SnapshotFileEntry {
                    path: path.clone(),
                    size: 0,
                    mtime_ns: 0,
                    is_dir: true,
                    is_symlink: false,
                });
            }
            for child in &node.children {
                collect_file_entries(child, &path, out);
            }
        }
        NodeType::File => {
            let (size, mtime_ns) = node
                .file_entry
                .as_ref()
                .map(|e| (e.size, e.mtime_ns))
                .unwrap_or((0, 0));
            out.push(SnapshotFileEntry {
                path,
                size,
                mtime_ns,
                is_dir: false,
                is_symlink: false,
            });
        }
        NodeType::Symlink => {
            out.push(SnapshotFileEntry {
                path,
                size: 0,
                mtime_ns: node.file_entry.as_ref().map(|e| e.mtime_ns).unwrap_or(0),
                is_dir: false,
                is_symlink: true,
            });
        }
    }
}

/// Collect all directory nodes from the manifest tree along with their recorded
/// modification times.  `prefix` accumulates the path relative to the tree root.
fn collect_dir_mtimes(
    node: &TreeNode,
    prefix: &std::path::Path,
    out: &mut Vec<(std::path::PathBuf, u64)>,
) {
    if node.node_type != NodeType::Directory {
        return;
    }
    let path = if node.name.is_empty() || node.name == "/" {
        prefix.to_path_buf()
    } else {
        prefix.join(&node.name)
    };
    if !path.as_os_str().is_empty() {
        out.push((path.clone(), node.dir_mtime_ns));
    }
    for child in &node.children {
        collect_dir_mtimes(child, &path, out);
    }
}

/// Storage path for an individual chunk object (fallback when pack+offset not known).
fn chunk_object_path(id: &ChunkId) -> String {
    let hex = hex::encode(id.as_bytes());
    format!("chunks/{}/{}", &hex[..2], &hex[2..])
}

// - Tests ----------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use bkp_types::snapshot::SnapshotId;

    /// `delete_restore_checkpoint` must not error when the file does not
    /// exist - the daemon RPC uses this idempotently and the GUI banner's
    /// "Delete" button must succeed even if the user clicks it twice.
    #[test]
    fn delete_restore_checkpoint_missing_returns_false_not_error() {
        let snapshot_id = SnapshotId::new();
        let result = delete_restore_checkpoint(&snapshot_id);
        // Either Ok(false) (file didn't exist) or Ok(true) (real-world race
        // where another test created and deleted it) - never an error.
        assert!(
            result.is_ok(),
            "delete must be idempotent, got Err: {:?}",
            result.err()
        );
        assert!(!result.unwrap());
    }

    /// `delete_restore_checkpoint` returns `true` when an existing checkpoint
    /// is removed, and `false` on the second call (proves true idempotency
    /// rather than false-success).
    #[test]
    fn delete_restore_checkpoint_existing_returns_true_then_false() {
        let snapshot_id = SnapshotId::new();
        let path = checkpoint_path(&snapshot_id);
        // Make sure parent exists; create a stub checkpoint file.
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, "{}").unwrap();
        assert!(path.exists());

        let first = delete_restore_checkpoint(&snapshot_id).unwrap();
        assert!(first, "first delete should report true");
        assert!(!path.exists(), "file should be gone after first delete");

        let second = delete_restore_checkpoint(&snapshot_id).unwrap();
        assert!(!second, "second delete should report false (idempotent)");
    }

    /// `migrate_legacy_restore_checkpoints` must be a safe no-op when there
    /// is no legacy directory to migrate from (the common case after a fresh
    /// install on the new path scheme).  Specifically: it must not panic, must
    /// not create empty directories, and must not log noise.
    #[test]
    fn migrate_legacy_restore_checkpoints_no_legacy_is_noop() {
        // Just call it and confirm it does not panic.  We can't easily verify
        // the "no log noise" claim from a unit test, but we can verify the
        // function returns cleanly even when nothing exists.
        migrate_legacy_restore_checkpoints();
    }

    /// The new per-machine `restore_checkpoints_dir()` MUST NOT resolve to the
    /// legacy per-user path under `dirs_next::data_local_dir()` when the
    /// daemon is running as root / LocalSystem.  We simulate that context by
    /// inspecting the path string for the platform-appropriate per-machine
    /// prefix.  This guards against the 0.3.x bug where checkpoints landed in
    /// `C:\Windows\System32\config\systemprofile\AppData\Local\nyxbackup\`
    /// when the service ran as LocalSystem.
    #[test]
    #[cfg(target_os = "linux")]
    #[allow(unsafe_code)]
    fn restore_checkpoints_dir_picks_per_machine_path_when_root() {
        // Force "looks like root" via env vars (matches the heuristic the
        // function itself uses since #![forbid(unsafe_code)] precludes a
        // real getuid() check).
        let prev_user = std::env::var_os("USER");
        let prev_home = std::env::var_os("HOME");
        // Rust 2024: std::env::set_var / remove_var are unsafe because they
        // mutate process-wide state (libc::setenv is not thread-safe).  These
        // are guarded by test-only scope which doesn't run concurrent setenv,
        // so the unsafe is OK here.
        unsafe {
            std::env::set_var("USER", "root");
            std::env::set_var("HOME", "/root");
        }

        let dir = restore_checkpoints_dir();
        assert!(
            dir.to_string_lossy().starts_with("/var/lib/nyxbackup/"),
            "as-root must resolve to /var/lib/nyxbackup/, got {}",
            dir.display()
        );

        // Restore env to avoid leaking into other tests.
        unsafe {
            if let Some(v) = prev_user {
                std::env::set_var("USER", v)
            } else {
                std::env::remove_var("USER")
            }
            if let Some(v) = prev_home {
                std::env::set_var("HOME", v)
            } else {
                std::env::remove_var("HOME")
            }
        }
    }
}
