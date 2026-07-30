// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! Dropbox OAuth 2.0 authorization-code flow.

use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::{bind_loopback_listener, open_browser, urlencode, wait_for_auth_code};

const TOKEN_URL: &str = "https://api.dropboxapi.com/oauth2/token";
const ACCOUNT_URL: &str = "https://api.dropboxapi.com/2/users/get_current_account";

/// Bundled OAuth app credentials.  Pass `env!("DROPBOX_APP_KEY")` and
/// `env!("DROPBOX_APP_SECRET")` from the caller so the secrets stay
/// embedded in each individual binary, not in this shared crate.
pub struct DropboxCreds<'a> {
    pub app_key: &'a str,
    pub app_secret: &'a str,
}

#[derive(Debug, Clone)]
pub struct DropboxOAuthResult {
    pub refresh_token: String,
    pub email: String,
}

/// Build the Dropbox authorization URL for the given loopback `redirect_uri`.
fn build_auth_url(creds: &DropboxCreds, redirect_uri: &str) -> String {
    format!(
        "https://www.dropbox.com/oauth2/authorize\
         ?client_id={client_id}\
         &redirect_uri={redirect_uri}\
         &response_type=code\
         &token_access_type=offline",
        client_id = urlencode(creds.app_key),
        redirect_uri = urlencode(redirect_uri),
    )
}

/// Manual (no-local-browser) flow, step 1: return `(auth_url, redirect_uri)`.
/// The caller shows the URL; the user authorizes on any browser, copies the
/// `code` from the (failed) localhost redirect, and passes both back to
/// [`exchange_code`].  No listener is held - the redirect lands on whatever
/// machine ran the browser, so we never see it.
pub fn manual_auth_url(creds: &DropboxCreds) -> anyhow::Result<(String, String)> {
    let (listener, redirect_uri) = bind_loopback_listener()?;
    drop(listener); // manual flow: nothing listens; we only need the URI string
    Ok((build_auth_url(creds, &redirect_uri), redirect_uri))
}

/// Run the full OAuth flow.  `cancel` is checked while waiting for the
/// browser redirect; triggering it aborts with "Authorization cancelled."
pub async fn run_oauth_flow(
    creds: DropboxCreds<'_>,
    cancel: CancellationToken,
) -> anyhow::Result<DropboxOAuthResult> {
    let (listener, redirect_uri) = bind_loopback_listener()?;
    let auth_url = build_auth_url(&creds, &redirect_uri);
    open_browser(&auth_url)?;
    let code = wait_for_auth_code(listener, cancel, "Dropbox").await?;
    exchange_code(&creds, &code, &redirect_uri).await
}

/// Exchange an authorization `code` (obtained via loopback or pasted from a
/// manual redirect) for a refresh token + account email.  `redirect_uri` must
/// match the one the `code` was issued for.
pub async fn exchange_code(
    creds: &DropboxCreds<'_>,
    code: &str,
    redirect_uri: &str,
) -> anyhow::Result<DropboxOAuthResult> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;

    let token_resp = http
        .post(TOKEN_URL)
        .form(&[
            ("client_id", creds.app_key),
            ("client_secret", creds.app_secret),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("grant_type", "authorization_code"),
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

    // (No connect-time scope check for Dropbox: its write scope is configured
    // on the app and not a restricted/silently-withheld scope like Google's
    // `drive`, so a check here only risks false-rejecting valid tokens.

    let account_json: serde_json::Value = http
        .post(ACCOUNT_URL)
        .bearer_auth(&access_token)
        .header("Content-Type", "application/json")
        .body("null")
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("Account info request failed: {e}"))?
        .json()
        .await
        .map_err(|e| anyhow::anyhow!("Account info parse failed: {e}"))?;

    let email = account_json["email"].as_str().unwrap_or("").to_string();

    Ok(DropboxOAuthResult {
        refresh_token,
        email,
    })
}
