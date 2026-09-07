// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! S3-compatible storage backend (Wasabi, Minio, Storj S3, Cloudflare R2, …).
//!
//! Thin wrapper around [`S3Backend`] that requires an explicit `endpoint_url`
//! and defaults the region to `"us-east-1"` (most providers accept any value).

use bkp_types::error::Result;

use super::s3::{S3Backend, S3Config};
use crate::backend::StorageBackend;

/// Configuration for an S3-compatible provider.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct S3CompatConfig {
    /// Bucket / container name.
    pub bucket: String,
    /// Key prefix within the bucket (empty string for root).
    #[serde(default)]
    pub prefix: String,
    /// Provider endpoint URL, e.g. `"https://s3.wasabisys.com"`.
    pub endpoint_url: String,
    /// Region string accepted by the provider (defaults to `"us-east-1"`).
    #[serde(default = "default_region")]
    pub region: String,
    /// Storage class string (provider-specific, e.g. `"STANDARD"`).
    #[serde(default)]
    pub storage_class: Option<String>,
    /// Access key ID.
    #[serde(default)]
    pub access_key_id: Option<String>,
    /// Secret access key.
    #[serde(default)]
    pub secret_access_key: Option<String>,
    /// same as `S3Config::retrieval_tier`.
    #[serde(default)]
    pub retrieval_tier: Option<String>,
    /// same as `S3Config::restore_lifetime_days`.
    #[serde(default)]
    pub restore_lifetime_days: Option<u32>,
}

fn default_region() -> String {
    "us-east-1".into()
}

/// S3-compatible storage backend.
pub struct S3CompatBackend(S3Backend);

impl S3CompatBackend {
    /// Construct a new `S3CompatBackend`.
    pub fn new(cfg: S3CompatConfig) -> Result<Self> {
        let s3_cfg = S3Config {
            bucket: cfg.bucket,
            prefix: cfg.prefix,
            region: cfg.region,
            storage_class: cfg.storage_class,
            endpoint_url: Some(cfg.endpoint_url),
            access_key_id: cfg.access_key_id,
            secret_access_key: cfg.secret_access_key,
            retrieval_tier: cfg.retrieval_tier,
            restore_lifetime_days: cfg.restore_lifetime_days,
        };
        Ok(Self(S3Backend::new(s3_cfg)?))
    }
}

// Delegate all trait methods to the inner S3Backend.
#[async_trait::async_trait]
impl StorageBackend for S3CompatBackend {
    async fn get(&self, path: &str) -> Result<Vec<u8>> {
        self.0.get(path).await
    }
    async fn get_range(&self, path: &str, from: u64, to: u64) -> Result<Vec<u8>> {
        // Without this forward, the trait default fell back to a full
        // `get` + slice - i.e. every chunk read pulled the entire 256 MiB
        // pack, blowing memory and choking R2.  Restore was effectively
        // unusable on s3_compat endpoints because of this single missing
        // method.
        self.0.get_range(path, from, to).await
    }
    async fn exists(&self, path: &str) -> Result<bool> {
        self.0.exists(path).await
    }
    async fn probe_access(&self) -> Result<()> {
        self.0.probe_access().await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        self.0.list(prefix).await
    }
    async fn list_with_sizes(&self, prefix: &str) -> Result<Vec<(String, u64)>> {
        self.0.list_with_sizes(prefix).await
    }
    async fn size(&self, path: &str) -> Result<u64> {
        self.0.size(path).await
    }
    async fn head_with_hash(&self, path: &str) -> Result<(u64, String, String)> {
        // S3-compatible endpoints (B2, MinIO, Wasabi, Cloudflare R2, etc.)
        // all return an ETag header on HEAD just like AWS S3, so delegate
        // to the inner S3Backend's native impl rather than falling back
        // to the trait default which the quick integrity audit treats
        // as "no hash support".
        self.0.head_with_hash(path).await
    }
    fn display_name(&self) -> String {
        self.0.display_name()
    }
    fn concurrency_hint(&self) -> Option<usize> {
        self.0.concurrency_hint()
    }
    async fn probe_pack_accessible(&self, path: &str) -> Result<bool> {
        self.0.probe_pack_accessible(path).await
    }
    async fn initiate_pack_restore(&self, path: &str) -> Result<()> {
        self.0.initiate_pack_restore(path).await
    }
}
