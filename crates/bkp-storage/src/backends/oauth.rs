// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! Shared OAuth 2.0 access-token cache for the Google Drive, OneDrive, and
//! Dropbox backends.
//!
//! Each backend stores a `TokenCache` and calls `get_token()` before every API
//! request.  The cache proactively refreshes the access token when less than
//! five minutes of validity remain; on HTTP 401 the backend calls `force_refresh()`
//! to bypass the expiry check.

use std::time::{Duration, Instant};

use bkp_types::error::{Error, Result};
use tokio::sync::RwLock;
use tracing::{debug, warn};

/// Resolve an OAuth client credential with a runtime override.
///
/// Precedence, highest first:
///   1. the per-endpoint config field (checked by the caller before calling this),
///   2. the process environment variable `var` at runtime, and
///   3. the value baked in at compile time (`bundled`).
///
/// The environment layer is a deliberate wind-down escape hatch: if the
/// project's OAuth apps ever stop working, an end user (or a delegate acting on
/// their behalf) can register their own OAuth app and point the tool at it by
/// setting the corresponding `*_CLIENT_ID` / `*_CLIENT_SECRET` /
/// `*_APP_KEY` / `*_APP_SECRET` environment variable - no rebuild required.
/// Their data lives in their own cloud account, so a fresh app with read scope
/// reaches the same backups.
pub fn cred_env_or(var: &str, bundled: &str) -> String {
    match std::env::var(var) {
        Ok(v) if !v.trim().is_empty() => v,
        _ => bundled.to_string(),
    }
}

// - Cached token -------------------------------

#[derive(Debug, Clone)]
struct CachedToken {
    access_token: String,
    /// Deadline before which the token is still valid.
    expires_at: Instant,
}

// - TokenCache --------------------------------

/// Thread-safe OAuth 2.0 access-token cache with automatic refresh.
///
/// Construct once per backend instance; share behind `Arc` if needed.
pub struct TokenCache {
    token_url: String,
    client_id: String,
    client_secret: String,
    refresh_token: String,
    /// Optional extra form field (e.g. `scope`).
    extra_params: Vec<(String, String)>,
    cached: RwLock<Option<CachedToken>>,
}

impl TokenCache {
    /// Create a new cache.  `extra_params` are appended to every token request.
    pub fn new(
        token_url: impl Into<String>,
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
        refresh_token: impl Into<String>,
        extra_params: Vec<(String, String)>,
    ) -> Self {
        Self {
            token_url: token_url.into(),
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            refresh_token: refresh_token.into(),
            extra_params,
            cached: RwLock::new(None),
        }
    }

    /// Return a valid access token, refreshing if within 5 minutes of expiry.
    pub async fn get_token(&self, client: &reqwest::Client) -> Result<String> {
        {
            let guard = self.cached.read().await;
            if let Some(ref t) = *guard
                && t.expires_at > Instant::now() + Duration::from_secs(300)
            {
                return Ok(t.access_token.clone());
            }
        }
        self.do_refresh(client).await
    }

    /// Force a refresh regardless of cached expiry (call after HTTP 401).
    pub async fn force_refresh(&self, client: &reqwest::Client) -> Result<String> {
        warn!("OAuth: forcing token refresh after 401");
        self.do_refresh(client).await
    }

    async fn do_refresh(&self, client: &reqwest::Client) -> Result<String> {
        debug!("OAuth: refreshing access token via {}", self.token_url);
        // client_secret is sent only when non-empty.  Microsoft Entra
        // returns AADSTS90023 ("Public clients can't send a client
        // secret") when "Allow public client flows" is enabled - the
        // OneDrive backend constructs this cache with an empty
        // client_secret precisely for that case.  Google Drive and
        // Dropbox stay confidential-client and continue to send their
        // secrets unchanged.
        let mut params: Vec<(&str, &str)> = vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", &self.refresh_token),
            ("client_id", &self.client_id),
        ];
        if !self.client_secret.is_empty() {
            params.push(("client_secret", &self.client_secret));
        }
        let extra: Vec<(&str, &str)> = self
            .extra_params
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        params.extend_from_slice(&extra);

        let resp = client
            .post(&self.token_url)
            .form(&params)
            .send()
            .await
            .map_err(|e| Error::Storage(format!("OAuth token refresh request: {e}")))?;

        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| Error::Storage(format!("OAuth token refresh body: {e}")))?;

        if !status.is_success() {
            return Err(Error::Storage(format!(
                "OAuth token refresh failed ({status}): {body}"
            )));
        }

        let json: serde_json::Value = serde_json::from_str(&body)
            .map_err(|e| Error::Storage(format!("OAuth token refresh parse: {e}: {body}")))?;

        let access_token = json["access_token"]
            .as_str()
            .ok_or_else(|| {
                Error::Storage("OAuth token refresh: no access_token in response".to_string())
            })?
            .to_string();

        let expires_in = json["expires_in"].as_u64().unwrap_or(3600);
        let expires_at = Instant::now() + Duration::from_secs(expires_in);

        debug!("OAuth: new access token; expires_in={expires_in}s");

        let mut guard = self.cached.write().await;
        *guard = Some(CachedToken {
            access_token: access_token.clone(),
            expires_at,
        });

        Ok(access_token)
    }
}

// - JSON credential blob helpers -----------------------

/// Parse the secret JSON blob stored in `secret_access_key` for Google Drive
/// and Dropbox: `{"client_secret":"...","refresh_token":"..."}`.
///
/// Returns `(client_secret_or_app_secret, refresh_token)`.  Logs a warning and
/// returns empty strings on parse failure.
pub fn parse_oauth_blob(blob: Option<&str>) -> (String, String) {
    let blob = match blob {
        Some(b) if !b.is_empty() => b,
        _ => return (String::new(), String::new()),
    };
    let v: serde_json::Value = serde_json::from_str(blob).unwrap_or_default();
    let secret = v["client_secret"]
        .as_str()
        .or_else(|| v["app_secret"].as_str())
        .unwrap_or("")
        .to_string();
    let rt = v["refresh_token"].as_str().unwrap_or("").to_string();
    (secret, rt)
}

/// Like `parse_oauth_blob` but also extracts `tenant_id` (for OneDrive).
///
/// Returns `(client_secret, refresh_token, tenant_id)`.
pub fn parse_oauth_blob_onedrive(
    blob: Option<&str>,
    region_fallback: Option<&str>,
) -> (String, String, String) {
    let blob = match blob {
        Some(b) if !b.is_empty() => b,
        _ => {
            return (
                String::new(),
                String::new(),
                region_fallback.unwrap_or("consumers").to_string(),
            );
        }
    };
    let v: serde_json::Value = serde_json::from_str(blob).unwrap_or_default();
    let secret = v["client_secret"].as_str().unwrap_or("").to_string();
    let rt = v["refresh_token"].as_str().unwrap_or("").to_string();
    let tenant = v["tenant_id"]
        .as_str()
        .or(region_fallback)
        .unwrap_or("consumers")
        .to_string();
    (secret, rt, tenant)
}
