// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! Restart-safe restore checkpoint.  One JSON file per active
//! restore session under [`crate::paths::checkpoint_dir`].  Resumed
//! transparently when the Recovery Tool starts and finds an unfinished file.
//!
//! Master keys are NEVER persisted - resuming a checkpoint re-prompts the
//! user for the key.  This is a deliberate security floor: a stolen laptop
//! with a half-finished restore in its data dir does not expose plaintext
//! keys.

use crate::paths;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// On-disk shape for a single interrupted-restore checkpoint.  Mirrors the
/// main daemon's `restore_session` table but as a flat file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    /// Unique identifier for this session - used as the filename stem.
    pub session_id: String,
    /// Snapshot UUID being restored (as stored in the source endpoint).
    pub snapshot_id: String,
    /// Backup set UUID the snapshot belongs to.
    pub set_id: String,
    /// Endpoint creds excluding the secret.  Secret is solicited on resume.
    pub endpoint: EndpointConfig,
    /// Local destination directory the user picked.
    pub destination: PathBuf,
    /// Filter paths the user selected (mirrors RestoreAllRequest).
    #[serde(default)]
    pub filter_paths: Vec<String>,
    /// Excluded paths within the selection.
    #[serde(default)]
    pub excluded_paths: Vec<String>,
    /// Paths already restored to disk - skipped on resume.
    #[serde(default)]
    pub completed_files: Vec<String>,
    /// Total bytes expected for the restore (sum of FileEntry.size).
    pub bytes_total: u64,
    /// Bytes restored so far.
    #[serde(default)]
    pub bytes_done: u64,
    /// Started-at timestamp (Unix seconds).
    pub started_at: u64,
    /// Last-updated timestamp (Unix seconds) - LRU on the resume banner.
    #[serde(default)]
    pub last_updated: u64,
}

/// Minimal endpoint config stored in the checkpoint - same fields as
/// `recent::RecentEndpoint` plus the user-facing label.  No secret.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndpointConfig {
    pub endpoint_type: String,
    pub url: String,
    pub key_id: String,
    #[serde(default)]
    pub label: String,
}

impl Checkpoint {
    /// Write the checkpoint to disk atomically (tmp + rename).
    pub fn save(&self) -> std::io::Result<()> {
        let dir = paths::checkpoint_dir();
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}.json", self.session_id));
        let tmp = path.with_extension("json.tmp");
        let body = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Remove the checkpoint - called on successful restore completion or
    /// explicit user Discard.
    pub fn discard(session_id: &str) -> std::io::Result<()> {
        let path = paths::checkpoint_dir().join(format!("{session_id}.json"));
        match std::fs::remove_file(&path) {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Scan the checkpoint directory.  Returns one entry per parseable file;
    /// silently skips unparseable ones so a corrupt sibling can't block the
    /// whole recovery flow.
    pub fn list_all() -> Vec<Checkpoint> {
        let dir = paths::checkpoint_dir();
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let Ok(body) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(cp) = serde_json::from_str::<Checkpoint>(&body) else {
                continue;
            };
            out.push(cp);
        }
        // Most-recent first.
        out.sort_by_key(|c| std::cmp::Reverse(c.last_updated));
        out
    }
}
