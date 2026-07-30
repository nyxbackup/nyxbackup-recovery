// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! BackendRegistry - builds a `StorageBackend` from an endpoint type string
//! and a TOML configuration snippet.
//!
//! The TOML snippet must contain all fields required by the chosen backend's
//! `*Config` struct.  Credentials (passwords, keys) are expected to be injected
//! by the caller (typically read from the OS keychain) before passing the TOML.

use std::sync::Arc;

use bkp_types::error::{Error, Result};

use crate::{
    backend::StorageBackend,
    backends::{
        azure::{AzureBackend, AzureConfig},
        b2::{B2Backend, B2Config},
        dropbox::{DropboxBackend, DropboxConfig},
        gcs::{GcsBackend, GcsConfig},
        googledrive::{GoogleDriveBackend, GoogleDriveConfig},
        local::{LocalBackend, LocalBackendConfig},
        onedrive::{OneDriveBackend, OneDriveConfig},
        s3::{S3Backend, S3Config},
        s3_compat::{S3CompatBackend, S3CompatConfig},
        sftp::{SftpBackend, SftpConfig},
        smb::{SmbBackend, SmbConfig},
        webdav::{WebDavBackend, WebDavConfig},
    },
    retry::{RetryBackend, RetryConfig},
};

/// Build a `StorageBackend` from the endpoint `kind` string and a TOML config.
///
/// `kind` must be one of:
/// `"local"`, `"s3"`, `"s3_compat"` (alias `"s3_compatible"`), `"backblaze_b2"`, `"sftp"`,
/// `"azure_blob"`, `"gcs"`, `"smb"`, `"google_drive"`, `"onedrive"`, `"dropbox"`.
///
/// The result is automatically wrapped in a [`RetryBackend`] with the default
/// retry policy (7 retries, 2 s base delay, 2× back-off, 120 s cap, ±25 %
/// jitter) so all backends retry transient network errors without any
/// per-backend code.
/// Variant of [`build_backend`] that lets the caller specify a custom
/// [`RetryConfig`] instead of `RetryConfig::default()` (infinite retries).
/// Use this for one-shot best-effort operations like bootstrap-records,
/// where infinite retries against a dead endpoint just spam the log
/// without ever succeeding.
pub async fn build_backend_with_retry(
    kind: &str,
    config_toml: &str,
    retry: RetryConfig,
) -> Result<Arc<dyn StorageBackend>> {
    let inner = build_backend_inner(kind, config_toml).await?;
    Ok(Arc::new(RetryBackend::new(inner, retry)))
}

/// Build a [`StorageBackend`] and wrap it in a [`RetryBackend`] with the
/// default infinite-retry policy.  See [`build_backend_with_retry`] for a
/// caller-controlled retry cap.
pub async fn build_backend(kind: &str, config_toml: &str) -> Result<Arc<dyn StorageBackend>> {
    let inner = build_backend_inner(kind, config_toml).await?;
    Ok(Arc::new(RetryBackend::new(inner, RetryConfig::default())))
}

async fn build_backend_inner(kind: &str, config_toml: &str) -> Result<Arc<dyn StorageBackend>> {
    let inner: Arc<dyn StorageBackend> = match kind {
        "local" => {
            let cfg: LocalBackendConfig = toml::from_str(config_toml)
                .map_err(|e| Error::Config(format!("local backend config: {e}")))?;
            let backend = LocalBackend::new(cfg.root)?;
            Arc::new(backend)
        }
        "s3" => {
            let cfg: S3Config = toml::from_str(config_toml)
                .map_err(|e| Error::Config(format!("S3 backend config: {e}")))?;
            Arc::new(S3Backend::new(cfg)?)
        }
        "s3_compat" | "s3_compatible" => {
            let cfg: S3CompatConfig = toml::from_str(config_toml)
                .map_err(|e| Error::Config(format!("S3Compat backend config: {e}")))?;
            Arc::new(S3CompatBackend::new(cfg)?)
        }
        "backblaze_b2" => {
            let cfg: B2Config = toml::from_str(config_toml)
                .map_err(|e| Error::Config(format!("B2 backend config: {e}")))?;
            Arc::new(B2Backend::new(cfg).await?)
        }
        "sftp" => {
            let cfg: SftpConfig = toml::from_str(config_toml)
                .map_err(|e| Error::Config(format!("SFTP backend config: {e}")))?;
            Arc::new(SftpBackend::new(cfg))
        }
        "azure_blob" => {
            let cfg: AzureConfig = toml::from_str(config_toml)
                .map_err(|e| Error::Config(format!("Azure backend config: {e}")))?;
            Arc::new(AzureBackend::new(cfg)?)
        }
        "gcs" => {
            let cfg: GcsConfig = toml::from_str(config_toml)
                .map_err(|e| Error::Config(format!("GCS backend config: {e}")))?;
            Arc::new(GcsBackend::new(cfg)?)
        }
        "smb" => {
            let cfg: SmbConfig = toml::from_str(config_toml)
                .map_err(|e| Error::Config(format!("SMB backend config: {e}")))?;
            Arc::new(SmbBackend::new(cfg)?)
        }
        "google_drive" => {
            let cfg: GoogleDriveConfig = toml::from_str(config_toml)
                .map_err(|e| Error::Config(format!("Google Drive backend config: {e}")))?;
            Arc::new(GoogleDriveBackend::new(cfg)?)
        }
        "onedrive" => {
            let cfg: OneDriveConfig = toml::from_str(config_toml)
                .map_err(|e| Error::Config(format!("OneDrive backend config: {e}")))?;
            Arc::new(OneDriveBackend::new(cfg)?)
        }
        "dropbox" => {
            let cfg: DropboxConfig = toml::from_str(config_toml)
                .map_err(|e| Error::Config(format!("Dropbox backend config: {e}")))?;
            Arc::new(DropboxBackend::new(cfg)?)
        }
        "webdav" => {
            let cfg: WebDavConfig = toml::from_str(config_toml)
                .map_err(|e| Error::Config(format!("WebDAV backend config: {e}")))?;
            Arc::new(WebDavBackend::new(cfg)?)
        }
        other => return Err(Error::Config(format!("unknown endpoint type: {other:?}"))),
    };
    Ok(inner)
}
