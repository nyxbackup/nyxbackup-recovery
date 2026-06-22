// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! Tauri-side adapter for the shared OneDrive OAuth flow in
//! [`bkp_oauth::onedrive`].  Exposes both the one-click loopback flow
//! (`run_oauth_flow`) and the manual relay (`manual_auth_url` /
//! `exchange_code`) for headless hosts with no usable browser.

use bkp_oauth::onedrive::{OneDriveCreds, OneDriveOAuthResult};
use serde::Serialize;
use tauri::{AppHandle, Listener};
use tokio_util::sync::CancellationToken;

// Compiled in from ONEDRIVE_OAUTH_CLIENT_ID.  OneDrive uses the public-client
// flow (no secret); the default `common` tenant covers personal + work/school
// accounts.
const BUNDLED_CLIENT_ID: &str = env!("ONEDRIVE_OAUTH_CLIENT_ID");

/// Runtime-overridable client id: the `ONEDRIVE_OAUTH_CLIENT_ID` env var if set,
/// otherwise the compiled-in value.  Cached once (wind-down escape hatch: a
/// user can register their own Entra app and point the flow at it without
/// rebuilding).
fn client_id() -> &'static str {
    static V: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    V.get_or_init(|| match std::env::var("ONEDRIVE_OAUTH_CLIENT_ID") {
        Ok(v) if !v.trim().is_empty() => v,
        _ => BUNDLED_CLIENT_ID.to_string(),
    })
    .as_str()
}

#[derive(Serialize)]
pub struct OneDriveOAuthFrontend {
    pub refresh_token: String,
    pub tenant_id: String,
    pub email: String,
}

impl From<OneDriveOAuthResult> for OneDriveOAuthFrontend {
    fn from(r: OneDriveOAuthResult) -> Self {
        Self {
            refresh_token: r.refresh_token,
            tenant_id: r.tenant_id,
            email: r.email,
        }
    }
}

// `tenant_id` is the Microsoft Identity audience: `common` (personal +
// work/school), `consumers` (personal only), `organizations` (work/school),
// or a tenant GUID.  Empty falls back to the bundled default.
fn creds(tenant_id: &str) -> OneDriveCreds<'_> {
    OneDriveCreds {
        client_id: client_id(),
        tenant_id,
    }
}

/// One-click loopback OAuth (works where a browser can reach this machine's
/// localhost - including WSL via the Windows browser).
pub async fn run_oauth_flow(
    tenant_id: String,
    app: AppHandle,
) -> anyhow::Result<OneDriveOAuthFrontend> {
    let cancel = CancellationToken::new();
    let cancel_for_event = cancel.clone();
    let _unlisten = app.listen("cancel-onedrive-oauth", move |_| {
        cancel_for_event.cancel();
    });
    Ok(
        bkp_oauth::onedrive::run_oauth_flow(creds(&tenant_id), cancel)
            .await?
            .into(),
    )
}

/// Manual (no-local-browser) relay, step 1: `(auth_url, redirect_uri)`.
pub fn manual_auth_url(tenant_id: String) -> anyhow::Result<(String, String)> {
    bkp_oauth::onedrive::manual_auth_url(&creds(&tenant_id))
}

/// Manual relay, step 2: exchange the pasted `code` for a refresh token.
pub async fn exchange_code(
    tenant_id: String,
    code: String,
    redirect_uri: String,
) -> anyhow::Result<OneDriveOAuthFrontend> {
    Ok(
        bkp_oauth::onedrive::exchange_code(&creds(&tenant_id), &code, &redirect_uri)
            .await?
            .into(),
    )
}
