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

mod logo;

/// Shared page chrome for the loopback redirect responses: dark-themed,
/// fully self-contained (no external assets - the browser reaches this over a
/// one-shot loopback socket with no network), centered card with the Nyx Backup
/// Recovery icon.  `accent` is the heading color; `title` / `body` are
/// already-escaped plain text.
fn redirect_html(accent: &str, title: &str, body: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset='utf-8'>\
         <meta name='viewport' content='width=device-width,initial-scale=1'>\
         <title>Nyx Backup Recovery</title></head>\
         <body style='margin:0;min-height:100vh;display:flex;align-items:center;\
         justify-content:center;background:#0f1020;color:#e8e8f0;\
         font-family:system-ui,-apple-system,Segoe UI,Roboto,sans-serif'>\
         <div style='text-align:center;padding:2.5em 3em;background:#1a1b2e;\
         border:1px solid #2a2b45;border-radius:14px;max-width:24em'>\
         <img src='data:image/png;base64,{icon}' width='48' height='48' alt='' \
         style='display:block;margin:0 auto .6em;border-radius:10px'/>\
         <div style='font-size:1.4em;font-weight:700;color:#a78bfa;\
         letter-spacing:.02em'>Nyx Backup Recovery</div>\
         <h2 style='margin:.6em 0 .3em;font-size:1.1em;color:{accent}'>{title}</h2>\
         <p style='margin:0;color:#a8a8c0;line-height:1.5'>{body}</p>\
         </div></body></html>",
        icon = logo::RECOVERY_ICON_PNG_B64,
    )
}

/// Success page shown after the browser delivers the authorization code.
fn html_ok(provider: &str) -> String {
    redirect_html(
        "#34d399",
        &format!("Connected to {provider}"),
        "You can close this tab and return to Nyx Backup Recovery.",
    )
}

/// Failure page shown when the provider returns `?error=` on the redirect.
fn html_err(provider: &str) -> String {
    redirect_html(
        "#f87171",
        &format!("{provider} authorization failed"),
        "Close this tab and return to Nyx Backup Recovery for details.",
    )
}

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

/// Decode one `application/x-www-form-urlencoded` value (RFC 6749 s.3.1
/// requires OAuth redirect params to use this encoding): `+` becomes a
/// space, then each `%XX` escape becomes its byte value.
///
/// This MUST run on the extracted authorization code before the token
/// exchange re-encodes it.  Microsoft personal-account (MSA) codes look
/// like `M.C515_...!...` and routinely contain `+` and `/`, which arrive
/// percent-encoded (`%2B`, `%2F`) in the loopback redirect.  Without
/// decoding here, the stray `%` gets encoded a second time by reqwest's
/// form serializer and Entra rejects the code with AADSTS70000 ("the
/// provided value for the 'code' parameter is not valid").  Google and
/// Dropbox codes are URL-safe, so this is a no-op for them.
fn form_urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                match (hi, lo) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 3;
                    }
                    // Not a valid escape - keep the literal '%' and move on.
                    _ => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn extract_query_param(line: &str, key: &str) -> Option<String> {
    let path = line.split_whitespace().nth(1)?;
    let query = path.split('?').nth(1)?;
    for part in query.split('&') {
        if let Some(rest) = part.strip_prefix(&format!("{key}=")) {
            return Some(form_urldecode(rest));
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
                let page = html_err(provider_name);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    page.len(),
                    page,
                );
                let _ = reader.get_mut().write_all(response.as_bytes()).await;
                return Err(anyhow::anyhow!("{provider_name} declined access: {err}"));
            }

            let page = html_ok(provider_name);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                page.len(),
                page,
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

#[cfg(test)]
mod tests {
    use super::{extract_query_param, form_urldecode};

    #[test]
    fn urlsafe_code_is_unchanged() {
        assert_eq!(form_urldecode("4-0AeanS0bXyZ_.~"), "4-0AeanS0bXyZ_.~");
    }

    #[test]
    fn percent_and_plus_are_decoded_once() {
        assert_eq!(form_urldecode("a%2Bb%2Fc"), "a+b/c");
        assert_eq!(form_urldecode("a+b"), "a b");
        assert_eq!(form_urldecode("ab%"), "ab%");
        assert_eq!(form_urldecode("ab%2"), "ab%2");
    }

    #[test]
    fn extract_decodes_msa_authorization_code() {
        let line = "GET /?code=M.C515_SN1.2.U%2BMsaArtifacts%2Fx6ATz&state=n HTTP/1.1";
        assert_eq!(
            extract_query_param(line, "code").as_deref(),
            Some("M.C515_SN1.2.U+MsaArtifacts/x6ATz")
        );
    }
}
