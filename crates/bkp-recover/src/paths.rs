// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! Per-platform paths used by the Recovery Tool.  Everything under a single
//! root so a clean uninstall is `rm -rf <root>`.

use std::path::PathBuf;

/// Root directory for the Recovery Tool's per-user state.
///
/// - Linux:   `~/.local/share/nyxbackup-recover/`
/// - macOS:   `~/Library/Application Support/NyxBackup-Recover/`
/// - Windows: `%LOCALAPPDATA%\NyxBackup\Recover\`
///
/// Created lazily on first write; absence is not an error.
pub fn data_root() -> PathBuf {
    #[cfg(target_os = "linux")]
    {
        if let Some(d) = dirs_next::data_local_dir() {
            return d.join("nyxbackup-recover");
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(d) = dirs_next::data_dir() {
            return d.join("NyxBackup-Recover");
        }
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(d) = dirs_next::data_local_dir() {
            return d.join("NyxBackup").join("Recover");
        }
    }
    // Last-resort fallback - never expected on shipped builds.
    std::env::temp_dir().join("nyxbackup-recover")
}

/// Where active-restore checkpoints live.  One JSON file per restore session.
pub fn checkpoint_dir() -> PathBuf {
    data_root().join("checkpoints")
}

/// Recently-used endpoints cache (last 5).  No secrets stored here.
pub fn recent_endpoints_file() -> PathBuf {
    data_root().join("recent.json")
}

/// Persistent settings (download bandwidth, log level, theme).
pub fn settings_file() -> PathBuf {
    data_root().join("settings.json")
}

/// Log directory used by `bkp_log::SizeRollingAppender`.
pub fn log_dir() -> PathBuf {
    data_root().join("logs")
}
