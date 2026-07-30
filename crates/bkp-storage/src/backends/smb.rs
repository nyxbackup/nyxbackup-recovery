// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! SMB/CIFS network share storage backend.
//!
//! All file I/O is delegated to `tokio::fs` once the share root path is
//! resolved.  The resolution strategy differs by platform:
//!
//! ## Windows
//! When `mount_path` is absent, a UNC path is constructed automatically:
//! `\\host\share\base_path`.  The Windows SMB client handles authentication
//! via the service account's credentials or the Windows Credential Manager -
//! no password is stored in the config file.
//!
//! ## Linux / macOS
//! Mount the CIFS/SMB share at the OS level first, then set `mount_path` to
//! the mount point:
//!
//! ```text
//! # Linux
//! mount -t cifs //host/share /mnt/smb -o credentials=/etc/samba/creds
//!
//! # macOS - connect via Finder or:
//! mount_smbfs //user@host/share /Volumes/share
//! ```
//!
//! Then in `config.toml`:
//! ```toml
//! [[backup_set.endpoint]]
//! type = "smb"
//! host = "nas.local"
//! share = "backups"
//! mount_path = "/mnt/smb"   # required on Linux/macOS
//! ```

use std::path::PathBuf;

use bkp_types::error::{Error, Result};
use tokio::fs;
use tracing::instrument;

use crate::backend::StorageBackend;

/// Configuration for the SMB/CIFS backend.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct SmbConfig {
    /// SMB server hostname or IP address.
    pub host: String,
    /// Share name.
    pub share: String,
    /// Sub-path within the share used as the storage root.
    #[serde(default)]
    pub base_path: String,
    /// Windows domain for NTLM authentication (optional, Windows only).
    #[serde(default)]
    pub domain: Option<String>,
    /// Username for SMB authentication.  On Windows the username is passed
    /// to `WNetAddConnection2W` together with `password`; on Linux/macOS
    /// the user is expected to have mounted the share already (the daemon
    /// just opens files via the local mount path).
    #[serde(default)]
    pub username: Option<String>,
    /// Password for SMB authentication (Windows only).  Used by
    /// `WNetAddConnection2W` to bind a temporary connection on backend
    /// construction.  Stored in the OS keychain at rest; loaded into
    /// memory only for the duration of the daemon process.
    #[serde(default)]
    pub password: Option<String>,
    /// Pre-mounted path to the share root.  **Required on Linux/macOS.**
    /// On Windows, if absent, a UNC path is constructed from `host`, `share`,
    /// and `base_path`.
    #[serde(default)]
    pub mount_path: Option<String>,
}

/// SMB/CIFS network share storage backend.
pub struct SmbBackend {
    root: PathBuf,
    display: String,
    /// On Windows: the UNC server path (`\\host\share`) we passed to
    /// WNetAddConnection2W on construction, so Drop can cancel the
    /// session.  None when no auth was requested or on non-Windows
    /// platforms.
    #[cfg(windows)]
    wnet_session: Option<String>,
}

impl SmbBackend {
    /// Construct a new `SmbBackend`, resolving the storage root path and
    /// (on Windows) establishing an authenticated SMB session via
    /// `WNetAddConnection2W` when a username + password are supplied.
    pub fn new(cfg: SmbConfig) -> Result<Self> {
        let root: PathBuf = if let Some(mp) = &cfg.mount_path {
            // `mount_path` is the mounted SHARE root; the URL's sub-path
            // (`base_path`, e.g. `ant_smb` in smb://host/share/ant_smb) is the
            // storage root WITHIN the share, so append it - matching the
            // Windows UNC path `\\host\share\base_path` built by resolve_root.
            // Empty base_path leaves the mount point unchanged.
            let mut r = PathBuf::from(mp);
            let sub = cfg.base_path.trim_matches(['/', '\\']);
            if !sub.is_empty() {
                r = r.join(sub);
            }
            r
        } else {
            resolve_root(&cfg)?
        };

        let display = format!(
            "smb://{}/{}/{}",
            cfg.host,
            cfg.share,
            cfg.base_path.trim_start_matches('/')
        );

        // bind credentials to the share via WNetAddConnection2W
        // so the daemon (LocalSystem) doesn't try to authenticate as the
        // machine account when the share's `valid users` excludes it.
        // CONNECT_TEMPORARY means the session lives only for this daemon
        // process - no LSA cache pollution.
        #[cfg(windows)]
        let wnet_session = win_smb::add_connection(&cfg)?;

        Ok(Self {
            root,
            display,
            #[cfg(windows)]
            wnet_session,
        })
    }

    fn full_path(&self, path: &str) -> PathBuf {
        self.root.join(path.trim_start_matches('/'))
    }
}

#[cfg(windows)]
impl Drop for SmbBackend {
    fn drop(&mut self) {
        if let Some(server) = self.wnet_session.take() {
            win_smb::cancel_connection(&server);
        }
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod win_smb {
    use super::SmbConfig;
    use bkp_types::error::{Error, Result};
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::ERROR_ALREADY_ASSIGNED;
    use windows_sys::Win32::NetworkManagement::WNet::{
        CONNECT_TEMPORARY, NETRESOURCEW, RESOURCETYPE_DISK, WNetAddConnection2W,
        WNetCancelConnection2W,
    };

    /// UTF-16-encode a `&str` and null-terminate.
    fn wide(s: &str) -> Vec<u16> {
        OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    /// Establish a temporary SMB session for `\\host\share` using
    /// username + password from `cfg`.  Returns the server UNC path so
    /// Drop can cancel later; returns `None` when no creds are provided
    /// (caller is relying on the daemon's process token, which works
    /// when the share permits the machine account).
    pub fn add_connection(cfg: &SmbConfig) -> Result<Option<String>> {
        let Some(password) = cfg.password.as_deref().filter(|p| !p.is_empty()) else {
            return Ok(None);
        };
        let Some(username) = cfg.username.as_deref().filter(|u| !u.is_empty()) else {
            return Ok(None);
        };
        // The remote name we connect to is `\\host\share` - we want the
        // session scoped to the share, not a deeper sub-path (any path
        // under the share inherits the session's creds automatically).
        let server = format!(r"\\{}\{}", cfg.host, cfg.share);
        let user_full = match cfg.domain.as_deref() {
            Some(d) if !d.is_empty() => format!("{d}\\{username}"),
            _ => username.to_string(),
        };

        let mut remote = wide(&server);
        let user = wide(&user_full);
        let pass = wide(password);

        let mut nr = NETRESOURCEW {
            dwScope: 0,
            dwType: RESOURCETYPE_DISK,
            dwDisplayType: 0,
            dwUsage: 0,
            lpLocalName: std::ptr::null_mut(),
            lpRemoteName: remote.as_mut_ptr(),
            lpComment: std::ptr::null_mut(),
            lpProvider: std::ptr::null_mut(),
        };
        // SAFETY: `nr` references buffers we own for the duration of
        // the call; `user`/`pass` are null-terminated UTF-16; CONNECT_TEMPORARY
        // does NOT persist creds to LSA.
        let rc = unsafe {
            WNetAddConnection2W(&mut nr, pass.as_ptr(), user.as_ptr(), CONNECT_TEMPORARY)
        };
        // ERROR_ALREADY_ASSIGNED (85): another connection to this server
        // already exists in the same logon session - treat as success
        // (the existing session's credentials may be the same; either way
        // we don't own that session and shouldn't cancel it on Drop).
        if rc == 0 {
            Ok(Some(server))
        } else if rc == ERROR_ALREADY_ASSIGNED {
            tracing::debug!(server, "SMB: connection already exists; skipping bind.");
            Ok(None)
        } else {
            Err(Error::Storage(format!(
                "SMB WNetAddConnection2W({server}) failed: Windows error {rc}"
            )))
        }
    }

    /// Drop a temporary session previously added via `add_connection`.
    /// Best-effort: failures are logged but not propagated (we're in
    /// Drop and can't usefully report errors anyway).
    pub fn cancel_connection(server: &str) {
        let remote = wide(server);
        // SAFETY: `remote` is null-terminated UTF-16; FALSE for fForce
        // means "fail if files are still open" - we accept that and log.
        let rc = unsafe { WNetCancelConnection2W(remote.as_ptr(), 0, 0) };
        if rc != 0 {
            tracing::debug!(
                server,
                code = rc,
                "SMB WNetCancelConnection2W returned non-zero."
            );
        }
    }
}

/// Build the OS-native path to the share root without a `mount_path` override.
#[cfg(windows)]
fn resolve_root(cfg: &SmbConfig) -> Result<PathBuf> {
    let base = cfg.base_path.trim_matches(['/', '\\']);
    let unc = if base.is_empty() {
        format!(r"\\{}\{}", cfg.host, cfg.share)
    } else {
        format!(r"\\{}\{}\{}", cfg.host, cfg.share, base.replace('/', r"\"))
    };
    Ok(PathBuf::from(unc))
}

#[cfg(not(windows))]
fn resolve_root(cfg: &SmbConfig) -> Result<PathBuf> {
    Err(Error::Config(format!(
        "SMB backend: on Linux/macOS, mount the share and set 'mount_path' to \
         the mount point (e.g. mount_path = \"/mnt/smb\").  \
         Share: //{}/{}",
        cfg.host, cfg.share
    )))
}

#[async_trait::async_trait]
impl StorageBackend for SmbBackend {
    #[instrument(skip(self), fields(root = ?self.root, path))]
    async fn get(&self, path: &str) -> Result<Vec<u8>> {
        let full = self.full_path(path);
        fs::read(&full)
            .await
            .map_err(|e| Error::Storage(format!("SMB read {}: {e}", full.display())))
    }

    #[instrument(skip(self), fields(root = ?self.root, path, from, to))]
    async fn get_range(&self, path: &str, from: u64, to: u64) -> Result<Vec<u8>> {
        use tokio::io::{AsyncReadExt, AsyncSeekExt};
        let full = self.full_path(path);
        let mut f = fs::File::open(&full)
            .await
            .map_err(|e| Error::Storage(format!("SMB open {}: {e}", full.display())))?;
        f.seek(std::io::SeekFrom::Start(from))
            .await
            .map_err(|e| Error::Storage(format!("SMB seek {}: {e}", full.display())))?;
        let len = (to - from) as usize;
        let mut buf = vec![0u8; len];
        f.read_exact(&mut buf)
            .await
            .map_err(|e| Error::Storage(format!("SMB read_range {}: {e}", full.display())))?;
        Ok(buf)
    }

    #[instrument(skip(self), fields(root = ?self.root, path))]
    // See StorageBackend::probe_access.  The SMB path resolves through the OS
    // filesystem (UNC / mount), whose exists() never errors, so confirm the
    // configured root is an accessible directory explicitly - an unmounted or
    // unreachable share must report failure, not silently pass.
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

    #[instrument(skip(self), fields(root = ?self.root, prefix))]
    async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        let root = self.root.clone();
        let prefix = prefix.to_string();
        tokio::task::spawn_blocking(move || list_sync(&root, &prefix))
            .await
            .map_err(|e| Error::Internal(format!("SMB list spawn_blocking: {e}")))?
    }

    #[instrument(skip(self), fields(root = ?self.root, path))]
    async fn size(&self, path: &str) -> Result<u64> {
        let full = self.full_path(path);
        let meta = fs::metadata(&full)
            .await
            .map_err(|e| Error::Storage(format!("SMB stat {}: {e}", full.display())))?;
        Ok(meta.len())
    }

    /// content-verifying HEAD for SMB-mounted
    /// destinations.  Behaviour matches the Local backend: pull the
    /// file through the OS-mount and SHA-256 it via bkp_crypto's
    /// vendor-validated hash backend.  SMB is typically over a LAN
    /// so this is fast in practice; users on slow SMB-over-WAN can
    /// disable scheduled audits via integrity_check_interval_days = 0.
    #[instrument(skip(self), fields(root = ?self.root, path))]
    async fn head_with_hash(&self, path: &str) -> Result<(u64, String, String)> {
        let data = self.get(path).await?;
        let size = data.len() as u64;
        let hash = bkp_crypto::hash::sha256_hex(&data);
        Ok((size, hash, "sha256".to_string()))
    }

    fn display_name(&self) -> String {
        self.display.clone()
    }
}

// - Directory walk ------------------------------

fn list_sync(root: &std::path::Path, prefix: &str) -> Result<Vec<String>> {
    let mut results = Vec::new();
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
        let entry = entry.map_err(|e| Error::Storage(format!("SMB readdir: {e}")))?;
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
