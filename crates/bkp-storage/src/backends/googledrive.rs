// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! Google Drive storage backend - Drive REST API v3.
//!
//! # Storage model
//!
//! All backup objects are stored as flat files inside a single configured Drive
//! folder.  Object paths (e.g. `packs/abc123.pack`) become the Drive file name
//! verbatim; Drive accepts slashes in file names so no encoding is needed.
//!
//! # Authentication
//!
//! Uses OAuth 2.0 offline access.  `client_id` + `client_secret` + `refresh_token`
//! are required.  Access tokens are cached and proactively refreshed 5 minutes
//! before expiry; on HTTP 401 a forced refresh is performed before retrying.
//!
//! # Uploads
//!
//! All uploads use the Drive resumable-upload protocol (two-step: initiate then
//! transfer) regardless of file size.  This handles pack files up to the Drive
//! per-file limit (5 TB) without a separate code path for large vs small files.
//!
//! # put_if_absent
//!
//! Drive has no atomic conditional-create primitive.  The implementation checks
//! for existence first and creates on miss.  There is a small TOCTOU window -
//! acceptable for the engine's lock-file and snapshot-index CAS usage.

use std::sync::Arc;
use std::time::Duration;

use bkp_types::error::{Error, Result};
use serde::Deserialize;
use tracing::{debug, instrument};

use super::oauth::TokenCache;
use crate::backend::StorageBackend;

const DRIVE_API: &str = "https://www.googleapis.com/drive/v3";
const DRIVE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

// Compiled in from GOOGLE_OAUTH_CLIENT_ID / GOOGLE_OAUTH_CLIENT_SECRET (empty in dev builds).
const BUNDLED_CLIENT_ID: &str = match option_env!("GOOGLE_OAUTH_CLIENT_ID") {
    Some(s) => s,
    None => "",
};
const BUNDLED_CLIENT_SECRET: &str = match option_env!("GOOGLE_OAUTH_CLIENT_SECRET") {
    Some(s) => s,
    None => "",
};

// - Configuration -------------------------------

/// Configuration for the Google Drive backend.
///
/// Only `folder_id` and `refresh_token` are required in config; `client_id` and
/// `client_secret` default to Nyx Backup's bundled app credentials when absent.
#[derive(Debug, Clone, Deserialize)]
pub struct GoogleDriveConfig {
    /// Google Drive folder ID (the long alphanumeric string from the folder URL).
    pub folder_id: String,
    /// Long-lived refresh token obtained via the OAuth authorization flow.
    pub refresh_token: String,
    /// Override client ID (falls back to Nyx Backup's bundled app credentials when empty).
    #[serde(default)]
    pub client_id: String,
    /// Override client secret (falls back to Nyx Backup's bundled app credentials when empty).
    #[serde(default)]
    pub client_secret: String,
}

// - API response types ----------------------------

#[derive(Debug, Deserialize)]
struct DriveFile {
    id: String,
    name: String,
    size: Option<String>, // Drive returns size as a string
    /// MD5 of the binary content, populated for
    /// files uploaded via the "media" or "multipart" upload type.
    /// Drive does not return this for native Google formats (Docs /
    /// Sheets / Slides), but every Nyx Backup pack is a binary blob
    /// uploaded via multipart so it is always present in practice.
    #[serde(rename = "md5Checksum", default)]
    md5_checksum: Option<String>,
}

#[derive(Deserialize)]
struct FileListResponse {
    files: Vec<DriveFile>,
    #[serde(rename = "nextPageToken")]
    next_page_token: Option<String>,
}

// - Backend ----------------------------------

/// Google Drive storage backend.
pub struct GoogleDriveBackend {
    client: reqwest::Client,
    folder_id: String,
    tokens: Arc<TokenCache>,
}

impl GoogleDriveBackend {
    /// Construct a new backend.  Does not make any network calls.
    pub fn new(cfg: GoogleDriveConfig) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(300))
            .build()
            .map_err(|e| Error::Storage(format!("Google Drive HTTP client: {e}")))?;

        let client_id = if cfg.client_id.is_empty() {
            super::oauth::cred_env_or("GOOGLE_OAUTH_CLIENT_ID", BUNDLED_CLIENT_ID)
        } else {
            cfg.client_id
        };
        let client_secret = if cfg.client_secret.is_empty() {
            super::oauth::cred_env_or("GOOGLE_OAUTH_CLIENT_SECRET", BUNDLED_CLIENT_SECRET)
        } else {
            cfg.client_secret
        };
        let tokens = Arc::new(TokenCache::new(
            DRIVE_TOKEN_URL,
            client_id,
            client_secret,
            cfg.refresh_token,
            vec![],
        ));

        Ok(Self {
            client,
            folder_id: cfg.folder_id,
            tokens,
        })
    }

    // - Internal helpers -------------------------

    /// Find a file by exact name in the configured folder.
    async fn find_file(&self, name: &str, token: &str) -> Result<Option<DriveFile>> {
        let q = format!(
            "\"{}\" in parents and name = \"{}\" and trashed = false",
            self.folder_id,
            name.replace('"', "\\\""),
        );
        let resp = self
            .client
            .get(format!("{DRIVE_API}/files"))
            .query(&[
                ("q", q.as_str()),
                ("fields", "files(id,name,size,md5Checksum)"),
                ("pageSize", "1"),
            ])
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| Error::Storage(format!("Drive find_file: {e}")))?;

        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| Error::Storage(format!("Drive find_file body: {e}")))?;
        if !status.is_success() {
            return Err(Error::Storage(format!(
                "Drive find_file ({status}): {body}"
            )));
        }
        let list: FileListResponse = serde_json::from_str(&body)
            .map_err(|e| Error::Storage(format!("Drive find_file parse: {e}")))?;
        Ok(list.files.into_iter().next())
    }

    /// List all files in the folder whose names start with `prefix`.
    async fn list_files(&self, prefix: &str, token: &str) -> Result<Vec<DriveFile>> {
        let q = format!("\"{}\" in parents and trashed = false", self.folder_id);

        let mut files = Vec::new();
        let mut page_token: Option<String> = None;

        loop {
            let mut params = vec![
                ("q", q.as_str()),
                ("fields", "files(id,name,size,md5Checksum),nextPageToken"),
                ("pageSize", "1000"),
            ];
            let pt_owned;
            if let Some(ref pt) = page_token {
                pt_owned = pt.clone();
                params.push(("pageToken", pt_owned.as_str()));
            }

            let resp = self
                .client
                .get(format!("{DRIVE_API}/files"))
                .query(&params)
                .bearer_auth(token)
                .send()
                .await
                .map_err(|e| Error::Storage(format!("Drive list: {e}")))?;

            let status = resp.status();
            let body = resp
                .text()
                .await
                .map_err(|e| Error::Storage(format!("Drive list body: {e}")))?;
            if !status.is_success() {
                return Err(Error::Storage(format!("Drive list ({status}): {body}")));
            }
            let page: FileListResponse = serde_json::from_str(&body)
                .map_err(|e| Error::Storage(format!("Drive list parse: {e}")))?;

            for f in page.files {
                if f.name.starts_with(prefix) {
                    files.push(f);
                }
            }

            page_token = page.next_page_token;
            if page_token.is_none() {
                break;
            }
        }

        Ok(files)
    }

    /// Get a token, and on 401 force-refresh and retry the supplied closure once.
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
impl StorageBackend for GoogleDriveBackend {
    #[instrument(skip(self), fields(drive_folder = %self.folder_id, path))]
    async fn get(&self, path: &str) -> Result<Vec<u8>> {
        debug!("Drive get {path}");
        let name = path.to_string();
        self.with_token(|token| {
            let name = name.clone();
            async move {
                let file = self
                    .find_file(&name, &token)
                    .await?
                    .ok_or_else(|| Error::Storage(format!("Drive: not found: {name}")))?;
                let url = format!("{DRIVE_API}/files/{}?alt=media", file.id);
                let resp = self
                    .client
                    .get(&url)
                    .bearer_auth(&token)
                    .send()
                    .await
                    .map_err(|e| Error::Storage(format!("Drive get {name}: {e}")))?;
                let status = resp.status();
                if status.as_u16() == 404 {
                    return Err(Error::Storage(format!("Drive: not found: {name}")));
                }
                if !status.is_success() {
                    let body = resp.text().await.unwrap_or_default();
                    return Err(Error::Storage(format!(
                        "Drive get {name} ({status}): {body}"
                    )));
                }
                resp.bytes()
                    .await
                    .map(|b| b.to_vec())
                    .map_err(|e| Error::Storage(format!("Drive get {name} body: {e}")))
            }
        })
        .await
    }

    #[instrument(skip(self), fields(drive_folder = %self.folder_id, path, from, to))]
    async fn get_range(&self, path: &str, from: u64, to: u64) -> Result<Vec<u8>> {
        debug!("Drive get_range {path} [{from}..{to})");
        let name = path.to_string();
        self.with_token(|token| {
            let name = name.clone();
            async move {
                let file = self
                    .find_file(&name, &token)
                    .await?
                    .ok_or_else(|| Error::Storage(format!("Drive: not found: {name}")))?;
                let url = format!("{DRIVE_API}/files/{}?alt=media", file.id);
                let resp = self
                    .client
                    .get(&url)
                    .bearer_auth(&token)
                    .header("Range", format!("bytes={}-{}", from, to - 1))
                    .send()
                    .await
                    .map_err(|e| Error::Storage(format!("Drive get_range {name}: {e}")))?;
                let status = resp.status();
                if !status.is_success() && status.as_u16() != 206 {
                    let body = resp.text().await.unwrap_or_default();
                    return Err(Error::Storage(format!(
                        "Drive get_range {name} ({status}): {body}"
                    )));
                }
                resp.bytes()
                    .await
                    .map(|b| b.to_vec())
                    .map_err(|e| Error::Storage(format!("Drive get_range {name} body: {e}")))
            }
        })
        .await
    }

    #[instrument(skip(self), fields(drive_folder = %self.folder_id, path))]
    // See StorageBackend::probe_access: a single cheap authed round trip via
    // exists("") - one files.list (pageSize=1) scoped to the configured
    // folder, which validates the token AND folder access without the
    // recursive walk that list("") performs.  Both Ok(true)/Ok(false) mean
    // reachable + authenticated.
    async fn probe_access(&self) -> Result<()> {
        self.exists("").await.map(|_| ())
    }

    async fn exists(&self, path: &str) -> Result<bool> {
        let name = path.to_string();
        self.with_token(|token| {
            let name = name.clone();
            async move { self.find_file(&name, &token).await.map(|f| f.is_some()) }
        })
        .await
    }

    #[instrument(skip(self), fields(drive_folder = %self.folder_id, prefix))]
    async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        debug!("Drive list {prefix:?}");
        let prefix = prefix.to_string();
        self.with_token(|token| {
            let prefix = prefix.clone();
            async move {
                self.list_files(&prefix, &token)
                    .await
                    .map(|files| files.into_iter().map(|f| f.name).collect())
            }
        })
        .await
    }

    #[instrument(skip(self), fields(drive_folder = %self.folder_id, prefix))]
    async fn list_with_sizes(&self, prefix: &str) -> Result<Vec<(String, u64)>> {
        let prefix = prefix.to_string();
        self.with_token(|token| {
            let prefix = prefix.clone();
            async move {
                self.list_files(&prefix, &token).await.map(|files| {
                    files
                        .into_iter()
                        .map(|f| {
                            let sz = f.size.as_deref().and_then(|s| s.parse().ok()).unwrap_or(0);
                            (f.name, sz)
                        })
                        .collect()
                })
            }
        })
        .await
    }

    #[instrument(skip(self), fields(drive_folder = %self.folder_id, path))]
    async fn size(&self, path: &str) -> Result<u64> {
        let name = path.to_string();
        self.with_token(|token| {
            let name = name.clone();
            async move {
                let file = self
                    .find_file(&name, &token)
                    .await?
                    .ok_or_else(|| Error::Storage(format!("Drive size: not found: {name}")))?;
                file.size
                    .as_deref()
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| Error::Storage(format!("Drive size: no size for {name}")))
            }
        })
        .await
    }

    /// content-verifying HEAD for Google Drive.
    /// Uses Drive's `md5Checksum` field, which the API populates for
    /// every binary upload (multipart / media uploadType - which is
    /// what `GoogleDriveBackend::put` uses).  Using the checksum gives
    /// the quick integrity audit content-level verification on Drive
    /// destinations, rather than existence-only via `size()`.
    #[instrument(skip(self), fields(drive_folder = %self.folder_id, path))]
    async fn head_with_hash(&self, path: &str) -> Result<(u64, String, String)> {
        let name = path.to_string();
        self.with_token(|token| {
            let name = name.clone();
            async move {
                let file = self.find_file(&name, &token).await?.ok_or_else(|| {
                    Error::Storage(format!("Drive head_with_hash: not found: {name}"))
                })?;
                let size: u64 = file
                    .size
                    .as_deref()
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| {
                        Error::Storage(format!("Drive head_with_hash: no size for {name}"))
                    })?;
                let hash = file.md5_checksum.ok_or_else(|| {
                    Error::Storage(format!("Drive head_with_hash: no md5Checksum for {name}"))
                })?;
                Ok((size, hash, "md5".to_string()))
            }
        })
        .await
    }

    fn display_name(&self) -> String {
        format!("gdrive://{}", self.folder_id)
    }
}
