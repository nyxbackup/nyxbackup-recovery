// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! Backblaze B2 storage backend - native B2 API v2.
//!
//! # Authentication
//!
//! On construction the backend calls `b2_authorize_account` with the
//! `application_key_id` / `application_key` pair.  The response supplies an
//! ephemeral `authorizationToken` (valid ~24 h), the per-region `apiUrl` and
//! `downloadUrl`, and - for bucket-scoped keys - the `allowed.bucketId`.  For
//! master keys or multi-bucket keys, `b2_list_buckets` is called once to
//! resolve the bucket name to its ID.
//!
//! # Token refresh
//!
//! Every operation holds a snapshot of the current [`AuthState`].  When a
//! request returns HTTP 401 the backend re-authorizes once and retries.  A
//! `tokio::sync::RwLock` guards the shared state so concurrent operations
//! stall at most once during a refresh.
//!
//! # Upload URLs
//!
//! `b2_upload_file` requires a single-use upload URL obtained from
//! `b2_get_upload_url`.  A fresh URL is acquired before each upload.  The
//! extra round-trip (~50 ms) is negligible compared to transferring chunk
//! data; it avoids all upload-slot state management.
//!
//! # Deletes
//!
//! B2 is a versioned object store.  `delete` uses `b2_list_file_versions` to
//! enumerate every version of the named file and calls `b2_delete_file_version`
//! on each - leaving no storage behind.
//!
//! # put_if_absent
//!
//! B2 has no atomic conditional-create primitive.  The implementation checks
//! for existence first and skips the upload if the object is already present.
//! There is a small TOCTOU race window; for the backup engine's usage (lock
//! files, snapshot-index CAS) this is an acceptable trade-off.

use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{debug, instrument, warn};

use bkp_types::error::{Error, Result};

use crate::backend::StorageBackend;

// - Configuration -------------------------------

/// Configuration for the native Backblaze B2 backend.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct B2Config {
    /// B2 bucket name.
    pub bucket: String,
    /// Key prefix within the bucket (empty for root).
    #[serde(default)]
    pub prefix: String,
    /// B2 application key ID.
    pub application_key_id: String,
    /// B2 application key.
    pub application_key: String,
    /// Ignored - kept for backward compatibility with S3-based config files.
    #[serde(default)]
    pub region: Option<String>,
    /// Ignored - B2 does not use S3 storage classes.
    #[serde(default)]
    pub storage_class: Option<String>,
}

// - Auth state --------------------------------

#[derive(Debug, Clone)]
struct AuthState {
    /// Short-lived token included in every API request.
    token: String,
    /// Base URL for management API calls (b2_get_upload_url, b2_list_*, …).
    api_url: String,
    /// Base URL for downloads: `{download_url}/file/{bucket}/{key}`.
    download_url: String,
    /// B2 bucket ID - needed by b2_get_upload_url and b2_list_*.
    bucket_id: String,
    /// B2 account ID - needed by b2_update_bucket.
    account_id: String,
}

// - Serde helpers for API responses ---------------------

#[derive(serde::Deserialize)]
struct AuthorizeResponse {
    #[serde(rename = "accountId")]
    account_id: String,
    #[serde(rename = "authorizationToken")]
    authorization_token: String,
    #[serde(rename = "apiUrl")]
    api_url: String,
    #[serde(rename = "downloadUrl")]
    download_url: String,
    /// Present on bucket-scoped application keys.
    allowed: Option<AllowedBucket>,
}

#[derive(serde::Deserialize)]
struct AllowedBucket {
    #[serde(rename = "bucketId")]
    bucket_id: Option<String>,
}

#[derive(serde::Deserialize)]
struct ListBucketsResponse {
    buckets: Vec<BucketEntry>,
}
#[derive(serde::Deserialize)]
struct BucketEntry {
    #[serde(rename = "bucketId")]
    bucket_id: String,
    #[serde(rename = "bucketName")]
    bucket_name: String,
}

#[derive(serde::Deserialize)]
struct FileEntry {
    #[serde(rename = "fileName")]
    file_name: String,
    #[serde(rename = "contentLength")]
    content_length: Option<u64>,
    /// SHA-1 of the uploaded file body, hex-encoded.  B2 returns
    /// `"none"` for objects uploaded via the large-file API where the
    /// SHA-1 was not provided per part - those need a different
    /// integrity strategy (we currently single-part upload).
    #[serde(rename = "contentSha1", default)]
    content_sha1: Option<String>,
}

#[derive(serde::Deserialize)]
struct ListFileNamesResponse {
    files: Vec<FileEntry>,
    #[serde(rename = "nextFileName")]
    next_file_name: Option<String>,
}

#[derive(serde::Deserialize)]
struct B2ErrorResponse {
    status: u16,
    code: String,
    message: String,
}

// - B2Backend ---------------------------------

/// Backblaze B2 storage backend using the native B2 API v2.
pub struct B2Backend {
    client: reqwest::Client,
    key_id: String,
    app_key: String,
    bucket_name: String,
    prefix: String,
    auth: RwLock<AuthState>,
}

impl B2Backend {
    /// Construct a `B2Backend`, authorizing immediately.
    pub async fn new(cfg: B2Config) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|e| Error::Storage(format!("B2 HTTP client: {e}")))?;

        let auth = authorize(
            &client,
            &cfg.application_key_id,
            &cfg.application_key,
            &cfg.bucket,
        )
        .await?;

        Ok(Self {
            client,
            key_id: cfg.application_key_id,
            app_key: cfg.application_key,
            bucket_name: cfg.bucket,
            prefix: cfg.prefix,
            auth: RwLock::new(auth),
        })
    }

    fn full_key(&self, path: &str) -> String {
        let p = path.trim_start_matches('/');
        if self.prefix.is_empty() {
            p.to_string()
        } else {
            format!("{}/{}", self.prefix.trim_end_matches('/'), p)
        }
    }

    fn strip_prefix<'a>(&self, key: &'a str) -> &'a str {
        if self.prefix.is_empty() {
            return key;
        }
        let prefix_slash = format!("{}/", self.prefix.trim_end_matches('/'));
        key.strip_prefix(&prefix_slash).unwrap_or(key)
    }

    /// Read a snapshot of the current auth state.
    async fn get_auth(&self) -> AuthState {
        self.auth.read().await.clone()
    }

    /// Re-authorize and update shared state.  Called on HTTP 401.
    async fn reauth(&self) -> Result<AuthState> {
        let mut guard = self.auth.write().await;
        // Another task may have already refreshed while we waited for the write lock.
        // Re-authorizing again is harmless but wasteful; do it unconditionally since
        // we have no version counter to skip the duplicate.
        let fresh = authorize(&self.client, &self.key_id, &self.app_key, &self.bucket_name).await?;
        *guard = fresh.clone();
        Ok(fresh)
    }

    /// Return the B2 file name (full_key) and content-length for `path` if it
    /// exists, or `None` otherwise.
    async fn head_file(&self, path: &str, auth: &AuthState) -> Result<Option<FileEntry>> {
        let key = self.full_key(path);
        let url = format!("{}/b2api/v2/b2_list_file_names", auth.api_url);
        let resp = self
            .client
            .post(&url)
            .header("Authorization", &auth.token)
            .json(&serde_json::json!({
                "bucketId": auth.bucket_id,
                "prefix": key,
                "maxFileCount": 1,
            }))
            .send()
            .await
            .map_err(|e| Error::Storage(format!("B2 head_file: {e}")))?;

        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| Error::Storage(format!("B2 head_file body: {e}")))?;
        if !status.is_success() {
            return Err(b2_error("head_file", status.as_u16(), &body));
        }
        let list: ListFileNamesResponse = serde_json::from_str(&body)
            .map_err(|e| Error::Storage(format!("B2 head_file parse: {e}")))?;

        // The prefix filter is a prefix, not an exact match - verify the name.
        Ok(list.files.into_iter().find(|f| f.file_name == key))
    }
}

// - StorageBackend impl ----------------------------

#[async_trait::async_trait]
impl StorageBackend for B2Backend {
    #[instrument(skip(self), fields(b2_bucket = %self.bucket_name, key = path))]
    async fn get(&self, path: &str) -> Result<Vec<u8>> {
        let key = self.full_key(path);
        debug!("B2 get {key}");
        self.get_range_impl(&key, None).await
    }

    #[instrument(skip(self), fields(b2_bucket = %self.bucket_name, key = path, from, to))]
    async fn get_range(&self, path: &str, from: u64, to: u64) -> Result<Vec<u8>> {
        let key = self.full_key(path);
        debug!("B2 get_range {key} [{from}..{to})");
        self.get_range_impl(&key, Some((from, to))).await
    }

    // See StorageBackend::probe_access: a single cheap authed round trip via
    // exists("").  Both Ok(true)/Ok(false) mean reachable + authenticated;
    // only a real connect/auth error propagates.  No content enumeration.
    async fn probe_access(&self) -> Result<()> {
        self.exists("").await.map(|_| ())
    }

    #[instrument(skip(self), fields(b2_bucket = %self.bucket_name, key = path))]
    async fn exists(&self, path: &str) -> Result<bool> {
        for attempt in 0..2u8 {
            let auth = self.get_auth().await;
            match self.head_file(path, &auth).await {
                Ok(entry) => return Ok(entry.is_some()),
                Err(Error::Storage(msg)) if msg.contains("401") && attempt == 0 => {
                    warn!("B2: 401 on exists, re-authorizing.");
                    self.reauth().await?;
                }
                Err(e) => return Err(e),
            }
        }
        unreachable!()
    }

    #[instrument(skip(self), fields(b2_bucket = %self.bucket_name, prefix = prefix))]
    async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        let full_prefix = self.full_key(prefix);
        debug!("B2 list {full_prefix}");

        for attempt in 0..2u8 {
            let auth = self.get_auth().await;
            match list_all_names(&self.client, &auth, &full_prefix).await {
                Err(Error::Storage(msg)) if msg.contains("401") && attempt == 0 => {
                    warn!("B2: 401 on list, re-authorizing.");
                    self.reauth().await?;
                }
                Err(e) => return Err(e),
                Ok(entries) => {
                    return Ok(entries
                        .into_iter()
                        .map(|e| self.strip_prefix(&e.file_name).to_string())
                        .collect());
                }
            }
        }
        unreachable!()
    }

    #[instrument(skip(self), fields(b2_bucket = %self.bucket_name, key = path))]
    async fn size(&self, path: &str) -> Result<u64> {
        for attempt in 0..2u8 {
            let auth = self.get_auth().await;
            match self.head_file(path, &auth).await {
                Ok(Some(entry)) => {
                    return entry.content_length.ok_or_else(|| {
                        Error::Storage(format!("B2 size: no contentLength for {path}"))
                    });
                }
                Ok(None) => return Err(Error::Storage(format!("B2 size: not found: {path}"))),
                Err(Error::Storage(msg)) if msg.contains("401") && attempt == 0 => {
                    warn!("B2: 401 on size, re-authorizing.");
                    self.reauth().await?;
                }
                Err(e) => return Err(e),
            }
        }
        unreachable!()
    }

    async fn head_with_hash(&self, path: &str) -> Result<(u64, String, String)> {
        for attempt in 0..2u8 {
            let auth = self.get_auth().await;
            match self.head_file(path, &auth).await {
                Ok(Some(entry)) => {
                    let size = entry.content_length.ok_or_else(|| {
                        Error::Storage(format!("B2 head_with_hash: no contentLength for {path}"))
                    })?;
                    // B2 returns SHA-1 of the uploaded file body in
                    // contentSha1.  Large-file uploads carry "none"
                    // (large_file_sha1 lives in fileInfo); we currently
                    // only single-part upload so this is always set.
                    let sha1 = entry.content_sha1.ok_or_else(|| {
                        Error::Storage(format!("B2 head_with_hash: no contentSha1 for {path}"))
                    })?;
                    if sha1 == "none" {
                        return Err(Error::Storage(format!(
                            "B2 head_with_hash: contentSha1=none (large-file upload) for {path}"
                        )));
                    }
                    return Ok((size, sha1, "sha1".into()));
                }
                Ok(None) => {
                    return Err(Error::Storage(format!(
                        "B2 head_with_hash: not found: {path}"
                    )));
                }
                Err(Error::Storage(msg)) if msg.contains("401") && attempt == 0 => {
                    warn!("B2: 401 on head_with_hash, re-authorizing.");
                    self.reauth().await?;
                }
                Err(e) => return Err(e),
            }
        }
        unreachable!()
    }

    fn display_name(&self) -> String {
        if self.prefix.is_empty() {
            format!("b2://{}", self.bucket_name)
        } else {
            format!("b2://{}/{}", self.bucket_name, self.prefix)
        }
    }

    fn concurrency_hint(&self) -> Option<usize> {
        // Backblaze B2 caps Class C transactions per-account and applies
        // per-bucket throttling; default-8 restores from residential links
        // tend to trip 503 backoff.  4 is the empirical sweet spot.
        Some(4)
    }

    /// Backblaze B2 has no equivalent of S3
    /// `RestoreObject` / Azure `SetBlobTier(Hot)` - the B2 native API
    /// keeps every file immediately readable while it exists.  Bucket
    /// lifecycle rules ("hide files after N days", "delete after N days")
    /// either keep the file fully accessible or delete it outright;
    /// there is no intermediate archived-but-thaw-able state to detect.
    ///
    /// So the trait defaults are *actually correct* for B2:
    ///   - `probe_pack_accessible` always returns true (the file either
    ///     exists and is readable, or doesn't exist at all and the
    ///     subsequent `get_range` surfaces a 404).
    ///   - `initiate_pack_restore` returning Ok(()) is a true no-op.
    ///
    /// We override them anyway with explicit docs so the next person
    /// auditing doesn't mistake "default impl" for "TODO".
    /// If B2 ever adds a real archive tier the explicit overrides are
    /// the right place to wire it.
    async fn probe_pack_accessible(&self, _path: &str) -> Result<bool> {
        Ok(true)
    }
    async fn initiate_pack_restore(&self, _path: &str) -> Result<()> {
        Ok(())
    }
}

// - B2Backend helper methods -------------------------

impl B2Backend {
    /// Core download implementation - `range` is `Some((from, to))` for partial reads.
    async fn get_range_impl(&self, key: &str, range: Option<(u64, u64)>) -> Result<Vec<u8>> {
        for attempt in 0..2u8 {
            let auth = self.get_auth().await;
            let url = format!(
                "{}/file/{}/{}",
                auth.download_url,
                self.bucket_name,
                url_encode_filename(key),
            );

            let mut req = self.client.get(&url).header("Authorization", &auth.token);
            if let Some((from, to)) = range {
                req = req.header("Range", format!("bytes={}-{}", from, to - 1));
            }

            let resp = req
                .send()
                .await
                .map_err(|e| Error::Storage(format!("B2 download {key}: {e}")))?;
            let status = resp.status();

            if status.as_u16() == 401 && attempt == 0 {
                warn!("B2: 401 on download, re-authorizing.");
                self.reauth().await?;
                continue;
            }
            if status.as_u16() == 404 {
                return Err(Error::Storage(format!("B2: not found: {key}")));
            }
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(b2_error(&format!("download {key}"), status.as_u16(), &body));
            }
            let bytes = resp
                .bytes()
                .await
                .map_err(|e| Error::Storage(format!("B2 download body {key}: {e}")))?;
            return Ok(bytes.to_vec());
        }
        unreachable!()
    }
}

// - Free functions ------------------------------

/// Call `b2_authorize_account` and, if needed, resolve the bucket ID via
/// `b2_list_buckets`.
async fn authorize(
    client: &reqwest::Client,
    key_id: &str,
    app_key: &str,
    bucket_name: &str,
) -> Result<AuthState> {
    use base64::Engine as _;
    let creds = base64::engine::general_purpose::STANDARD.encode(format!("{key_id}:{app_key}"));

    let resp = client
        .get("https://api.backblazeb2.com/b2api/v2/b2_authorize_account")
        .header("Authorization", format!("Basic {creds}"))
        .send()
        .await
        .map_err(|e| Error::Storage(format!("B2 authorize: {e}")))?;

    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| Error::Storage(format!("B2 authorize body: {e}")))?;
    if !status.is_success() {
        return Err(b2_error("authorize", status.as_u16(), &body));
    }

    let auth: AuthorizeResponse = serde_json::from_str(&body)
        .map_err(|e| Error::Storage(format!("B2 authorize parse: {e}")))?;

    // Prefer the bucket ID embedded in the auth response (bucket-scoped key).
    let bucket_id = if let Some(id) = auth.allowed.as_ref().and_then(|a| a.bucket_id.clone()) {
        id
    } else {
        // Master key or multi-bucket key - look up by name.
        resolve_bucket_id(
            client,
            &auth.authorization_token,
            &auth.api_url,
            &auth.account_id,
            bucket_name,
        )
        .await?
    };

    Ok(AuthState {
        token: auth.authorization_token,
        api_url: auth.api_url,
        download_url: auth.download_url,
        bucket_id,
        account_id: auth.account_id,
    })
}

/// Call `b2_list_buckets` to find the `bucketId` matching `bucket_name`.
async fn resolve_bucket_id(
    client: &reqwest::Client,
    token: &str,
    api_url: &str,
    account_id: &str,
    bucket_name: &str,
) -> Result<String> {
    let url = format!("{api_url}/b2api/v2/b2_list_buckets");
    let resp = client
        .post(&url)
        .header("Authorization", token)
        .json(&serde_json::json!({ "accountId": account_id, "bucketName": bucket_name }))
        .send()
        .await
        .map_err(|e| Error::Storage(format!("B2 list_buckets: {e}")))?;

    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| Error::Storage(format!("B2 list_buckets body: {e}")))?;
    if !status.is_success() {
        return Err(b2_error("list_buckets", status.as_u16(), &body));
    }

    let list: ListBucketsResponse = serde_json::from_str(&body)
        .map_err(|e| Error::Storage(format!("B2 list_buckets parse: {e}")))?;

    list.buckets
        .into_iter()
        .find(|b| b.bucket_name == bucket_name)
        .map(|b| b.bucket_id)
        .ok_or_else(|| Error::Storage(format!("B2: bucket not found: {bucket_name}")))
}

/// Paginate `b2_list_file_names` and return all `FileEntry` records under
/// `prefix`.
async fn list_all_names(
    client: &reqwest::Client,
    auth: &AuthState,
    prefix: &str,
) -> Result<Vec<FileEntry>> {
    let url = format!("{}/b2api/v2/b2_list_file_names", auth.api_url);
    let mut files: Vec<FileEntry> = Vec::new();
    let mut next: Option<String> = None;

    loop {
        let mut body = serde_json::json!({
            "bucketId": auth.bucket_id,
            "prefix": prefix,
            "maxFileCount": 1000,
        });
        if let Some(ref start) = next {
            body["startFileName"] = serde_json::Value::String(start.clone());
        }

        let resp = client
            .post(&url)
            .header("Authorization", &auth.token)
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Storage(format!("B2 list_file_names: {e}")))?;

        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| Error::Storage(format!("B2 list_file_names body: {e}")))?;
        if !status.is_success() {
            return Err(b2_error("list_file_names", status.as_u16(), &text));
        }

        let page: ListFileNamesResponse = serde_json::from_str(&text)
            .map_err(|e| Error::Storage(format!("B2 list_file_names parse: {e}")))?;

        let more = page.next_file_name.clone();
        files.extend(page.files);
        match more {
            Some(n) => next = Some(n),
            None => break,
        }
    }
    Ok(files)
}

// - Utilities ---------------------------------

/// Percent-encode a B2 file name for use in URL paths and X-Bz-File-Name headers.
///
/// B2 requires that the file name be UTF-8 percent-encoded; spaces become `%20`
/// (not `+`).  Slashes `/` must NOT be encoded so directory prefixes work.
fn url_encode_filename(name: &str) -> String {
    name.chars()
        .flat_map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~' | '/') {
                vec![c.to_string()]
            } else {
                c.to_string().bytes().map(|b| format!("%{b:02X}")).collect()
            }
        })
        .collect()
}

/// Build a user-visible `Error::Storage` from a B2 error response body.
fn b2_error(op: &str, status: u16, body: &str) -> Error {
    let detail = serde_json::from_str::<B2ErrorResponse>(body)
        .map(|e| format!("{} ({}): {}", e.code, e.status, e.message))
        .unwrap_or_else(|_| format!("HTTP {status}: {body}"));
    Error::Storage(format!("B2 {op}: {detail}"))
}

// - Lifecycle-rule management --------------------

impl B2Backend {
    /// Apply the "Keep only the last version of the file" lifecycle
    /// rule to the configured bucket.  See `apply_keep_last_version_lifecycle`
    /// in the trait docs for rationale.  This calls `b2_update_bucket`
    /// with a single lifecycle rule covering all files in the bucket
    /// (`fileNamePrefix=""`).
    ///
    /// Returns Ok on success.  Requires the application key to have
    /// `writeBucketSettings` capability; B2 returns a clear error
    /// otherwise.
    pub async fn apply_keep_last_version_lifecycle(&self) -> Result<()> {
        let auth = self.get_auth().await;
        let url = format!("{}/b2api/v2/b2_update_bucket", auth.api_url);
        let body = serde_json::json!({
            "accountId":     auth.account_id,
            "bucketId":      auth.bucket_id,
            "lifecycleRules": [
                {
                    // Apply to every file in the bucket.
                    "fileNamePrefix":          "",
                    // null = do not auto-hide based on age; only when
                    // overwritten by a newer version with the same name.
                    "daysFromUploadingToHiding": serde_json::Value::Null,
                    // Delete hidden files after 1 day.  Together with
                    // daysFromUploadingToHiding=null this is B2's
                    // "Keep only the last version" preset.
                    "daysFromHidingToDeleting": 1,
                },
            ],
        });
        let resp = self
            .client
            .post(&url)
            .header("Authorization", &auth.token)
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Storage(format!("B2 update_bucket request: {e}")))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(b2_error("update_bucket", status.as_u16(), &body));
        }
        Ok(())
    }
}
