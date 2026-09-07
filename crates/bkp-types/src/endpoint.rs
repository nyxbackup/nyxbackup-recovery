// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! Endpoint identity and configuration types defining backup destinations.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Unique identifier for a configured endpoint instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EndpointId(pub Uuid);

impl EndpointId {
    /// Generate a new random endpoint ID.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Construct from an existing UUID.
    pub fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }
}

impl Default for EndpointId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for EndpointId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The kind of storage provider backing an endpoint.
///
/// The string values below are the canonical `type` strings used in `config.toml`
/// (data format spec Section 6.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointType {
    /// AWS S3 - supports Standard, IA, Glacier, and Glacier Deep Archive tiers.
    S3,
    /// S3-compatible endpoint (Wasabi, Minio, Storj, Linode Object Storage, etc.).
    S3Compatible,
    /// Azure Blob Storage - Hot, Cool, Cold, and Archive tiers.
    AzureBlob,
    /// Backblaze B2 native API.
    BackblazeB2,
    /// Google Cloud Storage.
    Gcs,
    /// Google Drive (OAuth).
    GoogleDrive,
    /// Microsoft OneDrive (OAuth).
    OneDrive,
    /// Microsoft SharePoint (planned; not yet wired).
    SharePoint,
    /// Dropbox (OAuth).
    Dropbox,
    /// SFTP.
    Sftp,
    /// SMB network share.
    Smb,
    /// AFP network share (macOS only).
    Afp,
    /// WebDAV server (Nextcloud, ownCloud, Synology, QNAP, Apache mod_dav, etc.).
    /// Explicit rename so serde emits `"webdav"` rather than the
    /// auto-derived `"web_dav"` (`rename_all="snake_case"` splits at the
    /// internal CamelCase boundary).  Matches the canonical string used
    /// everywhere else (config.toml, registry, GUI dropdown).
    #[serde(rename = "webdav")]
    WebDav,
    /// Local folder or external disk.
    Local,
}

impl std::fmt::Display for EndpointType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::S3 => "s3",
            Self::S3Compatible => "s3_compatible",
            Self::AzureBlob => "azure_blob",
            Self::BackblazeB2 => "backblaze_b2",
            Self::Gcs => "gcs",
            Self::GoogleDrive => "google_drive",
            Self::OneDrive => "onedrive",
            Self::SharePoint => "sharepoint",
            Self::Dropbox => "dropbox",
            Self::Sftp => "sftp",
            Self::Smb => "smb",
            Self::Afp => "afp",
            Self::WebDav => "webdav",
            Self::Local => "local",
        };
        write!(f, "{s}")
    }
}

/// Return `true` when the configured `storage_class` for this `endpoint_type`
/// names an archive / cold tier whose objects require an hours-long
/// `RestoreObject`-style thaw + per-GB retrieval fee before they can be
/// downloaded.
///
/// The single source of truth for the cold-tier check.  Both the GUI
/// (disables "Verify integrity" buttons) and the daemon's IntegrityService
/// (RPC defence in depth: returns `FailedPrecondition` for routine deep
/// verify on these tiers) read this so the two layers can't drift.
///
/// `storage_class` is the raw provider string as stored in
/// `EndpointConfig.storage_class` (e.g. `"GLACIER"`, `"DEEP_ARCHIVE"`,
/// `"Archive"`).  `None`/empty always returns `false` (default tier is
/// always hot).
///
/// Comparison is case-insensitive against a small allowlist per backend.
pub fn is_archive_tier(endpoint_type: EndpointType, storage_class: Option<&str>) -> bool {
    let Some(class) = storage_class else {
        return false;
    };
    let c = class.trim();
    if c.is_empty() {
        return false;
    }

    match endpoint_type {
        // S3 + S3-compat: GLACIER (Flexible Retrieval) and DEEP_ARCHIVE
        // require RestoreObject.  GLACIER_IR (Instant Retrieval) does
        // NOT - it's read like Standard, just billed differently.
        // STANDARD / STANDARD_IA / INTELLIGENT_TIERING are all hot.
        EndpointType::S3 | EndpointType::S3Compatible => {
            c.eq_ignore_ascii_case("GLACIER") || c.eq_ignore_ascii_case("DEEP_ARCHIVE")
        }
        // Azure Blob: Archive tier requires rehydrate.  Hot/Cool/Cold
        // are all immediately readable.
        EndpointType::AzureBlob => c.eq_ignore_ascii_case("Archive"),
        // Backblaze B2: native tiers are all hot; only a bucket
        // lifecycle policy can move objects to cold.  We don't
        // currently surface lifecycle state in the config, so treat
        // an explicit "ARCHIVE" string (if ever set) as the trigger.
        EndpointType::BackblazeB2 => c.eq_ignore_ascii_case("ARCHIVE"),
        // GCS: Archive storage class.  Nearline/Coldline still read
        // immediately, just with higher per-op fees.
        EndpointType::Gcs => c.eq_ignore_ascii_case("ARCHIVE"),
        // Other endpoint types have no archive tier concept.
        _ => false,
    }
}

/// A configured backup endpoint (destination).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Endpoint {
    /// Unique identifier for this endpoint instance.
    pub id: EndpointId,
    /// The type of storage provider.
    #[serde(rename = "type")]
    pub endpoint_type: EndpointType,
    /// Handle used to retrieve credentials from the OS keychain.
    /// Credentials are never stored in config files in plaintext.
    pub keychain_handle: String,
    /// Provider-specific settings serialised as a TOML inline string.
    /// Parsed by `bkp-storage::registry::build_backend` when constructing the backend.
    pub settings_toml: String,
}
