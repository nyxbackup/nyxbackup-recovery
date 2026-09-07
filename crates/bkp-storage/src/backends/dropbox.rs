// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! Dropbox storage backend - Dropbox API v2.
//!
//! # Storage model
//!
//! Backup objects are stored using their natural path structure under the
//! configured root folder.  `packs/abc123.pack` is stored at
//! `${folder_path}/packs/abc123.pack` in Dropbox.  Intermediate folders are
//! created automatically by the Dropbox API.
//!
//! # Authentication
//!
//! Uses OAuth 2.0 with long-lived offline refresh tokens (Dropbox API v2).
//! `app_key`, `app_secret`, and `refresh_token` are required.  Refresh tokens
//! for Dropbox offline access do not expire unless explicitly revoked.
//!
//! Access tokens are cached and proactively refreshed 5 minutes before expiry.
//!
//! # Uploads
//!
//! Files up to 150 MB use a single-request upload (`files/upload`).  Pack files
//! are at most 16 MB so no upload session logic is needed.
//!
//! # put_if_absent
//!
//! `files/upload` with `mode: {".tag": "add"}` returns HTTP 409 when the file
//! already exists, providing an atomic conditional create.

use std::sync::Arc;
use std::time::Duration;

use bkp_types::error::{Error, Result};
use serde::Deserialize;
use tracing::{debug, instrument};

use super::oauth::TokenCache;
use crate::backend::StorageBackend;

const DBX_API: &str = "https://api.dropboxapi.com/2";
const DBX_CONTENT: &str = "https://content.dropboxapi.com/2";
const DBX_TOKEN_URL: &str = "https://api.dropboxapi.com/oauth2/token";

// Compiled in from DROPBOX_APP_KEY / DROPBOX_APP_SECRET (empty in dev builds).
const BUNDLED_APP_KEY: &str = match option_env!("DROPBOX_APP_KEY") {
    Some(s) => s,
    None => "",
};
const BUNDLED_APP_SECRET: &str = match option_env!("DROPBOX_APP_SECRET") {
    Some(s) => s,
    None => "",
};

// - Configuration -------------------------------

/// Configuration for the Dropbox backend.
#[derive(Debug, Clone, Deserialize)]
pub struct DropboxConfig {
    /// Root folder path in Dropbox, e.g. `/NyxBackup`.
    pub folder_path: String,
    /// Dropbox App key (the OAuth client_id).  Empty = use bundled Nyx Backup app key.
    #[serde(default)]
    pub app_key: String,
    /// Dropbox App secret.  Empty = use bundled Nyx Backup app secret.
    #[serde(default)]
    pub app_secret: String,
    /// Long-lived offline refresh token.
    pub refresh_token: String,
}

// - API response types ----------------------------

#[derive(Deserialize)]
struct FileMetadata {
    name: String,
    path_lower: Option<String>,
    size: Option<u64>,
    /// Dropbox's per-file content hash.  Their
    /// custom algorithm splits the file into 4 MiB blocks, SHA-256s
    /// each block, concatenates the digests, then SHA-256s the
    /// concatenation.  Always present on regular uploaded files;
    /// missing only on shared-link-pseudo-files or some legacy items.
    /// Reference: <https://www.dropbox.com/developers/reference/content-hash>.
    #[serde(default)]
    content_hash: Option<String>,
}

#[derive(Deserialize)]
struct ListFolderResult {
    entries: Vec<DropboxEntry>,
    cursor: String,
    has_more: bool,
}

#[derive(Deserialize)]
#[serde(tag = ".tag")]
enum DropboxEntry {
    #[serde(rename = "file")]
    File(FileMetadata),
    #[serde(rename = "folder")]
    Folder {},
    #[serde(rename = "deleted")]
    Deleted {},
}

#[derive(Deserialize)]
struct ListFolderContinueResult {
    entries: Vec<DropboxEntry>,
    cursor: String,
    has_more: bool,
}

// - Backend ----------------------------------

/// Dropbox storage backend.
pub struct DropboxBackend {
    client: reqwest::Client,
    /// Root folder path, e.g. `/NyxBackup`.
    folder_path: String,
    tokens: Arc<TokenCache>,
}

impl DropboxBackend {
    /// Construct a new backend.  Does not make any network calls.
    pub fn new(cfg: DropboxConfig) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(300))
            .build()
            .map_err(|e| Error::Storage(format!("Dropbox HTTP client: {e}")))?;

        let app_key = if cfg.app_key.is_empty() {
            super::oauth::cred_env_or("DROPBOX_APP_KEY", BUNDLED_APP_KEY)
        } else {
            cfg.app_key
        };
        let app_secret = if cfg.app_secret.is_empty() {
            super::oauth::cred_env_or("DROPBOX_APP_SECRET", BUNDLED_APP_SECRET)
        } else {
            cfg.app_secret
        };

        // Fail loudly when no app_key is available - either the daemon
        // was built without DROPBOX_APP_KEY in the env (so BUNDLED_APP_KEY
        // resolved to "" via option_env!) AND the endpoint config doesn't
        // supply one.  Sending the OAuth refresh request with client_id=""
        // results in Dropbox replying 400 "invalid_client: Invalid
        // client_id", a message that gives the operator no clue about
        // the actual cause; explicit error here costs nothing and saves
        // hours of head-scratching.  Matches the OneDrive backend's
        // identical guard.
        if app_key.is_empty() {
            return Err(Error::Storage(
                "Dropbox app_key is empty.  Build the daemon with \
                 DROPBOX_APP_KEY set in the environment (the .env file \
                 at the workspace root, or an explicit `export`), or \
                 supply app_key in the endpoint config."
                    .to_string(),
            ));
        }
        if app_secret.is_empty() {
            return Err(Error::Storage(
                "Dropbox app_secret is empty.  Build the daemon with \
                 DROPBOX_APP_SECRET set in the environment, or supply \
                 app_secret in the endpoint config."
                    .to_string(),
            ));
        }

        let tokens = Arc::new(TokenCache::new(
            DBX_TOKEN_URL,
            app_key,
            app_secret,
            cfg.refresh_token,
            vec![],
        ));

        let root = cfg.folder_path.trim_matches('/').to_string();
        let root = format!("/{root}");

        Ok(Self {
            client,
            folder_path: root,
            tokens,
        })
    }

    // - Path helpers ---------------------------

    fn full_path(&self, path: &str) -> String {
        if path.is_empty() {
            self.folder_path.clone()
        } else {
            format!("{}/{}", self.folder_path, path.trim_start_matches('/'))
        }
    }

    fn full_folder(&self, prefix: &str) -> String {
        let trimmed = prefix.trim_end_matches('/');
        if trimmed.is_empty() {
            self.folder_path.clone()
        } else {
            format!("{}/{}", self.folder_path, trimmed)
        }
    }

    // - Upload ------------------------------

    // - List -------------------------------

    async fn list_folder_all(
        &self,
        folder: &str,
        recursive: bool,
        token: &str,
    ) -> Result<Vec<(String, u64)>> {
        let resp = self
            .client
            .post(format!("{DBX_API}/files/list_folder"))
            .bearer_auth(token)
            .json(&serde_json::json!({
                "path": folder,
                "recursive": recursive,
                "include_deleted": false,
                "include_media_info": false,
                "limit": 2000,
            }))
            .send()
            .await
            .map_err(|e| Error::Storage(format!("Dropbox list_folder: {e}")))?;

        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| Error::Storage(format!("Dropbox list_folder body: {e}")))?;

        if status.as_u16() == 409 {
            // Folder doesn't exist - return empty.
            return Ok(Vec::new());
        }
        if !status.is_success() {
            return Err(Error::Storage(format!(
                "Dropbox list_folder ({status}): {body}"
            )));
        }

        let result: ListFolderResult = serde_json::from_str(&body)
            .map_err(|e| Error::Storage(format!("Dropbox list_folder parse: {e}")))?;

        let mut entries = Self::extract_files(&result.entries, &self.folder_path);
        let mut cursor = result.cursor;
        let mut has_more = result.has_more;

        while has_more {
            let cont_resp = self
                .client
                .post(format!("{DBX_API}/files/list_folder/continue"))
                .bearer_auth(token)
                .json(&serde_json::json!({ "cursor": cursor }))
                .send()
                .await
                .map_err(|e| Error::Storage(format!("Dropbox list_folder/continue: {e}")))?;

            let status = cont_resp.status();
            let body = cont_resp
                .text()
                .await
                .map_err(|e| Error::Storage(format!("Dropbox list_folder/continue body: {e}")))?;
            if !status.is_success() {
                return Err(Error::Storage(format!(
                    "Dropbox list_folder/continue ({status}): {body}"
                )));
            }
            let cont: ListFolderContinueResult = serde_json::from_str(&body)
                .map_err(|e| Error::Storage(format!("Dropbox list_folder/continue parse: {e}")))?;

            entries.extend(Self::extract_files(&cont.entries, &self.folder_path));
            cursor = cont.cursor;
            has_more = cont.has_more;
        }

        Ok(entries)
    }

    fn extract_files(entries: &[DropboxEntry], root: &str) -> Vec<(String, u64)> {
        let root_lower = root.to_lowercase();
        entries
            .iter()
            .filter_map(|e| match e {
                DropboxEntry::File(f) => {
                    let path_lower = f.path_lower.as_deref().unwrap_or("");
                    // Strip root prefix to get relative path.
                    let rel = if path_lower.starts_with(&root_lower) {
                        path_lower[root_lower.len()..].trim_start_matches('/')
                    } else {
                        &f.name
                    };
                    Some((rel.to_string(), f.size.unwrap_or(0)))
                }
                _ => None,
            })
            .collect()
    }

    /// Retry helper: on 401 force-refresh and retry once.
    async fn with_token<F, Fut, T>(&self, op: F) -> Result<T>
    where
        F: Fn(String) -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        for attempt in 0..2u8 {
            let token = self.tokens.get_token(&self.client).await?;
            match op(token).await {
                Err(Error::Storage(msg)) if msg.contains("401") && attempt == 0 => {
                    self.tokens.force_refresh(&self.client).await?;
                }
                other => return other,
            }
        }
        unreachable!()
    }
}

// - StorageBackend impl ----------------------------

#[async_trait::async_trait]
impl StorageBackend for DropboxBackend {
    #[instrument(skip(self), fields(dropbox_root = %self.folder_path, path))]
    async fn get(&self, path: &str) -> Result<Vec<u8>> {
        debug!("Dropbox get {path}");
        let dbx_path = self.full_path(path);
        let arg = serde_json::json!({ "path": dbx_path }).to_string();
        self.with_token(|token| {
            let arg = arg.clone();
            let path = path.to_string();
            async move {
                let resp = self
                    .client
                    .post(format!("{DBX_CONTENT}/files/download"))
                    .bearer_auth(&token)
                    .header("Dropbox-API-Arg", &arg)
                    .send()
                    .await
                    .map_err(|e| Error::Storage(format!("Dropbox get {path}: {e}")))?;
                let status = resp.status();
                if status.as_u16() == 409 {
                    return Err(Error::Storage(format!("Dropbox: not found: {path}")));
                }
                if !status.is_success() {
                    let body = resp.text().await.unwrap_or_default();
                    return Err(Error::Storage(format!(
                        "Dropbox get {path} ({status}): {body}"
                    )));
                }
                resp.bytes()
                    .await
                    .map(|b| b.to_vec())
                    .map_err(|e| Error::Storage(format!("Dropbox get {path} body: {e}")))
            }
        })
        .await
    }

    #[instrument(skip(self), fields(dropbox_root = %self.folder_path, path, from, to))]
    async fn get_range(&self, path: &str, from: u64, to: u64) -> Result<Vec<u8>> {
        // Dropbox supports HTTP Range on /files/download.  Without this
        // override the trait default fell back to full-pack download per
        // chunk read - same footgun that broke S3CompatBackend restores.
        debug!("Dropbox get_range {path} [{from}..{to}]");
        let dbx_path = self.full_path(path);
        let arg = serde_json::json!({ "path": dbx_path }).to_string();
        // Dropbox honours the standard `Range: bytes=from-(to-1)` header.
        // The restore engine passes a half-open [from, to) range so the
        // inclusive HTTP form is `to - 1`.  Empty ranges (to <= from)
        // would short-circuit to an empty Vec for callers; the engine
        // never sends those, but guard anyway.
        if to <= from {
            return Ok(Vec::new());
        }
        let range_header = format!("bytes={}-{}", from, to - 1);
        self.with_token(|token| {
            let arg = arg.clone();
            let path = path.to_string();
            let range_header = range_header.clone();
            async move {
                let resp = self
                    .client
                    .post(format!("{DBX_CONTENT}/files/download"))
                    .bearer_auth(&token)
                    .header("Dropbox-API-Arg", &arg)
                    .header("Range", &range_header)
                    .send()
                    .await
                    .map_err(|e| Error::Storage(format!("Dropbox get_range {path}: {e}")))?;
                let status = resp.status();
                if status.as_u16() == 409 {
                    return Err(Error::Storage(format!("Dropbox: not found: {path}")));
                }
                if !status.is_success() {
                    let body = resp.text().await.unwrap_or_default();
                    return Err(Error::Storage(format!(
                        "Dropbox get_range {path} ({status}): {body}"
                    )));
                }
                resp.bytes()
                    .await
                    .map(|b| b.to_vec())
                    .map_err(|e| Error::Storage(format!("Dropbox get_range {path} body: {e}")))
            }
        })
        .await
    }

    fn concurrency_hint(&self) -> Option<usize> {
        // Dropbox rate-limits aggressively per-token; 4 concurrent
        // requests is the practical sweet spot for restore throughput
        // without triggering 429 backoff.
        Some(4)
    }

    #[instrument(skip(self), fields(dropbox_root = %self.folder_path, path))]
    // See StorageBackend::probe_access: a single cheap authed round trip via
    // exists("") - one files/get_metadata on the configured folder (driving
    // the token refresh via with_token), not the recursive list("").  Both
    // Ok(true)/Ok(false) mean reachable + authenticated.
    async fn probe_access(&self) -> Result<()> {
        self.exists("").await.map(|_| ())
    }

    async fn exists(&self, path: &str) -> Result<bool> {
        let dbx_path = self.full_path(path);
        self.with_token(|token| {
            let dbx_path = dbx_path.clone();
            async move {
                let resp = self
                    .client
                    .post(format!("{DBX_API}/files/get_metadata"))
                    .bearer_auth(&token)
                    .json(&serde_json::json!({ "path": dbx_path }))
                    .send()
                    .await
                    .map_err(|e| Error::Storage(format!("Dropbox exists: {e}")))?;
                Ok(resp.status().is_success())
            }
        })
        .await
    }

    #[instrument(skip(self), fields(dropbox_root = %self.folder_path, prefix))]
    async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        debug!("Dropbox list {prefix:?}");
        let folder = self.full_folder(prefix);
        // Always recurse: `StorageBackend::list` must return ALL object keys
        // under `prefix` (the contract object stores honour).  Snapshot
        // discovery lists `indexes/` and expects the nested
        // `indexes/<set-id>/snapshot-index` keys; a non-recursive list returns
        // only the `<set-id>` folder entries (which extract_files drops),
        // leaving discovery empty - the same bug WebDAV had.
        let recursive = true;

        self.with_token(|token| {
            let folder = folder.clone();
            async move {
                let entries = self.list_folder_all(&folder, recursive, &token).await?;
                // extract_files() already returns root-relative paths
                // (e.g. "packs/<uuid>.pack" when listing the packs subfolder),
                // so we return them as-is.  An earlier non-recursive branch
                // prepended `prefix_dir` here, producing "packs/packs/<uuid>.pack"
                // double-prefix paths that parse_pack_id_from_path then failed
                // to parse silently - find_unmanifested_packs returned empty
                // and orphan packs accumulated indefinitely after every reset.
                Ok(entries.into_iter().map(|(p, _)| p).collect())
            }
        })
        .await
    }

    #[instrument(skip(self), fields(dropbox_root = %self.folder_path, prefix))]
    async fn list_with_sizes(&self, prefix: &str) -> Result<Vec<(String, u64)>> {
        let folder = self.full_folder(prefix);
        let recursive = prefix.is_empty();

        self.with_token(|token| {
            let folder = folder.clone();
            async move {
                // Same root-relative invariant as `list` above; no prefix
                // re-prepending needed.  See list() comment for context.
                self.list_folder_all(&folder, recursive, &token).await
            }
        })
        .await
    }

    /// content-verifying HEAD for Dropbox.
    /// Returns Dropbox's `content_hash` (their custom 4 MiB-block-
    /// SHA-256 algorithm).  Recorded baseline + audited values
    /// both use the same algorithm so comparisons are stable.
    #[instrument(skip(self), fields(dropbox_root = %self.folder_path, path))]
    async fn head_with_hash(&self, path: &str) -> Result<(u64, String, String)> {
        let dbx_path = self.full_path(path);
        self.with_token(|token| {
            let dbx_path = dbx_path.clone();
            let path = path.to_string();
            async move {
                let resp = self
                    .client
                    .post(format!("{DBX_API}/files/get_metadata"))
                    .bearer_auth(&token)
                    .json(&serde_json::json!({ "path": dbx_path }))
                    .send()
                    .await
                    .map_err(|e| Error::Storage(format!("Dropbox head_with_hash {path}: {e}")))?;
                let status = resp.status();
                if status.as_u16() == 409 {
                    return Err(Error::Storage(format!(
                        "Dropbox head_with_hash: not found: {path}"
                    )));
                }
                let body = resp
                    .text()
                    .await
                    .map_err(|e| Error::Storage(format!("Dropbox head_with_hash body: {e}")))?;
                if !status.is_success() {
                    return Err(Error::Storage(format!(
                        "Dropbox head_with_hash {path} ({status}): {body}"
                    )));
                }
                let meta: FileMetadata = serde_json::from_str(&body)
                    .map_err(|e| Error::Storage(format!("Dropbox head_with_hash parse: {e}")))?;
                let size = meta.size.ok_or_else(|| {
                    Error::Storage(format!("Dropbox head_with_hash: no size for {path}"))
                })?;
                let hash = meta.content_hash.ok_or_else(|| {
                    Error::Storage(format!(
                        "Dropbox head_with_hash: no content_hash for {path}"
                    ))
                })?;
                Ok((size, hash, "dropbox-content-hash".to_string()))
            }
        })
        .await
    }

    #[instrument(skip(self), fields(dropbox_root = %self.folder_path, path))]
    async fn size(&self, path: &str) -> Result<u64> {
        let dbx_path = self.full_path(path);
        self.with_token(|token| {
            let dbx_path = dbx_path.clone();
            let path = path.to_string();
            async move {
                let resp = self
                    .client
                    .post(format!("{DBX_API}/files/get_metadata"))
                    .bearer_auth(&token)
                    .json(&serde_json::json!({ "path": dbx_path }))
                    .send()
                    .await
                    .map_err(|e| Error::Storage(format!("Dropbox size {path}: {e}")))?;
                let status = resp.status();
                if status.as_u16() == 409 {
                    return Err(Error::Storage(format!("Dropbox size: not found: {path}")));
                }
                let body = resp
                    .text()
                    .await
                    .map_err(|e| Error::Storage(format!("Dropbox size body: {e}")))?;
                if !status.is_success() {
                    return Err(Error::Storage(format!(
                        "Dropbox size {path} ({status}): {body}"
                    )));
                }
                let meta: FileMetadata = serde_json::from_str(&body)
                    .map_err(|e| Error::Storage(format!("Dropbox size parse: {e}")))?;
                meta.size.ok_or_else(|| {
                    Error::Storage(format!("Dropbox size: no size field for {path}"))
                })
            }
        })
        .await
    }

    fn display_name(&self) -> String {
        format!("dropbox:{}", self.folder_path)
    }
}
