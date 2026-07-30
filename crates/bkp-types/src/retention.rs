// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! Retention policy types.

use serde::{Deserialize, Serialize};

/// Policy controlling how long snapshots are kept before becoming eligible for deletion.
///
/// Corresponds to the `retention_*` fields in `config.toml` (data format spec Section 6.2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetentionPolicy {
    /// Keep every snapshot taken within this many days of today.
    pub keep_all_days: u32,
    /// Keep one snapshot per calendar week for this many recent weeks.
    pub keep_weekly_count: u32,
    /// Keep one snapshot per calendar month for this many recent months.
    pub keep_monthly_count: u32,
    /// If `false` (default), deletion candidates are logged but not removed until
    /// the user explicitly approves. If `true`, GC runs automatically after each backup.
    pub auto_delete: bool,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            keep_all_days: 30,
            keep_weekly_count: 12,
            keep_monthly_count: 24,
            auto_delete: false,
        }
    }
}
