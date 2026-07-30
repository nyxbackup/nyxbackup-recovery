// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! Shared OAuth 2.0 authorization-code flow for Nyx Backup's bundled
//! cloud-storage app credentials (Google Drive, Dropbox).  Used by both
//! the GUI (`bkp-gui`) and TUI (`bkp-tui`).
//!
//! The flow is identical across providers:
//! 1. Bind a loopback `127.0.0.1:0` HTTP listener.
//! 2. Open the provider's consent screen in the user's default browser.
//! 3. Wait (up to 90 s) for `?code=` or `?error=` on the redirect.
//! 4. Serve a "you can close this tab" page back.
//! 5. POST the code to the provider's token endpoint.
//! 6. Fetch the user's email via the provider's userinfo endpoint.
//!
//! Caller passes its bundled `client_id` / `client_secret` (compiled in
//! via `env!()` in each binary so the embedded creds stay binary-local)
//! and a `CancellationToken` it can flip to abort the wait (e.g. user
//! pressed `Esc` in the TUI, or `cancel-*-oauth` Tauri event fired).

use std::net::TcpListener as StdTcpListener;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader as TokioBufReader};
use tokio::net::TcpListener;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

pub mod dropbox;
pub mod google;
pub mod onedrive;

const HTML_OK: &str = "<html><body style='font-family:sans-serif;text-align:center;padding-top:4em'>\
                       <h2>Connected to Nyx Backup</h2>\
                       <p>You can close this tab and return to the app.</p>\
                       </body></html>";
const HTML_ERR: &str = "<html><body style='font-family:sans-serif;text-align:center;padding-top:4em'>\
                        <h2>Authorization failed</h2>\
                        <p>Return to Nyx Backup for details.</p>\
                        </body></html>";

/// Percent-encode for query strings (RFC 3986 unreserved set).
pub fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

fn extract_query_param(line: &str, key: &str) -> Option<String> {
    let path = line.split_whitespace().nth(1)?;
    let query = path.split('?').nth(1)?;
    for part in query.split('&') {
        if let Some(rest) = part.strip_prefix(&format!("{key}=")) {
            return Some(rest.replace('+', " "));
        }
    }
    None
}

/// Bind a loopback HTTP listener on a random free port and return both
/// the listener and the `http://localhost:<port>` redirect URI.
pub fn bind_loopback_listener() -> anyhow::Result<(TcpListener, String)> {
    let std_listener = StdTcpListener::bind("127.0.0.1:0")?;
    std_listener.set_nonblocking(true)?;
    let port = std_listener.local_addr()?.port();
    let redirect_uri = format!("http://localhost:{port}");
    let listener = TcpListener::from_std(std_listener)?;
    Ok((listener, redirect_uri))
}

/// Wait for the browser to redirect to the loopback listener with a
/// `?code=` (success) or `?error=` (denial) query parameter.  Times out
/// after 90 seconds; cancels immediately if `cancel` is triggered.
///
/// `provider_name` is used in error strings only ("Google declined…",
/// "Timed out waiting for Dropbox…").
pub async fn wait_for_auth_code(
    listener: TcpListener,
    cancel: CancellationToken,
    provider_name: &str,
) -> anyhow::Result<String> {
    let accept_fut = timeout(Duration::from_secs(90), async {
        loop {
            let (stream, _) = listener.accept().await?;
            let mut reader = TokioBufReader::new(stream);

            let mut request_line = String::new();
            reader.read_line(&mut request_line).await?;
            // Drain remaining headers up to the blank line.
            loop {
                let mut hdr = String::new();
                reader.read_line(&mut hdr).await?;
                if hdr == "\r\n" || hdr.is_empty() {
                    break;
                }
            }

            if let Some(err) = extract_query_param(&request_line, "error") {
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    HTML_ERR.len(),
                    HTML_ERR,
                );
                let _ = reader.get_mut().write_all(response.as_bytes()).await;
                return Err(anyhow::anyhow!("{provider_name} declined access: {err}"));
            }

            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                HTML_OK.len(),
                HTML_OK,
            );
            reader.get_mut().write_all(response.as_bytes()).await?;

            if let Some(code) = extract_query_param(&request_line, "code") {
                return Ok::<String, anyhow::Error>(code);
            }
        }
    });

    tokio::select! {
        r = accept_fut => {
            r.map_err(|_| anyhow::anyhow!("Timed out waiting for {provider_name} authorization (90 seconds)."))?
                .map_err(|e| anyhow::anyhow!("{e}"))
        }
        _ = cancel.cancelled() => {
            Err(anyhow::anyhow!("Authorization cancelled."))
        }
    }
}

/// Open `url` in the user's default browser.  Returns an error if no
/// browser could be launched (rare on desktops; common on truly headless
/// servers where the TUI is the only available client).
pub fn open_browser(url: &str) -> anyhow::Result<()> {
    webbrowser::open(url).map_err(|e| anyhow::anyhow!("Could not open browser: {e}"))
}
