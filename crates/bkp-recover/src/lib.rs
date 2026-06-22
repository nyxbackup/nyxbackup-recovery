// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! Recovery Tool engine - a standalone, license-free, no-daemon restore
//! implementation.
//!
//! Sits directly on top of `bkp-storage`, `bkp-restore`, `bkp-crypto`, and
//! `bkp-manifest`.  All state lives in memory for the session and dies on
//! exit; checkpoints under the user's data dir are the only on-disk artefact.
//!
//! See `docs/REQUIREMENTS.md` for the full design.

pub mod checkpoint;
pub mod commands;
pub mod dropbox_oauth;
pub mod errors;
pub mod google_oauth;
#[cfg(target_os = "macos")]
pub mod menu_i18n;
pub mod onedrive_oauth;
pub mod paths;
pub mod recent;
pub mod session;
pub mod settings;
