// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! Microsoft OneDrive storage backend - Microsoft Graph API v1.0.
//!
//! # Storage model
//!
//! Backup objects are stored using nested folders mirroring the object path.
//! `packs/abc123.pack` becomes `/NyxBackup/packs/abc123.pack` in OneDrive.
//! Subfolders (`packs/`, `snapshots/`, etc.) are created on first use and their
//! item IDs are cached for the lifetime of the backend instance.
//!
//! # Authentication
//!
//! Uses the OAuth 2.0 **public-client** flow via the Microsoft Identity
//! Platform.  `client_id`, `refresh_token`, and `tenant_id` are required;
//! no client_secret is sent (Microsoft returns AADSTS90023 if we do -
//! the Entra app registration must have "Allow public client flows"
//! enabled).  Use `"common"` as `tenant_id` for personal + work/school,
//! `"consumers"` for personal only, `"organizations"` for work/school,
//! or a specific tenant GUID.
//!
//! Access tokens are cached and proactively refreshed 5 minutes before expiry.
//!
//! # Uploads
//!
//! Files ≤ 4 MB are uploaded with a single PUT request.  Files > 4 MB use an
//! upload session (chunked, fault-tolerant).  This covers pack files up to the
//! hard chunk limit of 16 MB.
//!
//! # put_if_absent
//!
//! The Graph API `createUploadSession` with `conflictBehavior: "fail"` returns
//! HTTP 409 when the file already exists, providing a near-atomic conditional
//! create.

use std::sync::Arc;
use std::time::Duration;

use bkp_types::error::{Error, Result};
use serde::Deserialize;
use tracing::{debug, instrument};

use super::oauth::TokenCache;
use crate::backend::StorageBackend;

const GRAPH_API: &str = "https://graph.microsoft.com/v1.0";

// bundled Microsoft Entra app credentials,
// same pattern as Google Drive (GOOGLE_OAUTH_CLIENT_*) and Dropbox
// (DROPBOX_APP_*).  Empty fallback under `option_env!` so plain
// `cargo build` in IDEs without a configured .env still compiles -
// only the GUI / TUI binaries built via build_windows_x86_64.sh
// actually need real values to run the OAuth flow.
const BUNDLED_CLIENT_ID: &str = match option_env!("ONEDRIVE_OAUTH_CLIENT_ID") {
    Some(s) => s,
    None => "",
};

// - Configuration -------------------------------

/// Configuration for the OneDrive backend.
///
/// switched to the public-client OAuth flow; the
/// `client_secret` config field was removed because Microsoft
/// rejects token requests that carry a secret when the registration
/// has "Allow public client flows" enabled.  `client_id` remains
/// optional - the bundled Entra app GUID embedded in the binary at
/// compile time fills in when the config doesn't provide it.  The
/// OAuth flow only persists
/// `refresh_token` + `tenant_id` and the user never sees app creds.
#[derive(Debug, Clone, Deserialize)]
pub struct OneDriveConfig {
    /// Root folder path in OneDrive, e.g. `/NyxBackup`.
    pub folder_path: String,
    /// Azure App Registration client ID.  Empty -> bundled client_id.
    #[serde(default)]
    pub client_id: String,
    /// Microsoft Identity Platform tenant ID.
    /// Use `"common"` (default) for personal + work/school,
    /// `"consumers"` for personal only, `"organizations"` for any
    /// work/school account, or a specific tenant GUID.
    #[serde(default = "default_tenant_id")]
    pub tenant_id: String,
    /// OAuth 2.0 refresh token.
    pub refresh_token: String,
}

fn default_tenant_id() -> String {
    "common".to_string()
}

// - API response types ----------------------------

#[derive(Deserialize)]
struct DriveItem {
    name: String,
    size: Option<u64>,
    folder: Option<serde_json::Value>,
    /// per-DriveItem hash bundle.  Microsoft
    /// returns up to three hash variants depending on the account
    /// type: `sha1Hash`, `quickXorHash` (business / SharePoint), and
    /// `sha256Hash` (rolling out 2024-).  We prefer sha256 when
    /// present, fall back to sha1 (personal), and last quickXorHash
    /// (some business tenants).
    file: Option<DriveItemFile>,
}

#[derive(Deserialize)]
struct DriveItemFile {
    hashes: Option<DriveItemHashes>,
}

#[derive(Deserialize)]
struct DriveItemHashes {
    #[serde(rename = "sha256Hash", default)]
    sha256: Option<String>,
    #[serde(rename = "sha1Hash", default)]
    sha1: Option<String>,
    #[serde(rename = "quickXorHash", default)]
    quick_xor: Option<String>,
}

#[derive(Deserialize)]
struct ChildrenResponse {
    value: Vec<DriveItem>,
    #[serde(rename = "@odata.nextLink")]
    next_link: Option<String>,
}

// - Backend ----------------------------------

/// Microsoft OneDrive storage backend.
pub struct OneDriveBackend {
    client: reqwest::Client,
    /// Root folder path, e.g. `/NyxBackup`.
    folder_path: String,
    tokens: Arc<TokenCache>,
}

impl OneDriveBackend {
    /// Construct a new backend.  Does not make any network calls.
    pub fn new(cfg: OneDriveConfig) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(300))
            .build()
            .map_err(|e| Error::Storage(format!("OneDrive HTTP client: {e}")))?;

        // Fall back to the bundled Entra app creds when the persisted
        // config doesn't carry them (the newer case where the
        // OAuth flow only stored refresh_token + tenant).
        // OneDrive uses the **public-client** OAuth flow:
        // client_id alone authenticates token-endpoint requests,
        // protected by the loopback redirect + one-shot listener.
        // Public clients MUST NOT send a client_secret (Microsoft
        // returns AADSTS90023) - the TokenCache is constructed with
        // an empty secret string and the refresh path omits the field.
        let effective_client_id = if cfg.client_id.is_empty() {
            super::oauth::cred_env_or("ONEDRIVE_OAUTH_CLIENT_ID", BUNDLED_CLIENT_ID)
        } else {
            cfg.client_id
        };
        if effective_client_id.is_empty() {
            return Err(Error::Storage(
                "OneDrive backend: client_id unavailable. \
                 The bundled Entra app GUID was not compiled in - \
                 set ONEDRIVE_OAUTH_CLIENT_ID at build time, or provide \
                 client_id in the backup-set config."
                    .into(),
            ));
        }

        let token_url = format!(
            "https://login.microsoftonline.com/{}/oauth2/v2.0/token",
            cfg.tenant_id
        );
        let tokens = Arc::new(TokenCache::new(
            token_url,
            effective_client_id,
            String::new(),
            cfg.refresh_token,
            vec![(
                "scope".to_string(),
                "offline_access Files.ReadWrite".to_string(),
            )],
        ));

        let root = cfg.folder_path.trim_end_matches('/').to_string();

        Ok(Self {
            client,
            folder_path: root,
            tokens,
        })
    }

    // - Path helpers ---------------------------

    /// Build the full OneDrive path for an object path.
    fn full_path(&self, path: &str) -> String {
        if path.is_empty() {
            self.folder_path.clone()
        } else {
            format!("{}/{}", self.folder_path, path.trim_start_matches('/'))
        }
    }

    /// Graph API path-based URL for an item.
    fn item_url(&self, path: &str) -> String {
        let fp = self.full_path(path);
        format!("{GRAPH_API}/me/drive/root:{}:", fp)
    }

    /// Graph API URL for an item's content.
    fn content_url(&self, path: &str) -> String {
        let fp = self.full_path(path);
        format!("{GRAPH_API}/me/drive/root:{fp}:/content")
    }

    // - Folder management -------------------------

    // - Upload helpers --------------------------

    // - List helpers ---------------------------

    /// List all items (files + folders) that are children of `folder_path`.
    async fn list_folder_children(&self, folder_path: &str, token: &str) -> Result<Vec<DriveItem>> {
        let fp = folder_path.to_string();
        let base_url = format!(
            "{GRAPH_API}/me/drive/root:{fp}:/children?$select=id,name,size,file,folder&$top=1000"
        );

        let mut items = Vec::new();
        let mut next_url: Option<String> = Some(base_url);

        while let Some(url) = next_url {
            let resp = self
                .client
                .get(&url)
                .bearer_auth(token)
                .send()
                .await
                .map_err(|e| Error::Storage(format!("OneDrive list_children: {e}")))?;

            let status = resp.status();
            if status.as_u16() == 404 {
                return Ok(Vec::new()); // folder doesn't exist yet
            }
            let body = resp
                .text()
                .await
                .map_err(|e| Error::Storage(format!("OneDrive list_children body: {e}")))?;
            if !status.is_success() {
                return Err(Error::Storage(format!(
                    "OneDrive list_children ({status}): {body}"
                )));
            }
            let page: ChildrenResponse = serde_json::from_str(&body)
                .map_err(|e| Error::Storage(format!("OneDrive list_children parse: {e}")))?;

            items.extend(page.value);
            next_url = page.next_link;
        }

        Ok(items)
    }

    /// Recursively list all files under `folder_path` up to two levels deep.
    /// Returns `(relative_path, size)` pairs.
    async fn list_recursive(
        &self,
        folder_path: &str,
        rel_prefix: &str,
        token: &str,
    ) -> Result<Vec<(String, u64)>> {
        let items = self.list_folder_children(folder_path, token).await?;
        let mut results = Vec::new();

        for item in items {
            let rel = if rel_prefix.is_empty() {
                item.name.clone()
            } else {
                format!("{rel_prefix}/{}", item.name)
            };

            if item.folder.is_some() {
                // Recurse one level.
                let sub_path = format!("{folder_path}/{}", item.name);
                let sub = Box::pin(self.list_recursive(&sub_path, &rel, token)).await?;
                results.extend(sub);
            } else {
                results.push((rel, item.size.unwrap_or(0)));
            }
        }
        Ok(results)
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
impl StorageBackend for OneDriveBackend {
    #[instrument(skip(self), fields(onedrive_root = %self.folder_path, path))]
    async fn get(&self, path: &str) -> Result<Vec<u8>> {
        debug!("OneDrive get {path}");
        let url = self.content_url(path);
        self.with_token(|token| {
            let url = url.clone();
            let path = path.to_string();
            async move {
                let resp = self
                    .client
                    .get(&url)
                    .bearer_auth(&token)
                    .send()
                    .await
                    .map_err(|e| Error::Storage(format!("OneDrive get {path}: {e}")))?;
                let status = resp.status();
                if status.as_u16() == 404 {
                    return Err(Error::Storage(format!("OneDrive: not found: {path}")));
                }
                if !status.is_success() {
                    let body = resp.text().await.unwrap_or_default();
                    return Err(Error::Storage(format!(
                        "OneDrive get {path} ({status}): {body}"
                    )));
                }
                resp.bytes()
                    .await
                    .map(|b| b.to_vec())
                    .map_err(|e| Error::Storage(format!("OneDrive get body: {e}")))
            }
        })
        .await
    }

    #[instrument(skip(self), fields(onedrive_root = %self.folder_path, path, from, to))]
    async fn get_range(&self, path: &str, from: u64, to: u64) -> Result<Vec<u8>> {
        let url = self.content_url(path);
        self.with_token(|token| {
            let url = url.clone();
            let path = path.to_string();
            async move {
                let resp = self
                    .client
                    .get(&url)
                    .bearer_auth(&token)
                    .header("Range", format!("bytes={}-{}", from, to - 1))
                    .send()
                    .await
                    .map_err(|e| Error::Storage(format!("OneDrive get_range {path}: {e}")))?;
                let status = resp.status();
                if !status.is_success() && status.as_u16() != 206 {
                    let body = resp.text().await.unwrap_or_default();
                    return Err(Error::Storage(format!(
                        "OneDrive get_range {path} ({status}): {body}"
                    )));
                }
                resp.bytes()
                    .await
                    .map(|b| b.to_vec())
                    .map_err(|e| Error::Storage(format!("OneDrive get_range body: {e}")))
            }
        })
        .await
    }

    #[instrument(skip(self), fields(onedrive_root = %self.folder_path, path))]
    // See StorageBackend::probe_access.  The connection test avoids list(""), which for OneDrive recurses one level
    // into every subfolder (one Graph round trip each) - an 8-12 s walk.
    // exists("") is a single Graph metadata GET on the configured folder
    // (driving the token refresh via with_token), so the probe is sub-second.
    // Both Ok(true)/Ok(false) mean reachable + authenticated.
    async fn probe_access(&self) -> Result<()> {
        self.exists("").await.map(|_| ())
    }

    async fn exists(&self, path: &str) -> Result<bool> {
        let url = self.item_url(path);
        self.with_token(|token| {
            let url = url.clone();
            async move {
                let resp = self
                    .client
                    .get(&url)
                    .bearer_auth(&token)
                    .send()
                    .await
                    .map_err(|e| Error::Storage(format!("OneDrive exists: {e}")))?;
                Ok(resp.status().as_u16() != 404 && resp.status().is_success())
            }
        })
        .await
    }

    #[instrument(skip(self), fields(onedrive_root = %self.folder_path, prefix))]
    async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        debug!("OneDrive list {prefix:?}");
        let (folder_path, name_prefix) = split_prefix(prefix, &self.folder_path);
        let folder_path = folder_path.clone();
        let name_prefix = name_prefix.clone();

        self.with_token(|token| {
            let folder_path = folder_path.clone();
            let name_prefix = name_prefix.clone();
            async move {
                if name_prefix.is_empty() && !folder_path.ends_with(&self.folder_path) {
                    // List a specific subfolder recursively.
                    let pairs = self
                        .list_recursive(
                            &folder_path,
                            &strip_root(&folder_path, &self.folder_path),
                            &token,
                        )
                        .await?;
                    return Ok(pairs.into_iter().map(|(p, _)| p).collect());
                }
                let pairs = self
                    .list_recursive(
                        &folder_path,
                        &strip_root(&folder_path, &self.folder_path),
                        &token,
                    )
                    .await?;
                Ok(pairs
                    .into_iter()
                    .filter(|(p, _)| p.starts_with(&name_prefix))
                    .map(|(p, _)| p)
                    .collect())
            }
        })
        .await
    }

    #[instrument(skip(self), fields(onedrive_root = %self.folder_path, prefix))]
    async fn list_with_sizes(&self, prefix: &str) -> Result<Vec<(String, u64)>> {
        let (folder_path, name_prefix) = split_prefix(prefix, &self.folder_path);
        let folder_path = folder_path.clone();
        let name_prefix = name_prefix.clone();

        self.with_token(|token| {
            let folder_path = folder_path.clone();
            let name_prefix = name_prefix.clone();
            async move {
                let pairs = self
                    .list_recursive(
                        &folder_path,
                        &strip_root(&folder_path, &self.folder_path),
                        &token,
                    )
                    .await?;
                Ok(pairs
                    .into_iter()
                    .filter(|(p, _)| p.starts_with(&name_prefix))
                    .collect())
            }
        })
        .await
    }

    /// content-verifying HEAD for OneDrive.
    /// Microsoft Graph's DriveItem exposes a `file.hashes` bundle.
    /// Prefer sha256 (new tenants 2024+), fall back to sha1
    /// (personal accounts), then quickXorHash (some business
    /// tenants).  At least one is always present for an uploaded
    /// binary; if all three are missing we return an error so the
    /// scheduler logs the gap rather than recording empty baseline.
    #[instrument(skip(self), fields(onedrive_root = %self.folder_path, path))]
    async fn head_with_hash(&self, path: &str) -> Result<(u64, String, String)> {
        let url = format!("{}?$select=size,file", self.item_url(path));
        self.with_token(|token| {
            let url = url.clone();
            let path = path.to_string();
            async move {
                let resp = self
                    .client
                    .get(&url)
                    .bearer_auth(&token)
                    .send()
                    .await
                    .map_err(|e| Error::Storage(format!("OneDrive head_with_hash {path}: {e}")))?;
                let status = resp.status();
                if status.as_u16() == 404 {
                    return Err(Error::Storage(format!(
                        "OneDrive head_with_hash: not found: {path}"
                    )));
                }
                let body = resp
                    .text()
                    .await
                    .map_err(|e| Error::Storage(format!("OneDrive head_with_hash body: {e}")))?;
                if !status.is_success() {
                    return Err(Error::Storage(format!(
                        "OneDrive head_with_hash {path} ({status}): {body}"
                    )));
                }
                let item: DriveItem = serde_json::from_str(&body)
                    .map_err(|e| Error::Storage(format!("OneDrive head_with_hash parse: {e}")))?;
                let size = item.size.ok_or_else(|| {
                    Error::Storage(format!("OneDrive head_with_hash: no size for {path}"))
                })?;
                let hashes = item.file.and_then(|f| f.hashes).ok_or_else(|| {
                    Error::Storage(format!(
                        "OneDrive head_with_hash: no file.hashes for {path}"
                    ))
                })?;
                if let Some(h) = hashes.sha256 {
                    return Ok((size, h.to_lowercase(), "sha256".to_string()));
                }
                if let Some(h) = hashes.sha1 {
                    return Ok((size, h.to_lowercase(), "sha1".to_string()));
                }
                if let Some(h) = hashes.quick_xor {
                    return Ok((size, h, "quickxor".to_string()));
                }
                Err(Error::Storage(format!(
                    "OneDrive head_with_hash: no usable hash for {path}"
                )))
            }
        })
        .await
    }

    #[instrument(skip(self), fields(onedrive_root = %self.folder_path, path))]
    async fn size(&self, path: &str) -> Result<u64> {
        // Deserialize into a size-only struct: the request `$select=size`
        // returns just `{ "size": N }` (no `name`), so the full `DriveItem`
        // (which requires `name`) cannot parse it - that failure made every
        // pack's size() fail, leaving the chunk->pack map empty and every
        // chunk falling back to the legacy `chunks/<hash>` path (404).
        #[derive(Deserialize)]
        struct SizeResp {
            size: Option<u64>,
        }
        let url = format!("{}?$select=size", self.item_url(path));
        self.with_token(|token| {
            let url = url.clone();
            let path = path.to_string();
            async move {
                let resp = self
                    .client
                    .get(&url)
                    .bearer_auth(&token)
                    .send()
                    .await
                    .map_err(|e| Error::Storage(format!("OneDrive size {path}: {e}")))?;
                let status = resp.status();
                if status.as_u16() == 404 {
                    return Err(Error::Storage(format!("OneDrive size: not found: {path}")));
                }
                let body = resp
                    .text()
                    .await
                    .map_err(|e| Error::Storage(format!("OneDrive size body: {e}")))?;
                if !status.is_success() {
                    return Err(Error::Storage(format!(
                        "OneDrive size {path} ({status}): {body}"
                    )));
                }
                let item: SizeResp = serde_json::from_str(&body)
                    .map_err(|e| Error::Storage(format!("OneDrive size parse: {e}")))?;
                item.size.ok_or_else(|| {
                    Error::Storage(format!("OneDrive size: no size field for {path}"))
                })
            }
        })
        .await
    }

    fn display_name(&self) -> String {
        format!("onedrive:{}", self.folder_path)
    }
}

// - Path helpers -------------------------------

/// Split an object prefix into `(full_folder_path, remaining_name_prefix)`.
///
/// `list("packs/")` with root `/NyxBackup` → `("/NyxBackup/packs", "")`.
/// `list("")` with root `/NyxBackup` → `("/NyxBackup", "")`.
fn split_prefix(prefix: &str, root: &str) -> (String, String) {
    let prefix = prefix.trim_end_matches('/');
    if prefix.is_empty() {
        return (root.to_string(), String::new());
    }
    // If the prefix ends at a directory boundary, list that directory.
    let full = format!("{root}/{prefix}");
    (full, String::new())
}

/// Strip the root prefix from a full path, returning the relative path.
fn strip_root(full: &str, root: &str) -> String {
    full.strip_prefix(root)
        .unwrap_or(full)
        .trim_start_matches('/')
        .to_string()
}
