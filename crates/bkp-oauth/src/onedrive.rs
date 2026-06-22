// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! Microsoft OneDrive OAuth 2.0 authorization-code flow.
//!
//! Targets both personal and business accounts via the Microsoft Identity
//! Platform v2.0 endpoints.  `tenant_id` selects the audience:
//!   - `"consumers"` - personal Microsoft accounts only (live.com / outlook.com).
//!   - `"organizations"` - any work or school account.
//!   - `"common"` - both personal and work accounts (we default to this).
//!   - A specific tenant GUID - lock down to one Entra tenant.
//!
//! Scopes:
//!   - `Files.ReadWrite` - read + write app-targeted files inside the user's
//!     OneDrive.  Same shape as Google's `drive.file` scope by default, but
//!     OneDrive's `Files.ReadWrite` actually grants full personal-OneDrive
//!     access at runtime - no need for the broader `Files.ReadWrite.All`
//!     for the typical end-user backup case (the bundled app is consumer-
//!     facing, not for managing other tenants' files).
//!   - `offline_access` - required to get a refresh_token.  Without this
//!     the token endpoint returns access_token only and the daemon's
//!     next backup fails after one hour.
//!   - `User.Read` - lets the userinfo call return the user's `mail` /
//!     `userPrincipalName` so we can display "Connected as foo@bar".
//!   - `openid` + `email` - identity scopes for the userinfo call.

use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::{bind_loopback_listener, open_browser, urlencode, wait_for_auth_code};

const USERINFO_URL: &str = "https://graph.microsoft.com/v1.0/me";

/// Default tenant when the caller doesn't specify one.  `common` accepts
/// both personal (live.com) and work/school accounts, which is the
/// least-surprising default for a consumer-facing backup tool.
pub const DEFAULT_TENANT: &str = "common";

/// Bundled OAuth client credentials.  Pass `env!("ONEDRIVE_OAUTH_CLIENT_ID")`
/// and `env!("ONEDRIVE_OAUTH_CLIENT_SECRET")` from the caller so the
/// secrets stay embedded in each individual binary, not in this shared
/// crate.
pub struct OneDriveCreds<'a> {
    pub client_id: &'a str,
    /// Tenant ID or one of `common` / `consumers` / `organizations`.
    /// Empty -> [`DEFAULT_TENANT`].
    pub tenant_id: &'a str,
}

#[derive(Debug, Clone)]
pub struct OneDriveOAuthResult {
    /// Refresh token suitable for storage and re-use by
    /// `bkp-storage::backends::onedrive::OneDriveBackend`.
    pub refresh_token: String,
    /// Tenant we authenticated against (resolved from `creds.tenant_id`
    /// or [`DEFAULT_TENANT`]).  Passed through to the backend config so
    /// future token refreshes hit the same endpoint.
    pub tenant_id: String,
    /// Email / UPN decoded from the `/me` endpoint, best-effort.  Empty
    /// on failure (the rest of the flow still completes).
    pub email: String,
}

/// Authorization-code OAuth flow against the Microsoft Identity
/// Platform v2.0 endpoints.  Caller supplies the bundled client_id +
/// secret + tenant; we open the user's browser, listen on loopback for
/// the redirect, exchange the code for a refresh_token, then decode
/// their email for display.
///
/// `cancel` is checked while waiting for the browser redirect;
/// triggering it aborts with "Authorization cancelled."  The folder
/// path itself (e.g. `/NyxBackup`) is NOT collected here - the editor
/// asks for it separately because Microsoft Graph doesn't have a
/// well-known "pick a folder" picker URL we can embed in the flow.
/// Space-separated scopes.  offline_access MUST be present to receive a
/// refresh_token; the same list is resent on the token exchange so it is
/// stamped onto the refresh_token consistently.
const SCOPE: &str = "Files.ReadWrite offline_access User.Read openid email";

/// Resolve the effective tenant (`creds.tenant_id` or [`DEFAULT_TENANT`]).
fn resolve_tenant(creds: &OneDriveCreds) -> String {
    if creds.tenant_id.trim().is_empty() {
        DEFAULT_TENANT.to_string()
    } else {
        creds.tenant_id.trim().to_string()
    }
}

/// Build the Microsoft authorization URL for the given tenant + loopback
/// `redirect_uri`.
fn build_auth_url(creds: &OneDriveCreds, tenant: &str, redirect_uri: &str) -> String {
    format!(
        "https://login.microsoftonline.com/{tenant}/oauth2/v2.0/authorize\
         ?client_id={client_id}\
         &redirect_uri={redirect_uri}\
         &response_type=code\
         &scope={scope}\
         &response_mode=query\
         &prompt=select_account",
        tenant = urlencode(tenant),
        client_id = urlencode(creds.client_id),
        redirect_uri = urlencode(redirect_uri),
        scope = urlencode(SCOPE),
    )
}

/// Manual (no-local-browser) flow, step 1: return `(auth_url, redirect_uri)`.
/// See `dropbox::manual_auth_url` for the relay rationale.
pub fn manual_auth_url(creds: &OneDriveCreds) -> anyhow::Result<(String, String)> {
    let tenant = resolve_tenant(creds);
    let (listener, redirect_uri) = bind_loopback_listener()?;
    drop(listener);
    Ok((build_auth_url(creds, &tenant, &redirect_uri), redirect_uri))
}

pub async fn run_oauth_flow(
    creds: OneDriveCreds<'_>,
    cancel: CancellationToken,
) -> anyhow::Result<OneDriveOAuthResult> {
    let tenant = resolve_tenant(&creds);
    let (listener, redirect_uri) = bind_loopback_listener()?;
    let auth_url = build_auth_url(&creds, &tenant, &redirect_uri);
    open_browser(&auth_url)?;
    let code = wait_for_auth_code(listener, cancel, "Microsoft").await?;
    exchange_code(&creds, &code, &redirect_uri).await
}

/// Exchange an authorization `code` for a refresh token.  `redirect_uri` must
/// match the one the `code` was issued for.
pub async fn exchange_code(
    creds: &OneDriveCreds<'_>,
    code: &str,
    redirect_uri: &str,
) -> anyhow::Result<OneDriveOAuthResult> {
    let tenant = resolve_tenant(creds);
    let token_url = format!("https://login.microsoftonline.com/{tenant}/oauth2/v2.0/token");
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;

    // Public-client OAuth flow.  Nyx Backup is a desktop app; Microsoft's
    // guidance for desktop apps is "public client + loopback redirect"
    // (the Entra "Allow public client flows" toggle our setup doc tells
    // users to enable commits the registration to public-client
    // semantics).  Public clients MUST NOT send client_secret on the
    // token exchange - Microsoft returns AADSTS90023 ("Public clients
    // can't send a client secret") if we do.  Security model: the
    // loopback redirect with a one-shot listener bound to a random high
    // port is what protects this flow.
    //
    // The scope on the token exchange is what gets stamped onto the
    // refresh_token; resend the exact same list to ensure consistency.
    let token_resp = http
        .post(&token_url)
        .form(&[
            ("client_id", creds.client_id),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("grant_type", "authorization_code"),
            ("scope", SCOPE),
        ])
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("Token exchange failed: {e}"))?;

    let token_json: serde_json::Value = token_resp
        .json()
        .await
        .map_err(|e| anyhow::anyhow!("Token response parse failed: {e}"))?;

    let refresh_token = token_json["refresh_token"]
        .as_str()
        .ok_or_else(|| {
            let err = token_json
                .get("error_description")
                .or_else(|| token_json.get("error"))
                .and_then(|v| v.as_str())
                .unwrap_or("no refresh_token in response");
            anyhow::anyhow!("Token exchange failed: {err}")
        })?
        .to_string();

    let access_token = token_json["access_token"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("No access_token in response"))?
        .to_string();

    // (No connect-time scope check for OneDrive: Files.ReadWrite is not a
    // restricted/silently-withheld scope like Google's `drive`, and Microsoft
    // echoes scopes inconsistently, so a substring check risks false-rejecting
    // a valid token.  Google keeps its check because the restricted `drive`
    // scope genuinely can be granted-minus-drive.)

    // /me returns the signed-in user.  Different account types put the
    // email in different fields: personal MS accounts use `userPrincipalName`,
    // work / school often have it in `mail`.  Fall back through both.
    let email = match http
        .get(USERINFO_URL)
        .bearer_auth(&access_token)
        .send()
        .await
    {
        Ok(resp) => {
            let info_json: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
            info_json["mail"]
                .as_str()
                .or_else(|| info_json["userPrincipalName"].as_str())
                .unwrap_or("")
                .to_string()
        }
        Err(_) => String::new(),
    };

    Ok(OneDriveOAuthResult {
        refresh_token,
        tenant_id: tenant,
        email,
    })
}
