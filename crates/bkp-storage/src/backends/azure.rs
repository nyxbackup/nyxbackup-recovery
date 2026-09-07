// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! Azure Blob Storage backend.
//!
//! Uses `object_store` with the `azure` feature - the same crate that backs the
//! S3 backend - so the implementation is nearly identical to `S3Backend`.
//!
//! # Credentials
//!
//! Supply either:
//! - `account` + `access_key` directly, **or**
//! - `connection_string` (format:
//!   `DefaultEndpointsProtocol=https;AccountName=…;AccountKey=…;EndpointSuffix=core.windows.net`)
//!   from which `AccountName` and `AccountKey` are parsed automatically.
//!
//! # Conditional put
//!
//! `put_if_absent` uses `PutMode::Create` which maps to `If-None-Match: *` - the
//! same mechanism used by the S3 backend.  Azure Blob Storage supports this
//! natively for block blobs.

use bkp_types::error::{Error, Result};
use futures::StreamExt;
use object_store::{ObjectStore, ObjectStoreExt, azure::MicrosoftAzureBuilder, path::Path};
use tracing::instrument;

use crate::backend::StorageBackend;

/// Configuration for the Azure Blob Storage backend.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct AzureConfig {
    /// Storage account name.  Ignored when `connection_string` is set.
    #[serde(default)]
    pub account: String,
    /// Container name.
    pub container: String,
    /// Key prefix within the container (empty for root).
    #[serde(default)]
    pub prefix: String,
    /// Storage account access key (base64-encoded 512-bit key as shown in the
    /// Azure portal).  Required unless `connection_string` is set.
    #[serde(default)]
    pub access_key: Option<String>,
    /// Azure Storage connection string.  When present, `account` and `access_key`
    /// are parsed from it and the explicit fields are ignored.
    #[serde(default)]
    pub connection_string: Option<String>,
    /// Azure Blob access tier: "Hot", "Cool", "Cold", or "Archive".  Empty /
    /// absent = container default.  Applied only to `packs/...` so the
    /// snapshot-index and manifests remain immediately readable on every
    /// run (same scoping as the S3 storage_class plumbing).  Archive tier
    /// requires rehydration on restore - prefer Cold for backups.
    #[serde(default)]
    pub storage_class: Option<String>,
}

/// Azure Blob Storage backend.
pub struct AzureBackend {
    store: Box<dyn ObjectStore>,
    account: String,
    container: String,
    /// Base64-encoded account key, retained so `set_tier` can construct
    /// SharedKey-signed requests against the REST API for tiers the
    /// `azure_storage_blobs` SDK enum does not enumerate (notably Cold).
    /// this is a Secret-shaped value; the field is read at
    /// signing time and never logged.
    access_key: String,
    prefix: String,
    display: String,
    /// Shared HTTPS client for set_tier REST calls.  Reused across
    /// every set_tier invocation in a re-tier sweep so a 10000-pack
    /// re-tier doesn't open 10000 TLS connections.
    http: reqwest::Client,
}

impl AzureBackend {
    /// Construct a new `AzureBackend`.
    pub fn new(cfg: AzureConfig) -> Result<Self> {
        let (account, access_key) =
            if let Some(cs) = cfg.connection_string.as_deref().filter(|s| !s.is_empty()) {
                parse_connection_string(cs).ok_or_else(|| {
                    Error::Config(
                        "Azure connection_string must contain AccountName= and AccountKey=".into(),
                    )
                })?
            } else {
                let ak = cfg.access_key.clone().unwrap_or_default();
                (cfg.account.clone(), ak)
            };

        if account.is_empty() {
            return Err(Error::Config(
                "Azure config: 'account' is required (or provide 'connection_string')".into(),
            ));
        }

        let store = MicrosoftAzureBuilder::new()
            .with_account(&account)
            .with_access_key(&access_key)
            .with_container_name(&cfg.container)
            .build()
            .map_err(|e| Error::Storage(format!("Azure build: {e}")))?;

        let display = format!("azure://{}/{}", account, cfg.container);
        if let Some(ref class) = cfg.storage_class
            && !class.is_empty()
        {
            tracing::debug!(account = %account, access_tier = %class, "Azure access tier configured");
        }
        let http = reqwest::Client::builder()
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .connect_timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| Error::Storage(format!("Azure HTTP client build: {e}")))?;

        Ok(Self {
            store: Box::new(store),
            account,
            container: cfg.container,
            access_key,
            prefix: cfg.prefix,
            display,
            http,
        })
    }

    fn full_key(&self, path: &str) -> Path {
        let p = path.trim_start_matches('/');
        if self.prefix.is_empty() {
            Path::from(p)
        } else {
            let prefix = self.prefix.trim_end_matches('/');
            Path::from(format!("{prefix}/{p}").as_str())
        }
    }

    fn strip_prefix<'a>(&self, key: &'a str) -> &'a str {
        if self.prefix.is_empty() {
            return key;
        }
        let prefix_slash = format!("{}/", self.prefix.trim_end_matches('/'));
        key.strip_prefix(&prefix_slash).unwrap_or(key)
    }

    /// Call the Azure Blob `Set Blob Tier` REST endpoint with SharedKey
    /// auth.  Used by [`set_tier`](StorageBackend::set_tier) because the
    /// `azure_storage_blobs` SDK's `AccessTier` enum doesn't include
    /// the Cold tier (introduced Nov 2023) - going through the SDK
    /// would fail for Cold even though Cold is the cheapest instant
    /// access tier and the one we most want to support.
    ///
    /// `new_tier` MUST be one of `Hot`, `Cool`, `Cold`, or `Archive`.
    ///
    /// Reference: <https://learn.microsoft.com/rest/api/storageservices/set-blob-tier>
    async fn rest_set_blob_tier(&self, logical_path: &str, new_tier: &str) -> Result<()> {
        self.rest_set_blob_tier_inner(logical_path, new_tier).await
    }

    /// probe whether the Azure blob at `path` is
    /// in the Archive tier and therefore not directly downloadable.
    /// Returns `Ok(true)` if archived, `Ok(false)` if accessible (Hot /
    /// Cool / Cold), `Ok(false)` on any HEAD failure so a transient
    /// glitch doesn't block restore - the subsequent `get_range` will
    /// surface the real error.
    async fn probe_pack_archived(&self, logical_path: &str) -> Result<bool> {
        const API_VERSION: &str = "2021-12-02";
        let blob_key = self.full_key(logical_path);
        let blob_path = blob_key.as_ref();
        let utc = chrono::Utc::now()
            .format("%a, %d %b %Y %H:%M:%S GMT")
            .to_string();
        let url = format!(
            "https://{}.blob.core.windows.net/{}/{}",
            self.account, self.container, blob_path
        );
        let string_to_sign = format!(
            "HEAD\n\n\n\n\n\n\n\n\n\n\n\nx-ms-date:{date}\nx-ms-version:{ver}\n/{acct}/{cont}/{blob}",
            date = utc,
            ver = API_VERSION,
            acct = self.account,
            cont = self.container,
            blob = blob_path,
        );
        use base64::{Engine, engine::general_purpose::STANDARD as B64};
        let key_bytes = B64
            .decode(&self.access_key)
            .map_err(|e| Error::Storage(format!("Azure access_key is not valid base64: {e}")))?;
        let mac = bkp_crypto::hmac::hmac_sha256_raw(&key_bytes, string_to_sign.as_bytes());
        let signature = B64.encode(mac);
        let auth = format!("SharedKey {}:{}", self.account, signature);

        let resp = match self
            .http
            .head(&url)
            .header("x-ms-version", API_VERSION)
            .header("x-ms-date", &utc)
            .header("Authorization", &auth)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!(blob = blob_path, error = %e, "Azure archive probe HEAD failed; assuming accessible");
                return Ok(false);
            }
        };
        if !resp.status().is_success() {
            tracing::debug!(blob = blob_path, status = %resp.status(),
                            "Azure archive probe HEAD non-success; assuming accessible");
            return Ok(false);
        }
        let tier = resp
            .headers()
            .get("x-ms-access-tier")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let status = resp
            .headers()
            .get("x-ms-archive-status")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if tier.eq_ignore_ascii_case("Archive") {
            tracing::debug!(blob = blob_path, archive_status = %status,
                            "Azure blob is archived");
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn rest_set_blob_tier_inner(&self, logical_path: &str, new_tier: &str) -> Result<()> {
        const API_VERSION: &str = "2021-12-02";

        let blob_key = self.full_key(logical_path);
        let blob_path = blob_key.as_ref();
        let utc = chrono::Utc::now()
            .format("%a, %d %b %Y %H:%M:%S GMT")
            .to_string();
        let url = format!(
            "https://{}.blob.core.windows.net/{}/{}?comp=tier",
            self.account, self.container, blob_path
        );

        // StringToSign for Azure Blob SharedKey, full thirteen-field
        // canonical form per "Authorize with Shared Key" docs.  PUT
        // with comp=tier carries no body, no content-type, and no
        // content-length header - all the empty lines stay empty.
        let string_to_sign = format!(
            "PUT\n\n\n\n\n\n\n\n\n\n\n\nx-ms-access-tier:{tier}\nx-ms-date:{date}\nx-ms-version:{ver}\n/{acct}/{cont}/{blob}\ncomp:tier",
            tier = new_tier,
            date = utc,
            ver = API_VERSION,
            acct = self.account,
            cont = self.container,
            blob = blob_path,
        );

        use base64::{Engine, engine::general_purpose::STANDARD as B64};
        let key_bytes = B64
            .decode(&self.access_key)
            .map_err(|e| Error::Storage(format!("Azure access_key is not valid base64: {e}")))?;
        let mac = bkp_crypto::hmac::hmac_sha256_raw(&key_bytes, string_to_sign.as_bytes());
        let signature = B64.encode(mac);
        let auth = format!("SharedKey {}:{}", self.account, signature);

        let resp = self
            .http
            .put(&url)
            .header("x-ms-version", API_VERSION)
            .header("x-ms-date", &utc)
            .header("x-ms-access-tier", new_tier)
            .header("Authorization", &auth)
            .header("Content-Length", "0")
            .send()
            .await
            .map_err(|e| Error::Storage(format!("Azure set_tier PUT {blob_path}: {e}")))?;

        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }

        let body = resp.text().await.unwrap_or_default();
        Err(Error::Storage(format!(
            "Azure set_tier {blob_path} -> {new_tier} failed: HTTP {status}: {body}"
        )))
    }
}

/// Parse `AccountName` and `AccountKey` from an Azure Storage connection string.
fn parse_connection_string(s: &str) -> Option<(String, String)> {
    let mut account = None;
    let mut key = None;
    for part in s.split(';') {
        if let Some(v) = part.strip_prefix("AccountName=") {
            account = Some(v.to_string());
        } else if let Some(v) = part.strip_prefix("AccountKey=") {
            key = Some(v.to_string());
        }
    }
    account.zip(key)
}

#[async_trait::async_trait]
impl StorageBackend for AzureBackend {
    #[instrument(skip(self), fields(account = %self.account, key = path))]
    async fn get(&self, path: &str) -> Result<Vec<u8>> {
        let key = self.full_key(path);
        let result = match self.store.get(&key).await {
            Ok(r) => r,
            Err(object_store::Error::NotFound { .. }) => {
                return Err(Error::Storage(format!("Azure get {key}: not found (404)")));
            }
            Err(e) => return Err(Error::Storage(format!("Azure get {key}: {e}"))),
        };
        let bytes = result
            .bytes()
            .await
            .map_err(|e| Error::Storage(format!("Azure get body {key}: {e}")))?;
        Ok(bytes.to_vec())
    }

    #[instrument(skip(self), fields(account = %self.account, key = path, from, to))]
    async fn get_range(&self, path: &str, from: u64, to: u64) -> Result<Vec<u8>> {
        let key = self.full_key(path);
        let bytes = self
            .store
            .get_range(&key, from..to)
            .await
            .map_err(|e| Error::Storage(format!("Azure get_range {key}: {e}")))?;
        Ok(bytes.to_vec())
    }

    // See StorageBackend::probe_access: a single cheap authed round trip via
    // exists("").  Both Ok(true)/Ok(false) mean reachable + authenticated;
    // only a real connect/auth error propagates.  No content enumeration.
    async fn probe_access(&self) -> Result<()> {
        self.exists("").await.map(|_| ())
    }

    #[instrument(skip(self), fields(account = %self.account, key = path))]
    async fn exists(&self, path: &str) -> Result<bool> {
        let key = self.full_key(path);
        match self.store.head(&key).await {
            Ok(_) => Ok(true),
            Err(object_store::Error::NotFound { .. }) => Ok(false),
            Err(e) => Err(Error::Storage(format!("Azure exists {key}: {e}"))),
        }
    }

    #[instrument(skip(self), fields(account = %self.account, prefix = prefix))]
    async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        let full_prefix = self.full_key(prefix);
        let mut stream = self.store.list(Some(&full_prefix));
        let mut paths = Vec::new();
        while let Some(meta) = stream.next().await {
            let meta =
                meta.map_err(|e| Error::Storage(format!("Azure list {full_prefix}: {e}")))?;
            paths.push(self.strip_prefix(meta.location.as_ref()).to_string());
        }
        Ok(paths)
    }

    async fn list_with_sizes(&self, prefix: &str) -> Result<Vec<(String, u64)>> {
        let full_prefix = self.full_key(prefix);
        let mut stream = self.store.list(Some(&full_prefix));
        let mut results = Vec::new();
        while let Some(meta) = stream.next().await {
            let meta =
                meta.map_err(|e| Error::Storage(format!("Azure list {full_prefix}: {e}")))?;
            results.push((
                self.strip_prefix(meta.location.as_ref()).to_string(),
                meta.size,
            ));
        }
        Ok(results)
    }

    #[instrument(skip(self), fields(account = %self.account, key = path))]
    async fn size(&self, path: &str) -> Result<u64> {
        let key = self.full_key(path);
        let meta = self
            .store
            .head(&key)
            .await
            .map_err(|e| Error::Storage(format!("Azure size {key}: {e}")))?;
        Ok(meta.size as u64)
    }

    async fn head_with_hash(&self, path: &str) -> Result<(u64, String, String)> {
        let key = self.full_key(path);
        let meta = self
            .store
            .head(&key)
            .await
            .map_err(|e| Error::Storage(format!("Azure head_with_hash {key}: {e}")))?;
        // object_store ObjectMeta exposes Azure's blob ETag.  Azure's
        // ETag is opaque (not Content-MD5) but stable across HEADs as
        // long as the blob hasn't been overwritten - sufficient for
        // bit-rot detection.  Could be upgraded to Content-MD5 via the
        // azure_storage_blobs SDK later if a deeper guarantee is
        // wanted.
        let etag = meta
            .e_tag
            .ok_or_else(|| Error::Storage(format!("Azure head_with_hash {key}: no ETag")))?;
        let etag = etag.trim_matches('"').to_string();
        Ok((meta.size as u64, etag, "azure-etag".into()))
    }

    fn display_name(&self) -> String {
        self.display.clone()
    }

    /// detect Archive-tier blobs that aren't yet rehydrated.
    /// Returns `Ok(false)` when the blob is in Archive tier (so the engine
    /// kicks off rehydration via `initiate_pack_restore` and waits).
    async fn probe_pack_accessible(&self, path: &str) -> Result<bool> {
        // Invert the archived flag - accessible is the boolean the
        // engine expects.  Probe errors degrade to "accessible" so a
        // glitch doesn't block a fully-Hot restore.
        match self.probe_pack_archived(path).await {
            Ok(archived) => Ok(!archived),
            Err(e) => {
                tracing::debug!(pack = path, error = %e,
                                "Azure probe_pack_accessible failed; assuming accessible");
                Ok(true)
            }
        }
    }

    /// rehydrate an Archive blob to Hot tier.
    /// Azure has no separate `RestoreObject`-style API; rehydration is
    /// triggered by changing the access tier.  Standard priority lands
    /// in ~15 h, High in ~1 h (caller pays the premium).  Reusing
    /// `rest_set_blob_tier` keeps signing logic in one place.
    async fn initiate_pack_restore(&self, path: &str) -> Result<()> {
        self.rest_set_blob_tier(path, "Hot").await.map_err(|e| {
            tracing::warn!(pack = path, error = %e,
                           "Azure initiate_pack_restore: set_tier(Hot) failed");
            e
        })
    }
}
