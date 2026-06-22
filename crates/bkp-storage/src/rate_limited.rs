// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! Token-bucket bandwidth throttle wrapping any [`StorageBackend`].
//!
//! Construct via [`RateLimitedBackend::new`]; pass `0` for either limit to
//! leave that direction unlimited.
//!
//! ```ignore
//! let inner = S3Backend::new(cfg)?;
//! let throttled = RateLimitedBackend::new(
//!     Arc::new(inner),
//!     512,   // upload:   512 Kbps
//!     2048,  // download: 2 Mbps
//! );
//! ```

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::Mutex;

use bkp_types::error::Result;

use crate::backend::{ObjectPath, StorageBackend};

// - Token bucket -------------------------------

/// Single-direction token-bucket rate limiter.
///
/// Tracks the earliest moment the next operation may start.  Callers
/// pass the number of bytes they are about to transfer; the bucket
/// sleeps until that bandwidth slot is available and then advances
/// `next_allowed` by the corresponding time quantum.
struct Bucket {
    /// Effective throughput in bytes / second.  Zero means unlimited.
    rate_bytes_per_sec: f64,
    /// Earliest time the next operation may start.
    next_allowed: Mutex<Instant>,
}

impl Bucket {
    fn new(kbps: u64) -> Self {
        Self {
            rate_bytes_per_sec: (kbps as f64) * 1024.0,
            next_allowed: Mutex::new(Instant::now()),
        }
    }

    async fn consume(&self, bytes: usize) {
        if self.rate_bytes_per_sec <= 0.0 || bytes == 0 {
            return;
        }
        let delay = Duration::from_secs_f64(bytes as f64 / self.rate_bytes_per_sec);

        let sleep_until = {
            let mut next = self.next_allowed.lock().await;
            let now = Instant::now();
            // Don't let debt accumulate beyond 2 seconds of burst.
            let start = (*next).max(now);
            let max_start = now + Duration::from_secs(2);
            let start = start.min(max_start);
            *next = start + delay;
            start
        };

        let now = Instant::now();
        if sleep_until > now {
            tokio::time::sleep(sleep_until - now).await;
        }
    }
}

// - RateLimitedBackend ----------------------------

/// Wraps any [`StorageBackend`] with per-direction bandwidth throttling.
pub struct RateLimitedBackend {
    inner: Arc<dyn StorageBackend>,
    download: Bucket,
}

impl RateLimitedBackend {
    /// Create a new throttled wrapper.
    ///
    /// * `_upload_kbps`  - accepted for call-site symmetry but unused; the
    ///   recovery tool only downloads.
    /// * `download_kbps` - max download throughput in Kbps; 0 = unlimited.
    pub fn new(inner: Arc<dyn StorageBackend>, _upload_kbps: u64, download_kbps: u64) -> Self {
        Self {
            inner,
            download: Bucket::new(download_kbps),
        }
    }
}

#[async_trait]
impl StorageBackend for RateLimitedBackend {
    async fn get(&self, path: &str) -> Result<Vec<u8>> {
        let data = self.inner.get(path).await?;
        self.download.consume(data.len()).await;
        Ok(data)
    }

    async fn get_range(&self, path: &str, from: u64, to: u64) -> Result<Vec<u8>> {
        let data = self.inner.get_range(path, from, to).await?;
        self.download.consume(data.len()).await;
        Ok(data)
    }

    async fn exists(&self, path: &str) -> Result<bool> {
        self.inner.exists(path).await
    }

    async fn probe_access(&self) -> Result<()> {
        self.inner.probe_access().await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<ObjectPath>> {
        self.inner.list(prefix).await
    }

    async fn list_with_sizes(&self, prefix: &str) -> Result<Vec<(ObjectPath, u64)>> {
        self.inner.list_with_sizes(prefix).await
    }

    async fn size(&self, path: &str) -> Result<u64> {
        self.inner.size(path).await
    }

    async fn head_with_hash(&self, path: &str) -> Result<(u64, String, String)> {
        self.inner.head_with_hash(path).await
    }

    fn display_name(&self) -> String {
        self.inner.display_name()
    }

    // Forward optional hints / probes to the wrapped backend so per-backend
    // tuning (e.g. R2's `concurrency_hint = 2`) survives the rate-limit
    // wrapper instead of silently reverting to the trait defaults.
    fn concurrency_hint(&self) -> Option<usize> {
        self.inner.concurrency_hint()
    }

    async fn probe_pack_accessible(&self, path: &str) -> Result<bool> {
        self.inner.probe_pack_accessible(path).await
    }

    async fn initiate_pack_restore(&self, path: &str) -> Result<()> {
        self.inner.initiate_pack_restore(path).await
    }
}
