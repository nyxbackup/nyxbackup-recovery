// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! Backend implementations.

pub mod azure;
pub mod b2;
pub mod dropbox;
pub mod gcs;
pub mod googledrive;
pub mod local;
pub mod oauth;
pub mod onedrive;
pub mod s3;
pub mod s3_compat;
pub mod sftp;
pub mod smb;
pub mod webdav;
