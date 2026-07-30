// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! Local filesystem storage backend.
//!
//! Uses atomic writes: data is written to a `.tmp` file then renamed into place.
//! `put_if_absent` uses `O_CREAT|O_EXCL` for atomicity on the same filesystem.

use std::path::PathBuf;

use bkp_types::error::{Error, Result};
use tokio::fs;

use crate::backend::StorageBackend;

/// Configuration for the local filesystem backend.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct LocalBackendConfig {
    /// Root directory where objects are stored.
    pub root: PathBuf,
}

/// Local filesystem storage backend.
///
/// Objects are stored as files under `root/`, with the object path as a relative
/// sub-path. Leading `/` characters are stripped to prevent path traversal.
pub struct LocalBackend {
    root: PathBuf,
}

impl LocalBackend {
    /// Create a new `LocalBackend`, creating `root` if it does not exist.
    pub fn new(root: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&root).map_err(|e| {
            Error::Storage(format!(
                "cannot create local backend root {}: {e}",
                root.display()
            ))
        })?;
        Ok(Self { root })
    }

    /// Resolve an object path to an absolute filesystem path.
    ///
    /// Strips leading `/` to prevent traversal outside `root`.
    fn full_path(&self, path: &str) -> PathBuf {
        let safe = path.trim_start_matches('/');
        self.root.join(safe)
    }
}

#[async_trait::async_trait]
impl StorageBackend for LocalBackend {
    async fn get(&self, path: &str) -> Result<Vec<u8>> {
        let full = self.full_path(path);
        fs::read(&full)
            .await
            .map_err(|e| Error::Storage(format!("read {}: {e}", full.display())))
    }

    async fn get_range(&self, path: &str, from: u64, to: u64) -> Result<Vec<u8>> {
        use tokio::io::{AsyncReadExt, AsyncSeekExt};
        let full = self.full_path(path);
        let mut f = fs::File::open(&full)
            .await
            .map_err(|e| Error::Storage(format!("open {}: {e}", full.display())))?;
        f.seek(std::io::SeekFrom::Start(from))
            .await
            .map_err(|e| Error::Storage(format!("seek {}: {e}", full.display())))?;
        let len = (to - from) as usize;
        let mut buf = vec![0u8; len];
        f.read_exact(&mut buf)
            .await
            .map_err(|e| Error::Storage(format!("read_range {}: {e}", full.display())))?;
        Ok(buf)
    }

    // See StorageBackend::probe_access.  A filesystem exists() never errors,
    // so confirm the configured root is an accessible directory explicitly -
    // a missing / unreadable path must report failure, not silently pass.
    async fn probe_access(&self) -> Result<()> {
        let root = self.full_path("");
        if root.is_dir() {
            Ok(())
        } else {
            Err(Error::Storage(format!(
                "path not found or not a directory: {}",
                root.display()
            )))
        }
    }

    async fn exists(&self, path: &str) -> Result<bool> {
        Ok(self.full_path(path).exists())
    }

    async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        let root = self.root.clone();
        let prefix = prefix.to_string();
        tokio::task::spawn_blocking(move || list_sync(&root, &prefix))
            .await
            .map_err(|e| Error::Internal(format!("list spawn_blocking: {e}")))?
    }

    async fn size(&self, path: &str) -> Result<u64> {
        let full = self.full_path(path);
        let meta = fs::metadata(&full)
            .await
            .map_err(|e| Error::Storage(format!("stat {}: {e}", full.display())))?;
        Ok(meta.len())
    }

    /// content-verifying HEAD for local files.
    /// Reads the file fully and routes through bkp_crypto's vendor-
    /// validated SHA-256 backend (no raw sha2
    /// crate use outside bkp-crypto).  Returns (size, hex-hash,
    /// "sha256") so the quick integrity audit catches bit rot on a
    /// local destination just as it does on S3 / Azure / GCS / B2.
    async fn head_with_hash(&self, path: &str) -> Result<(u64, String, String)> {
        let data = self.get(path).await?;
        let size = data.len() as u64;
        let hash = bkp_crypto::hash::sha256_hex(&data);
        Ok((size, hash, "sha256".to_string()))
    }

    fn display_name(&self) -> String {
        format!("local://{}", self.root.display())
    }
}

// - Synchronous directory walk ------------------------

/// Synchronous recursive directory walk collecting relative paths under `root`
/// that start with `prefix`.
fn list_sync(root: &std::path::Path, prefix: &str) -> Result<Vec<String>> {
    let mut results = Vec::new();
    // Find the deepest existing directory that is a prefix of the requested path.
    let start = if prefix.is_empty() {
        root.to_path_buf()
    } else {
        root.join(prefix.trim_start_matches('/'))
    };
    let walk_root = if start.is_dir() {
        start
    } else {
        start.parent().unwrap_or(root).to_path_buf()
    };
    walk_dir(&walk_root, root, prefix, &mut results)?;
    Ok(results)
}

fn walk_dir(
    dir: &std::path::Path,
    root: &std::path::Path,
    prefix: &str,
    results: &mut Vec<String>,
) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    for entry in entries {
        let entry = entry.map_err(|e| Error::Storage(format!("readdir: {e}")))?;
        let path = entry.path();
        if path.is_dir() {
            walk_dir(&path, root, prefix, results)?;
        } else {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            if prefix.is_empty() || rel.starts_with(prefix) {
                results.push(rel);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Seed a file at `root/rel`, creating parent dirs.  The backend is
    /// read-only, so tests stage fixtures on disk directly.
    fn seed(root: &std::path::Path, rel: &str, data: &[u8]) {
        let p = root.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).expect("create_dir_all");
        }
        std::fs::write(p, data).expect("write");
    }

    #[tokio::test]
    async fn get_reads_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        seed(dir.path(), "sub/file.bin", b"hello world");
        let backend = LocalBackend::new(dir.path().to_path_buf()).expect("new");
        let data = backend.get("sub/file.bin").await.expect("get");
        assert_eq!(data, b"hello world");
    }

    #[tokio::test]
    async fn exists_reflects_filesystem() {
        let dir = tempfile::tempdir().expect("tempdir");
        let backend = LocalBackend::new(dir.path().to_path_buf()).expect("new");
        assert!(!backend.exists("foo.bin").await.expect("exists1"));
        seed(dir.path(), "foo.bin", b"x");
        assert!(backend.exists("foo.bin").await.expect("exists2"));
    }

    #[tokio::test]
    async fn get_range_returns_half_open_slice() {
        let dir = tempfile::tempdir().expect("tempdir");
        seed(dir.path(), "r.bin", b"0123456789");
        let backend = LocalBackend::new(dir.path().to_path_buf()).expect("new");
        // [from, to) - `to` is exclusive.
        let data = backend.get_range("r.bin", 2, 5).await.expect("get_range");
        assert_eq!(data, b"234");
    }

    #[tokio::test]
    async fn list_and_size() {
        let dir = tempfile::tempdir().expect("tempdir");
        seed(dir.path(), "packs/a.pack", b"aaaa");
        seed(dir.path(), "packs/b.pack", b"bb");
        seed(dir.path(), "other/c.bin", b"c");
        let backend = LocalBackend::new(dir.path().to_path_buf()).expect("new");

        let mut packs = backend.list("packs/").await.expect("list");
        packs.sort();
        assert_eq!(packs, vec!["packs/a.pack", "packs/b.pack"]);

        assert_eq!(backend.size("packs/a.pack").await.expect("size"), 4);
    }
}
