// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! StorageBackend trait - the core abstraction for all storage endpoints.
//!
//! Every endpoint (S3, Azure, SFTP, local, ...) implements this trait.
//! The backup engine and restore engine operate exclusively through this trait,
//! making endpoints interchangeable and testable with a mock backend.

use bkp_types::error::{Error, Result};

/// Opaque identifier for a stored object (remote path string).
pub type ObjectPath = String;

/// StorageBackend defines the minimal interface required by the backup engine.
///
/// All implementations must be Send + Sync for use across async task boundaries.
#[async_trait::async_trait]
pub trait StorageBackend: Send + Sync {
    /// Read the object at `path` and return its bytes.
    async fn get(&self, path: &str) -> Result<Vec<u8>>;

    /// Return true if an object exists at `path`.
    async fn exists(&self, path: &str) -> Result<bool>;

    /// List all object paths with the given `prefix`.
    async fn list(&self, prefix: &str) -> Result<Vec<ObjectPath>>;

    /// Cheaply confirm the configured root / bucket is reachable and the
    /// credentials (or OAuth token) are valid, WITHOUT enumerating contents.
    ///
    /// Used by the GUI/TUI "Test connection" path and by proactive
    /// token-health checks.  This is REQUIRED (no default): a
    /// default that fell back to `list("")` would be a silent performance
    /// trap.  On the OAuth backends (OneDrive / Google Drive / Dropbox)
    /// `list("")` recurses one level into every subfolder - one round trip
    /// each - turning a reachability check into an 8-12 s directory walk
    /// on a large object-store prefix it paginates
    /// every object.  Each backend must implement a single bounded call:
    /// bucket / drive metadata, a `max-keys=1` list, or a root `stat`.  Do
    /// NOT implement this as `self.list("")`.
    async fn probe_access(&self) -> Result<()>;

    /// List all objects with the given `prefix`, returning `(path, byte_size)` pairs.
    ///
    /// The default implementation calls `list()` then issues concurrent `size()` calls
    /// (up to 32 in flight at once).  Backends whose list responses already include
    /// object sizes (S3, Azure, GCS, B2, S3-compat) should override this to avoid the
    /// extra round-trips.
    async fn list_with_sizes(&self, prefix: &str) -> Result<Vec<(ObjectPath, u64)>> {
        use futures::stream::{self, StreamExt};
        let paths = self.list(prefix).await?;
        let results: Vec<Result<(ObjectPath, u64)>> = stream::iter(paths)
            .map(|path| async move {
                let sz = self.size(&path).await?;
                Ok((path, sz))
            })
            .buffer_unordered(32)
            .collect()
            .await;
        results.into_iter().collect()
    }

    /// Return the byte length of the object at `path` without downloading it.
    async fn size(&self, path: &str) -> Result<u64>;

    /// Read a byte range `[from, to)` from the object at `path`.
    ///
    /// This maps to an HTTP `Range: bytes=from-(to-1)` request on backends that
    /// support it (S3, Azure, B2, GCS), avoiding a full object download when only
    /// the pack footer index needs to be read.
    ///
    /// Fetch a byte range of an object.
    ///
    /// This is REQUIRED (no default) - the previous default fell back to a
    /// whole-object `get` + slice, which is a silent performance disaster
    /// when restore engines call this for tiny chunk reads (15 KB lookups
    /// downloading 256 MiB packs).  Backends that genuinely cannot do
    /// partial reads (SFTP/SMB on some servers) must implement this
    /// explicitly and document the fallback, so it's visible at the
    /// backend boundary instead of buried in a trait default.  See the
    /// S3CompatBackend and DropboxBackend both
    /// silently took this default for months.
    async fn get_range(&self, path: &str, from: u64, to: u64) -> Result<Vec<u8>>;

    /// Read the backend's recorded hash + size for an object without
    /// downloading it.  Used by the quick-integrity audit to detect
    /// cloud-side bit rot or accidental object deletion in seconds.
    ///
    /// Returns `(size_bytes, hash, algo)` where `algo` is one of
    /// `"etag"` / `"md5"` / `"crc32c"` / `"sha1"` etc. - whatever the
    /// backend's own metadata exposes.  The caller only checks that
    /// `(hash, algo)` matches what was recorded at upload time.
    ///
    /// Default implementation returns `Error::Storage("unsupported")`;
    /// backends with native object hashes (S3 / Azure / GCS / B2)
    /// override.  Backends without (SFTP / Dropbox / Local / SMB)
    /// stay on the default - the audit falls back to an existence-only
    /// HEAD via `size()` for them.
    async fn head_with_hash(&self, _path: &str) -> Result<(u64, String, String)> {
        Err(Error::Storage(
            "head_with_hash unsupported on this backend".into(),
        ))
    }

    /// Human-readable name for log messages, e.g. "s3://my-bucket/prefix".
    fn display_name(&self) -> String;

    /// Suggested maximum number of concurrent operations for this backend.
    ///
    /// Returns `None` to use the caller's default (typically 8).
    /// Backends that open a new network connection per call (SFTP, SMB) should
    /// return a small value to avoid overwhelming the server's connection limit.
    fn concurrency_hint(&self) -> Option<usize> {
        None
    }

    /// Probe whether the pack at `path` is accessible for a range read.
    ///
    /// Returns `Ok(true)` when the pack can be downloaded, `Ok(false)` when it is
    /// in an archive storage tier (e.g. S3 Glacier / Deep Archive) and must first
    /// be retrieved.  Default: always accessible - non-archive backends return true.
    async fn probe_pack_accessible(&self, path: &str) -> Result<bool> {
        let _ = path;
        Ok(true)
    }

    /// Initiate an archive retrieval for the pack at `path`.
    ///
    /// Asynchronously requests the backend to begin thawing the object so it can
    /// be read in a future operation.  Only meaningful for archive-tier S3 objects.
    /// Default: no-op - non-archive backends always succeed immediately.
    async fn initiate_pack_restore(&self, path: &str) -> Result<()> {
        let _ = path;
        Ok(())
    }

    // - Critical-object duplication -----------------

    /// Read the object at `path`.  Falls back to `<path>.bak` on failure.
    ///
    /// Returns the original primary error if BOTH copies fail.
    async fn get_critical(&self, path: &str) -> Result<Vec<u8>> {
        match self.get(path).await {
            Ok(data) => Ok(data),
            Err(primary_err) => {
                let bak_path = format!("{path}.bak");
                match self.get(&bak_path).await {
                    Ok(data) => {
                        tracing::warn!(target: "bkp_storage",
                            "primary critical object {path} failed ({primary_err}); \
                             recovered from .bak");
                        Ok(data)
                    }
                    Err(bak_err) => {
                        tracing::error!(target: "bkp_storage",
                            "CRITICAL: both primary and .bak failed for {path}: \
                             primary={primary_err}; bak={bak_err}");
                        Err(primary_err)
                    }
                }
            }
        }
    }
}
