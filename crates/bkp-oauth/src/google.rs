// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! Google Drive OAuth 2.0 authorization-code flow.

use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::{bind_loopback_listener, open_browser, urlencode, wait_for_auth_code};

const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const USERINFO_URL: &str = "https://www.googleapis.com/oauth2/v2/userinfo";
// full `drive` scope, not `drive.file`.  The user pastes a
// Drive folder URL/ID they pre-created and the backend lists +
// uploads + deletes inside it.  `drive.file` only grants access to
// files the app itself created, so the very first DriveFiles.List
// against the user's folder fails with
// "Request had insufficient authentication scopes" /
// ACCESS_TOKEN_SCOPE_INSUFFICIENT.  Standard practice for backup
// tools that target Drive (rclone, Duplicacy, restic-via-rclone)
// is to request the full `drive` scope - this matches user
// expectation when granting "access to my Drive for backup".
const SCOPE: &str = "https://www.googleapis.com/auth/drive openid email";

/// Bundled OAuth client credentials.  Pass `env!("GOOGLE_OAUTH_CLIENT_ID")`
/// and `env!("GOOGLE_OAUTH_CLIENT_SECRET")` from the caller so the secrets
/// stay embedded in each individual binary, not in this shared crate.
pub struct GoogleCreds<'a> {
    pub client_id: &'a str,
    pub client_secret: &'a str,
}

#[derive(Debug, Clone)]
pub struct GoogleDriveOAuthResult {
    pub folder_id: String,
    pub refresh_token: String,
    pub email: String,
}

/// Extract the Drive folder ID from a full folder URL, or return the input
/// as-is if it already looks like a bare folder ID.
pub fn extract_folder_id(input: &str) -> String {
    let input = input.trim();
    if let Some(pos) = input.find("/folders/") {
        let after = &input[pos + "/folders/".len()..];
        let id = after.split(['?', '&', '#']).next().unwrap_or(after);
        return id.trim().to_string();
    }
    input.to_string()
}

/// Build the Google authorization URL for the given loopback `redirect_uri`.
fn build_auth_url(creds: &GoogleCreds, redirect_uri: &str) -> String {
    format!(
        "https://accounts.google.com/o/oauth2/v2/auth\
         ?client_id={client_id}\
         &redirect_uri={redirect_uri}\
         &response_type=code\
         &scope={scope}\
         &access_type=offline\
         &prompt=consent",
        client_id = urlencode(creds.client_id),
        redirect_uri = urlencode(redirect_uri),
        scope = urlencode(SCOPE),
    )
}

/// Manual (no-local-browser) flow, step 1: return `(auth_url, redirect_uri)`.
/// See `dropbox::manual_auth_url` for the relay rationale.
pub fn manual_auth_url(creds: &GoogleCreds) -> anyhow::Result<(String, String)> {
    let (listener, redirect_uri) = bind_loopback_listener()?;
    drop(listener);
    Ok((build_auth_url(creds, &redirect_uri), redirect_uri))
}

/// Run the full OAuth flow.  `cancel` is checked while waiting for the
/// browser redirect; triggering it aborts with "Authorization cancelled."
pub async fn run_oauth_flow(
    folder_url: &str,
    creds: GoogleCreds<'_>,
    cancel: CancellationToken,
) -> anyhow::Result<GoogleDriveOAuthResult> {
    let (listener, redirect_uri) = bind_loopback_listener()?;
    let auth_url = build_auth_url(&creds, &redirect_uri);
    open_browser(&auth_url)?;
    let code = wait_for_auth_code(listener, cancel, "Google").await?;
    exchange_code(&creds, folder_url, &code, &redirect_uri).await
}

/// Exchange an authorization `code` for a refresh token, validating the
/// granted Drive scope and resolving the folder ID from `folder_url`.
/// `redirect_uri` must match the one the `code` was issued for.
pub async fn exchange_code(
    creds: &GoogleCreds<'_>,
    folder_url: &str,
    code: &str,
    redirect_uri: &str,
) -> anyhow::Result<GoogleDriveOAuthResult> {
    let folder_id = extract_folder_id(folder_url);
    if folder_id.is_empty() {
        anyhow::bail!("Could not extract folder ID from the provided URL.");
    }

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;

    let token_resp = http
        .post(TOKEN_URL)
        .form(&[
            ("client_id", creds.client_id),
            ("client_secret", creds.client_secret),
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

    // validate the scope Google actually granted before we save the
    // token.  The restricted `drive` scope can be silently withheld (OAuth app
    // not verified, consent reduced, Testing-mode limits); accepting the token
    // anyway defers the failure to the first Drive API call as a cryptic
    // ACCESS_TOKEN_SCOPE_INSUFFICIENT.  Catch it here and name what Google did
    // grant so the fix is obvious.
    let granted = token_json["scope"].as_str().unwrap_or("");
    let has_drive = granted
        .split(' ')
        .any(|s| s == "https://www.googleapis.com/auth/drive");
    if !granted.is_empty() && !has_drive {
        anyhow::bail!(
            "Google did not grant Drive access (granted scopes: [{granted}]). The \
             OAuth app needs the full 'drive' scope approved in the Google Cloud \
             Console (OAuth consent screen -> Scopes), and your account must be a \
             listed Test user if the app is in Testing mode. Fix that, then re-run \
             Connect."
        );
    }

    let info_json: serde_json::Value = http
        .get(USERINFO_URL)
        .bearer_auth(&access_token)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("Userinfo request failed: {e}"))?
        .json()
        .await
        .map_err(|e| anyhow::anyhow!("Userinfo parse failed: {e}"))?;

    let email = info_json["email"].as_str().unwrap_or("").to_string();

    Ok(GoogleDriveOAuthResult {
        folder_id,
        refresh_token,
        email,
    })
}
