// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! Exponential back-off retry decorator for any [`StorageBackend`].
//!
//! Wraps an inner backend and transparently retries any operation that
//! returns a *transient* error (connection reset, timeout, HTTP 429/5xx, …)
//! using truncated exponential back-off with ±25 % jitter.
//!
//! Non-transient errors (auth failures, object not found, permission denied,
//! bad request, …) propagate immediately without consuming retry budget.
//!
//! # Wiring
//!
//! Applied automatically by [`crate::registry::build_backend`] so every
//! backend - S3, Azure, GCS, B2, SFTP, SMB, local - gets retry behaviour
//! without any per-backend code.
//!
//! # Tuning
//!
//! The default [`RetryConfig`] is:
//!
//! | Parameter       | Default           |
//! |-----------------|-------------------|
//! | `max_retries`   | indefinite        |
//! | `base_delay`    | 2 s               |
//! | `max_delay`     | 120 s             |
//! | `multiplier`    | 2.0               |
//! | `jitter`        | ±25 %             |
//!
//! With `max_retries = u32::MAX` the retry loop runs until success or until
//! the backup is explicitly cancelled by the user.  The delay saturates at
//! `max_delay` (120 s ± jitter ≈ 90-150 s) so a 1-hour network outage
//! produces roughly 30 retry attempts spaced ~2 minutes apart; when the
//! network returns the next attempt succeeds and the backup continues.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rand::Rng;
use tracing::warn;

use bkp_types::error::{Error, Result};

use crate::backend::{ObjectPath, StorageBackend};

// - Config ----------------------------------

/// Retry policy for [`RetryBackend`].
#[derive(Clone, Debug)]
pub struct RetryConfig {
    /// Maximum number of *additional* attempts after the first.
    pub max_retries: u32,
    /// Delay before the first retry.
    pub base_delay: Duration,
    /// Upper bound on the delay (before jitter is applied).
    pub max_delay: Duration,
    /// Back-off multiplier applied to the delay after each attempt.
    pub multiplier: f64,
    /// Jitter fraction applied symmetrically, e.g. `0.25` → ±25 %.
    pub jitter: f64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: u32::MAX, // retry indefinitely until success or cancel
            base_delay: Duration::from_secs(2),
            max_delay: Duration::from_secs(120),
            multiplier: 2.0,
            jitter: 0.25,
        }
    }
}

// - Transient-error classification ----------------------

/// Return `true` if `err` is a transient condition that is worth retrying.
///
/// Transient means: the server or network is temporarily unavailable, not
/// that the request itself is invalid.  Auth errors, not-found, bad-request,
/// and crypto errors are never transient.
fn is_transient(err: &Error) -> bool {
    match err {
        Error::Io(e) => matches!(
            e.kind(),
            std::io::ErrorKind::ConnectionRefused
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::TimedOut
                | std::io::ErrorKind::WouldBlock
                | std::io::ErrorKind::Interrupted
                | std::io::ErrorKind::UnexpectedEof
        ),
        Error::Storage(msg) => is_transient_msg(msg),
        _ => false,
    }
}

fn is_transient_msg(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();

    // Helper: does `m` contain `code` as a bona fide HTTP status, not just
    // as random digits inside a UUID or elapsed-time number like "181.2403s"
    // (which used to false-match "403" and break retry).  Accept the code
    // when preceded by a non-digit or start-of-string and followed by a
    // non-digit or end-of-string.
    fn has_http_code(m: &str, code: &str) -> bool {
        let bytes = m.as_bytes();
        let code_b = code.as_bytes();
        let mut i = 0;
        while i + code_b.len() <= bytes.len() {
            if &bytes[i..i + code_b.len()] == code_b {
                let before_ok = i == 0 || !bytes[i - 1].is_ascii_digit();
                let after_ok =
                    i + code_b.len() == bytes.len() || !bytes[i + code_b.len()].is_ascii_digit();
                if before_ok && after_ok {
                    return true;
                }
            }
            i += 1;
        }
        false
    }

    // Explicit non-transient patterns - phrase-based first (unambiguous),
    // numeric codes only via the boundary-checking helper so timestamps
    // like "1.2403027s" do not get misread as a 403.
    if m.contains("not found")
        || m.contains("forbidden")
        || m.contains("unauthorized")
        || m.contains("bad request")
        || has_http_code(&m, "404")
        || has_http_code(&m, "403")
        || has_http_code(&m, "401")
        || has_http_code(&m, "400")
    {
        return false;
    }
    // Network-level transients
    m.contains("connection refused")
        || m.contains("connection reset")
        || m.contains("connection aborted")
        || m.contains("broken pipe")
        || m.contains("timed out")
        || m.contains("timeout")
        || m.contains("network unreachable")
        || m.contains("no route to host")
        || m.contains("host unreachable")
        || m.contains("name resolution")
        || m.contains("unexpected eof")
        || m.contains("end of file")
        || m.contains("error sending request") // reqwest transport error
        || m.contains("error decoding response body") // reqwest mid-stream reset on GET body
        || m.contains("body decode")
        || m.contains("incomplete message") // hyper HTTP/2 partial frame
        || m.contains("stream reset")
        || m.contains("stream closed")
        || m.contains("connection closed")
        // HTTP throttle / server-side transients - via boundary helper
        || has_http_code(&m, "429")
        || has_http_code(&m, "500")
        || has_http_code(&m, "502")
        || has_http_code(&m, "503")
        || has_http_code(&m, "504")
        || m.contains("too many requests")
        || m.contains("service unavailable")
        || m.contains("bad gateway")
        || m.contains("gateway timeout")
        || m.contains("internal server error")
}

// - Retry helper -------------------------------

fn jittered(base: Duration, jitter_fraction: f64) -> Duration {
    let factor = 1.0 + rand::thread_rng().gen_range(-jitter_fraction..=jitter_fraction);
    Duration::from_secs_f64((base.as_secs_f64() * factor).max(0.0))
}

async fn with_retry<F, Fut, T>(cfg: &RetryConfig, label: &str, mut f: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut attempt = 0u32;
    let mut delay = cfg.base_delay;
    loop {
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) if !is_transient(&e) => return Err(e),
            Err(e) if attempt >= cfg.max_retries => {
                warn!(target: "bkp_storage",
                      "{label}: transient error exhausted {} retries: {e}", cfg.max_retries);
                return Err(e);
            }
            Err(e) => {
                let sleep = jittered(delay, cfg.jitter);
                // max_retries==u32::MAX is the "retry indefinitely" sentinel
                // (see RetryConfig::default).  Adding 1 would wrap to 0 and
                // render as "attempt N/0".  Use a clear label instead.
                let max_label = if cfg.max_retries == u32::MAX {
                    "∞".to_string()
                } else {
                    (cfg.max_retries + 1).to_string()
                };
                warn!(target: "bkp_storage",
                      "{label}: transient error (attempt {}/{}) - retry in {:.1}s: {e}",
                      attempt + 1, max_label, sleep.as_secs_f64());
                tokio::time::sleep(sleep).await;
                delay = delay.mul_f64(cfg.multiplier).min(cfg.max_delay);
                attempt += 1;
            }
        }
    }
}

// - RetryBackend -------------------------------

/// [`StorageBackend`] decorator that retries transient errors with
/// exponential back-off and jitter.
pub struct RetryBackend {
    inner: Arc<dyn StorageBackend>,
    cfg: RetryConfig,
}

impl RetryBackend {
    /// Wrap `inner` with the given retry policy.
    pub fn new(inner: Arc<dyn StorageBackend>, cfg: RetryConfig) -> Self {
        Self { inner, cfg }
    }
}

#[async_trait]
impl StorageBackend for RetryBackend {
    async fn get(&self, path: &str) -> Result<Vec<u8>> {
        let inner = Arc::clone(&self.inner);
        let path = path.to_string();
        with_retry(&self.cfg, &format!("get:{path}"), || {
            let inner = Arc::clone(&inner);
            let path = path.clone();
            async move { inner.get(&path).await }
        })
        .await
    }

    // Deliberately NOT wrapped in with_retry: probe_access backs the
    // connection test and the token-health check, which want a fast,
    // decisive yes/no.  The indefinite-retry loop (max_retries = u32::MAX,
    // 2 s base backoff) would turn a flaky probe into a multi-minute hang.
    // Fail fast and let the caller re-probe.
    async fn probe_access(&self) -> Result<()> {
        self.inner.probe_access().await
    }

    async fn exists(&self, path: &str) -> Result<bool> {
        let inner = Arc::clone(&self.inner);
        let path = path.to_string();
        with_retry(&self.cfg, &format!("exists:{path}"), || {
            let inner = Arc::clone(&inner);
            let path = path.clone();
            async move { inner.exists(&path).await }
        })
        .await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<ObjectPath>> {
        let inner = Arc::clone(&self.inner);
        let prefix = prefix.to_string();
        with_retry(&self.cfg, &format!("list:{prefix}"), || {
            let inner = Arc::clone(&inner);
            let prefix = prefix.clone();
            async move { inner.list(&prefix).await }
        })
        .await
    }

    async fn list_with_sizes(&self, prefix: &str) -> Result<Vec<(ObjectPath, u64)>> {
        let inner = Arc::clone(&self.inner);
        let prefix = prefix.to_string();
        with_retry(&self.cfg, &format!("list_with_sizes:{prefix}"), || {
            let inner = Arc::clone(&inner);
            let prefix = prefix.clone();
            async move { inner.list_with_sizes(&prefix).await }
        })
        .await
    }

    async fn size(&self, path: &str) -> Result<u64> {
        let inner = Arc::clone(&self.inner);
        let path = path.to_string();
        with_retry(&self.cfg, &format!("size:{path}"), || {
            let inner = Arc::clone(&inner);
            let path = path.clone();
            async move { inner.size(&path).await }
        })
        .await
    }

    async fn head_with_hash(&self, path: &str) -> Result<(u64, String, String)> {
        // Without this forward the call hits the trait's default impl
        // which returns Error::Storage("head_with_hash unsupported"),
        // and the quick integrity audit falls back to existence-only
        // HEAD - even when the underlying backend (S3 / Azure / GCS /
        // B2) has a native implementation.
        let inner = Arc::clone(&self.inner);
        let path = path.to_string();
        with_retry(&self.cfg, &format!("head_with_hash:{path}"), || {
            let inner = Arc::clone(&inner);
            let path = path.clone();
            async move { inner.head_with_hash(&path).await }
        })
        .await
    }

    async fn get_range(&self, path: &str, from: u64, to: u64) -> Result<Vec<u8>> {
        let inner = Arc::clone(&self.inner);
        let path = path.to_string();
        with_retry(
            &self.cfg,
            &format!("get_range:{path}[{from}..{to}]"),
            || {
                let inner = Arc::clone(&inner);
                let path = path.clone();
                async move { inner.get_range(&path, from, to).await }
            },
        )
        .await
    }

    fn display_name(&self) -> String {
        self.inner.display_name()
    }

    fn concurrency_hint(&self) -> Option<usize> {
        self.inner.concurrency_hint()
    }

    // RetryBackend wraps EVERY backend (see registry::build_backend) - if
    // we don't forward these, archive-tier detection (probe_pack_accessible)
    // silently returns Ok(true) for every pack and the Glacier-thaw POST
    // (initiate_pack_restore) is a no-op.  Both of those are silent
    // correctness bugs for any user on an archive storage class.
    async fn probe_pack_accessible(&self, path: &str) -> Result<bool> {
        self.inner.probe_pack_accessible(path).await
    }

    async fn initiate_pack_restore(&self, path: &str) -> Result<()> {
        self.inner.initiate_pack_restore(path).await
    }
}

// - Tests -----------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_io_kinds() {
        assert!(is_transient(&Error::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "reset"
        ))));
        assert!(is_transient(&Error::Io(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "timeout"
        ))));
        assert!(!is_transient(&Error::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "denied"
        ))));
    }

    #[test]
    fn transient_storage_msgs() {
        assert!(is_transient(&Error::Storage(
            "connection reset by peer".into()
        )));
        assert!(is_transient(&Error::Storage(
            "status 503 service unavailable".into()
        )));
        assert!(is_transient(&Error::Storage(
            "rate limit: 429 too many requests".into()
        )));
        assert!(!is_transient(&Error::Storage(
            "status 403 forbidden".into()
        )));
        assert!(!is_transient(&Error::Storage(
            "status 404 not found".into()
        )));
        assert!(!is_transient(&Error::Storage(
            "status 400 bad request".into()
        )));
        // UUID/timestamp containing "503" must not promote a 404 to transient
        assert!(!is_transient(&Error::Storage(
            "Azure get idx/snapshot-index: not found (404): RequestId:abc-503d-xyz".into()
        )));
        assert!(!is_transient(&Error::Config("bad config".into())));
    }

    /// Regression: an elapsed-time substring like `181.2403027s` contains
    /// the bytes "403".  Naive `contains("403")` used to classify the
    /// outer S3 transport error as non-transient and short-circuit the
    /// retry loop.  The boundary helper must reject digit-adjacent matches.
    #[test]
    fn timestamp_digits_dont_match_http_codes() {
        // The exact shape of the wrapped error from object_store after its
        // inner retries time out: contains "Error after 5 retries in
        // 181.2403027s" - the "403" lives in the elapsed time, not as a
        // status code, and the underlying cause is a transport failure.
        let msg = "S3 put packs/abc.pack: Generic S3 error: Error after 5 retries \
                   in 181.2403027s, max_retries:10, retry_timeout:180s, \
                   source:error sending request for url (https://s3...)";
        assert!(
            is_transient(&Error::Storage(msg.into())),
            "elapsed-time digits must not promote transport error to non-transient"
        );
    }

    /// Conversely a real HTTP 403 - with the code at a word boundary -
    /// must still be classified non-transient.
    #[test]
    fn real_http_codes_still_classified() {
        assert!(!is_transient(&Error::Storage("HTTP 403".into())));
        assert!(!is_transient(&Error::Storage("status: 403".into())));
        assert!(!is_transient(&Error::Storage("(404)".into())));
        assert!(is_transient(&Error::Storage(
            "HTTP 503 service unavailable".into()
        )));
    }

    /// Reqwest's "error sending request for url" - the bare transport
    /// failure that wraps DNS/TLS/connection issues - must be transient.
    #[test]
    fn reqwest_transport_error_is_transient() {
        let msg = "reqwest error: error sending request for url (https://example.com/x)";
        assert!(is_transient(&Error::Storage(msg.into())));
    }

    /// Mid-stream body reset on a GET: reqwest reports "error decoding
    /// response body".  Common when downloading a 256 MiB pack and the
    /// TLS connection drops mid-transfer.  Must retry, not abort.
    #[test]
    fn reqwest_body_decode_error_is_transient() {
        let msg = "Storage error: S3 get body packs/abc.pack: Generic S3 error: error decoding response body";
        assert!(is_transient(&Error::Storage(msg.into())));
    }

    /// HTTP/2 stream-level errors carry phrases like "stream reset" /
    /// "incomplete message" / "stream closed".  All transient.
    #[test]
    fn http2_stream_errors_are_transient() {
        assert!(is_transient(&Error::Storage(
            "hyper: stream reset by peer".into()
        )));
        assert!(is_transient(&Error::Storage("h2: stream closed".into())));
        assert!(is_transient(&Error::Storage(
            "hyper: incomplete message: connection reset".into()
        )));
    }
}
