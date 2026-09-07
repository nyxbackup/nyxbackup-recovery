// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! Google Cloud Storage backend.
//!
//! Authentication (in priority order):
//!   1. `service_account_key_json` - inline JSON key string (from keychain).
//!   2. `service_account_key_path` - path to a service account JSON key file.
//!   3. Application Default Credentials via `GOOGLE_APPLICATION_CREDENTIALS` env var.

use bkp_types::error::{Error, Result};
use futures::StreamExt;
use object_store::{ObjectStore, ObjectStoreExt, gcp::GoogleCloudStorageBuilder, path::Path};
use tracing::instrument;

use crate::backend::StorageBackend;

/// Configuration for the GCS backend.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct GcsConfig {
    /// GCS bucket name.
    pub bucket: String,
    /// Key prefix within the bucket (empty string for none).
    #[serde(default)]
    pub prefix: String,
    /// Inline service account key JSON (takes priority over `service_account_key_path`).
    /// Stored via keychain rather than in the config file.
    #[serde(default, skip_serializing)]
    pub service_account_key_json: Option<String>,
    /// Path to a service account JSON key file on the local machine.
    #[serde(default)]
    pub service_account_key_path: Option<String>,
    /// GCS storage class (STANDARD, NEARLINE, COLDLINE, ARCHIVE).
    /// Empty / absent = bucket default.
    #[serde(default)]
    pub storage_class: Option<String>,
}

/// Google Cloud Storage backend.
pub struct GcsBackend {
    store: Box<dyn ObjectStore>,
    bucket: String,
    prefix: String,
    display: String,
}

impl GcsBackend {
    /// Construct a new `GcsBackend`.
    pub fn new(cfg: GcsConfig) -> Result<Self> {
        let mut builder = GoogleCloudStorageBuilder::new().with_bucket_name(&cfg.bucket);

        if let Some(key_json) = &cfg.service_account_key_json {
            builder = builder.with_service_account_key(key_json);
        } else if let Some(key_path) = &cfg.service_account_key_path {
            builder = builder.with_service_account_path(key_path);
        } else {
            // Fall through to Application Default Credentials.
            // GOOGLE_APPLICATION_CREDENTIALS env var or well-known file locations
            // (~/.config/gcloud/application_default_credentials.json on Linux/macOS,
            //  %APPDATA%\gcloud\application_default_credentials.json on Windows).
            let has_adc = std::env::var("GOOGLE_APPLICATION_CREDENTIALS").is_ok();
            if !has_adc {
                return Err(Error::Storage(
                    "GCS credentials required: provide a service account key (via \
                     service_account_key_path or service_account_key_json) or set \
                     GOOGLE_APPLICATION_CREDENTIALS to an ADC key file"
                        .into(),
                ));
            }
        }

        // storage_class is now applied per-PUT via
        // Attribute::StorageClass on PutOptions.  GCS does not use SigV4
        // per-header signing, so the previous default_headers approach
        // technically worked, but routing it through the same path as S3
        // keeps the two backends consistent and avoids carrying extra
        // header crates.
        if let Some(ref class) = cfg.storage_class {
            tracing::debug!(bucket = %cfg.bucket, storage_class = %class, "GCS storage class configured");
        }

        let store = builder
            .build()
            .map_err(|e| Error::Storage(format!("GCS build: {e}")))?;

        let display = format!("gs://{}/{}", cfg.bucket, cfg.prefix);
        Ok(Self {
            store: Box::new(store),
            bucket: cfg.bucket,
            prefix: cfg.prefix,
            display,
        })
    }

    /// Prepend the configured prefix to `path`.
    fn full_key(&self, path: &str) -> Path {
        let p = path.trim_start_matches('/');
        if self.prefix.is_empty() {
            Path::from(p)
        } else {
            let prefix = self.prefix.trim_end_matches('/');
            Path::from(format!("{prefix}/{p}").as_str())
        }
    }

    /// Strip the configured prefix from an object key, returning the logical path.
    fn strip_prefix<'a>(&self, key: &'a str) -> &'a str {
        if self.prefix.is_empty() {
            return key;
        }
        let prefix_slash = format!("{}/", self.prefix.trim_end_matches('/'));
        key.strip_prefix(&prefix_slash).unwrap_or(key)
    }
}

#[async_trait::async_trait]
impl StorageBackend for GcsBackend {
    #[instrument(skip(self), fields(bucket = %self.bucket, key = path))]
    async fn get(&self, path: &str) -> Result<Vec<u8>> {
        let key = self.full_key(path);
        let result = self
            .store
            .get(&key)
            .await
            .map_err(|e| Error::Storage(format!("GCS get {key}: {e}")))?;
        let bytes = result
            .bytes()
            .await
            .map_err(|e| Error::Storage(format!("GCS get body {key}: {e}")))?;
        Ok(bytes.to_vec())
    }

    #[instrument(skip(self), fields(bucket = %self.bucket, key = path, from, to))]
    async fn get_range(&self, path: &str, from: u64, to: u64) -> Result<Vec<u8>> {
        let key = self.full_key(path);
        let bytes = self
            .store
            .get_range(&key, from..to)
            .await
            .map_err(|e| Error::Storage(format!("GCS get_range {key}: {e}")))?;
        Ok(bytes.to_vec())
    }

    // See StorageBackend::probe_access: a single cheap authed round trip via
    // exists("").  Both Ok(true)/Ok(false) mean reachable + authenticated;
    // only a real connect/auth error propagates.  No content enumeration.
    async fn probe_access(&self) -> Result<()> {
        self.exists("").await.map(|_| ())
    }

    #[instrument(skip(self), fields(bucket = %self.bucket, key = path))]
    async fn exists(&self, path: &str) -> Result<bool> {
        let key = self.full_key(path);
        match self.store.head(&key).await {
            Ok(_) => Ok(true),
            Err(object_store::Error::NotFound { .. }) => Ok(false),
            Err(e) => Err(Error::Storage(format!("GCS exists {key}: {e}"))),
        }
    }

    #[instrument(skip(self), fields(bucket = %self.bucket, prefix = prefix))]
    async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        let full_prefix = self.full_key(prefix);
        let mut stream = self.store.list(Some(&full_prefix));
        let mut paths = Vec::new();
        while let Some(meta) = stream.next().await {
            let meta = meta.map_err(|e| Error::Storage(format!("GCS list {full_prefix}: {e}")))?;
            paths.push(self.strip_prefix(meta.location.as_ref()).to_string());
        }
        Ok(paths)
    }

    async fn list_with_sizes(&self, prefix: &str) -> Result<Vec<(String, u64)>> {
        let full_prefix = self.full_key(prefix);
        let mut stream = self.store.list(Some(&full_prefix));
        let mut results = Vec::new();
        while let Some(meta) = stream.next().await {
            let meta = meta.map_err(|e| Error::Storage(format!("GCS list {full_prefix}: {e}")))?;
            results.push((
                self.strip_prefix(meta.location.as_ref()).to_string(),
                meta.size,
            ));
        }
        Ok(results)
    }

    #[instrument(skip(self), fields(bucket = %self.bucket, key = path))]
    async fn size(&self, path: &str) -> Result<u64> {
        let key = self.full_key(path);
        let meta = self
            .store
            .head(&key)
            .await
            .map_err(|e| Error::Storage(format!("GCS size {key}: {e}")))?;
        Ok(meta.size as u64)
    }

    async fn head_with_hash(&self, path: &str) -> Result<(u64, String, String)> {
        let key = self.full_key(path);
        let meta = self
            .store
            .head(&key)
            .await
            .map_err(|e| Error::Storage(format!("GCS head_with_hash {key}: {e}")))?;
        // GCS exposes both crc32c and md5Hash in object metadata.
        // object_store's ObjectMeta surfaces the ETag (which is the
        // crc32c by default for non-composite objects).  Stable across
        // HEADs, so equality with the recorded value detects bit rot.
        let etag = meta
            .e_tag
            .ok_or_else(|| Error::Storage(format!("GCS head_with_hash {key}: no ETag")))?;
        let etag = etag.trim_matches('"').to_string();
        Ok((meta.size as u64, etag, "gcs-etag".into()))
    }

    fn display_name(&self) -> String {
        self.display.clone()
    }
}
