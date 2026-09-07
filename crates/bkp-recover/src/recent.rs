// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! Recently-used endpoints cache.  Convenience-only: stores up to 5 prior
//! connections so a multi-session recovery doesn't force the user to re-type
//! every field each time.
//!
//! this cache stores **all** per-backend fields including the
//! secret access key / password / refresh token.  The file lives under the
//! per-user `%LOCALAPPDATA%\NyxBackup\Recover\` (Windows) /
//! `~/.local/share/nyxbackup-recover/` (Linux) /
//! `~/Library/Application Support/NyxBackup-Recover/` (macOS) directory
//! with the same OS-level ACLs that protect the keyring entries the main
//! app uses.  Clear it by deleting `recent.json` from that folder.
//!
//! The MASTER encryption key is still NEVER persisted - it lives in memory
//! only for the running session.  Storage credentials and the master key
//! are intentionally treated differently: the storage creds get you to the
//! bytes; the master key decrypts them.  Saving the storage creds in a
//! per-user file matches what every other Nyx Backup binary does (the main
//! app stores them in the OS keyring); persisting the master key would let
//! a stolen recovery binary decrypt the user's data, which we don't want.

use crate::paths;
use serde::{Deserialize, Serialize};

const MAX_RECENT: usize = 5;

/// A single recently-used endpoint entry.  Holds enough info to pre-fill
/// the Connect screen for a one-click re-connect (including the storage
/// credentials).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecentEndpoint {
    /// Storage backend type string (e.g. "s3", "azure_blob", "backblaze_b2",
    /// "sftp").  Identical to the registry key used in `bkp-storage`.
    pub endpoint_type: String,
    /// User-facing URL or path.  E.g. an S3 bucket name with prefix
    /// (`s3://my-bucket/data`), an Azure container URL, an SFTP host.
    pub url: String,
    /// Access key ID / username.
    pub key_id: String,
    /// Secret access key / password / refresh token.  Persisted as of
    /// The user opts into this trade-off for the recovery use
    /// case where re-typing the secret every session is painful (especially
    /// on long S3 keys).  See module-level doc for the file-ACL rationale.
    #[serde(default)]
    pub secret: String,
    /// AWS region (S3) / endpoint URL slot for s3_compat.  Matches the
    /// `storage_region` field the main app's editor sends - the SAME slot
    /// carries the S3-compatible endpoint URL via editor convention.
    #[serde(default)]
    pub region: String,
    /// WebDAV base URL.  Empty for other backends.
    #[serde(default)]
    pub endpoint_url: String,
    /// User-friendly label shown in the dropdown.  Defaults to
    /// `<endpoint_type>: <url>` if not customised.
    #[serde(default)]
    pub label: String,
    /// Last-used timestamp (Unix seconds).  Drives the LRU eviction order.
    #[serde(default)]
    pub last_used: u64,
}

/// A bounded LRU of recently-used endpoints.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RecentList {
    #[serde(default)]
    pub items: Vec<RecentEndpoint>,
}

impl RecentList {
    /// Read from disk; returns an empty list on missing / unreadable file.
    pub fn load() -> Self {
        std::fs::read_to_string(paths::recent_endpoints_file())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Persist to disk.  Creates the data dir if needed.  On Unix the file
    /// is created with mode 0600 so other local users can't read the
    /// stored secrets; on Windows the per-user `%LOCALAPPDATA%` ACL
    /// already restricts access to the owning user.
    pub fn save(&self) -> std::io::Result<()> {
        let path = paths::recent_endpoints_file();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let s = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(&path, s)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    /// Insert or move-to-front the given endpoint, evicting the oldest if
    /// the list exceeds `MAX_RECENT`.  Comparison key is
    /// `(endpoint_type, url, key_id)` - re-saving with a new secret /
    /// region updates the existing entry in place rather than creating a
    /// duplicate.
    pub fn touch(&mut self, mut entry: RecentEndpoint) {
        if entry.last_used == 0 {
            entry.last_used = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
        }
        self.items.retain(|e| {
            !(e.endpoint_type == entry.endpoint_type
                && e.url == entry.url
                && e.key_id == entry.key_id)
        });
        self.items.insert(0, entry);
        if self.items.len() > MAX_RECENT {
            self.items.truncate(MAX_RECENT);
        }
    }

    /// Remove the entry matching `(endpoint_type, url, key_id)` - the same
    /// identity key [`touch`](Self::touch) dedupes on.  Returns `true` if an
    /// entry was removed.
    pub fn remove(&mut self, endpoint_type: &str, url: &str, key_id: &str) -> bool {
        let before = self.items.len();
        self.items
            .retain(|e| !(e.endpoint_type == endpoint_type && e.url == url && e.key_id == key_id));
        self.items.len() != before
    }
}
