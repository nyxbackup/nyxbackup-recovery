// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! SMB/CIFS network share storage backend.
//!
//! The strategy differs by platform, but the goal is the same everywhere:
//! recovery must reach the share WITHOUT the user preparing anything first.
//!
//! ## Windows
//! When `mount_path` is absent, a UNC path is constructed automatically:
//! `\\host\share\base_path`.  The Windows SMB client handles authentication
//! via the service account's credentials or the Windows Credential Manager -
//! no password is stored in the config file.
//!
//! ## Linux
//! An SMB2/3 session is opened in-process via libsmbclient, using the username
//! and password from the endpoint.  No mount, no root, no `/etc/fstab` entry:
//! a kernel CIFS mount needs CAP_SYS_ADMIN, and setuid `mount.cifs` only serves
//! mount points already listed in `/etc/fstab`, so neither is available to a
//! GUI running as an ordinary user - which is exactly the live-USB situation
//! this tool exists for.
//!
//! `mount_path` remains supported and takes precedence when it points at a
//! directory that exists, so existing configs and hand-made mounts keep
//! working (and are cheaper than a second SMB session).
//!
//! ## macOS
//! There is no packaged libsmbclient, so the share must be mounted first and
//! `mount_path` set to the mount point:
//!
//! ```text
//! mount_smbfs //user@host/share /Volumes/share
//! ```
//!
//! In `config.toml`:
//! ```toml
//! [[backup_set.endpoint]]
//! type = "smb"
//! host = "nas.local"
//! share = "backups"
//! mount_path = "/Volumes/share"   # required on macOS; optional on Linux
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
    /// Username for SMB authentication.  Passed to `WNetAddConnection2W` on
    /// Windows and to libsmbclient on Linux; unused on macOS, where the OS
    /// mount carries the credentials.
    #[serde(default)]
    pub username: Option<String>,
    /// Password for SMB authentication.  Used by `WNetAddConnection2W` on
    /// Windows and by the libsmbclient session on Linux.  Never persisted by
    /// the recovery tool - it lives in memory for the process lifetime only.
    #[serde(default)]
    pub password: Option<String>,
    /// Pre-mounted path to the share root.  **Required on macOS.**  Optional
    /// on Linux, where it takes precedence over the in-process SMB session
    /// when it points at an existing directory.  On Windows, if absent, a UNC
    /// path is constructed from `host`, `share`, and `base_path`.
    #[serde(default)]
    pub mount_path: Option<String>,
}

/// SMB/CIFS network share storage backend.
pub struct SmbBackend {
    /// Storage root on the local filesystem.  Used in MOUNTED mode (Windows
    /// UNC, or a Linux/macOS `mount_path` that exists); empty in direct mode.
    root: PathBuf,
    display: String,
    /// Linux only: an in-process SMB session, used when there is no usable
    /// mount.  `None` means the filesystem path in `root` is authoritative.
    #[cfg(target_os = "linux")]
    direct: Option<DirectSmb>,
    /// On Linux/macOS: the exact OS command that mounts this share at the
    /// configured mount point.  Recovery cannot mount the share itself (the
    /// kernel needs CAP_SYS_ADMIN, and setuid `mount.cifs` only serves
    /// mount points already listed in `/etc/fstab`), so the next best thing
    /// is to hand the user the command to run.
    #[cfg(not(windows))]
    mount_hint: String,
    /// On Windows: the UNC server path (`\\host\share`) we passed to
    /// WNetAddConnection2W on construction, so Drop can cancel the
    /// session.  None when no auth was requested or on non-Windows
    /// platforms.
    #[cfg(windows)]
    wnet_session: Option<String>,
}

/// An in-process SMB2/3 session via libsmbclient (Samba's client library).
///
/// This is what lets recovery read a share with no OS mount at all: the kernel
/// mount path needs CAP_SYS_ADMIN, and setuid `mount.cifs` only serves mount
/// points already listed in `/etc/fstab` - neither is available to a GUI
/// running as an ordinary user off a live USB.
#[cfg(target_os = "linux")]
struct DirectSmb {
    /// libsmbclient keeps a PROCESS-WIDE context that is not thread-safe, so
    /// every call is serialized through this mutex.  Recovery reads are
    /// LAN-bound and correctness matters more than parallelism here.
    client: std::sync::Arc<std::sync::Mutex<pavao::SmbClient>>,
    /// Storage root within the share: leading `/`, no trailing `/`, empty when
    /// the share root itself is the storage root.
    base: String,
}

#[cfg(target_os = "linux")]
impl DirectSmb {
    /// Open a session to `//host/share` using the supplied credentials.
    fn connect(cfg: &SmbConfig) -> Result<Self> {
        let creds = pavao::SmbCredentials::default()
            .server(format!("smb://{}", cfg.host))
            .share(format!("/{}", cfg.share.trim_matches('/')))
            .username(cfg.username.clone().unwrap_or_default())
            .password(cfg.password.clone().unwrap_or_default())
            .workgroup(cfg.domain.clone().unwrap_or_default());

        // one_share_per_server keeps a single connection per server rather
        // than reconnecting per path, which matters when a restore walks
        // thousands of pack objects.
        let options = pavao::SmbOptions::default()
            .one_share_per_server(true)
            .no_auto_anonymous_login(false);

        let client = pavao::SmbClient::new(creds, options).map_err(|e| {
            Error::Storage(format!(
                "SMB connect //{}/{}: {e}",
                cfg.host,
                cfg.share.trim_matches('/')
            ))
        })?;

        let sub = cfg.base_path.trim_matches(['/', '\\']);
        let base = if sub.is_empty() {
            String::new()
        } else {
            format!("/{}", sub.replace('\\', "/"))
        };

        Ok(Self {
            client: std::sync::Arc::new(std::sync::Mutex::new(client)),
            base,
        })
    }

    /// Resolve an object path to a share-relative libsmbclient path.
    fn full(&self, path: &str) -> String {
        let p = path.trim_start_matches('/').trim_end_matches('/');
        match (self.base.is_empty(), p.is_empty()) {
            (true, true) => "/".to_string(),
            (true, false) => format!("/{p}"),
            (false, true) => self.base.clone(),
            (false, false) => format!("{}/{p}", self.base),
        }
    }
}

/// Recursively collect file paths under `dir`, relative to `base`.
#[cfg(target_os = "linux")]
fn walk_smb(
    client: &pavao::SmbClient,
    base: &str,
    dir: &str,
    prefix: &str,
    out: &mut Vec<String>,
) -> Result<()> {
    // A directory that cannot be opened is reported as an error here, unlike
    // the filesystem walk: over a real SMB session an unreadable directory is
    // a genuine failure, not "nothing here".
    let entries = client
        .list_dir(dir)
        .map_err(|e| Error::Storage(format!("SMB list {dir}: {e}")))?;

    for entry in entries {
        let child = format!("{}/{}", dir.trim_end_matches('/'), entry.name());
        match entry.get_type() {
            pavao::SmbDirentType::Dir => walk_smb(client, base, &child, prefix, out)?,
            pavao::SmbDirentType::File | pavao::SmbDirentType::Link => {
                let rel = child
                    .strip_prefix(base)
                    .unwrap_or(&child)
                    .trim_start_matches('/')
                    .to_string();
                if prefix.is_empty() || rel.starts_with(prefix) {
                    out.push(rel);
                }
            }
            // Workgroup / server / printer / IPC entries are not storage.
            _ => {}
        }
    }
    Ok(())
}

/// Build the OS command that mounts this share at the configured mount point.
///
/// The backend already knows the host, share, username and mount point, so the
/// command can be produced complete except for the password, which the user is
/// prompted for interactively by `mount`.
#[cfg(not(windows))]
fn mount_command_hint(cfg: &SmbConfig) -> String {
    let mount_point = cfg.mount_path.as_deref().unwrap_or("/mnt/smb");
    let user = cfg.username.as_deref().unwrap_or("USERNAME");
    if cfg!(target_os = "macos") {
        format!(
            "mount_smbfs //{user}@{}/{} {mount_point}",
            cfg.host, cfg.share
        )
    } else {
        format!(
            "sudo mkdir -p {mount_point} && sudo mount -t cifs //{}/{} {mount_point} \
             -o username={user},vers=3.0,uid=$(id -u),gid=$(id -g)",
            cfg.host, cfg.share
        )
    }
}

impl SmbBackend {
    /// Construct a new `SmbBackend`, resolving the storage root path and
    /// (on Windows) establishing an authenticated SMB session via
    /// `WNetAddConnection2W` when a username + password are supplied.
    pub fn new(cfg: SmbConfig) -> Result<Self> {
        // `mount_path` is the mounted SHARE root; the URL's sub-path
        // (`base_path`, e.g. `set-a` in smb://host/share/set-a) is the storage
        // root WITHIN the share, so append it - matching the Windows UNC path
        // `\\host\share\base_path` built by resolve_root.  Empty base_path
        // leaves the mount point unchanged.
        let mounted_root: Option<PathBuf> = cfg.mount_path.as_ref().map(|mp| {
            let mut r = PathBuf::from(mp);
            let sub = cfg.base_path.trim_matches(['/', '\\']);
            if !sub.is_empty() {
                r = r.join(sub);
            }
            r
        });

        // On Linux an existing mount still wins - it is the fastest route and
        // keeps working for anyone who already configured one.  With no usable
        // mount we open an SMB session in-process instead of demanding the
        // user go mount it by hand.
        #[cfg(target_os = "linux")]
        let (root, direct) = match &mounted_root {
            Some(r) if r.is_dir() => (r.clone(), None),
            _ => (PathBuf::new(), Some(DirectSmb::connect(&cfg)?)),
        };

        #[cfg(not(target_os = "linux"))]
        let root: PathBuf = match mounted_root {
            Some(r) => r,
            None => resolve_root(&cfg)?,
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

        #[cfg(not(windows))]
        let mount_hint = mount_command_hint(&cfg);

        Ok(Self {
            root,
            display,
            #[cfg(target_os = "linux")]
            direct,
            #[cfg(not(windows))]
            mount_hint,
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

/// macOS only: there is no packaged libsmbclient to talk SMB in-process, so the
/// share still has to be mounted at the OS level first.  (Linux never reaches
/// this - it opens an SMB session directly when no mount is configured, and
/// Windows builds the UNC path above.)
#[cfg(all(not(windows), not(target_os = "linux")))]
fn resolve_root(cfg: &SmbConfig) -> Result<PathBuf> {
    Err(Error::Config(format!(
        "SMB backend: on macOS, mount the share and set 'mount_path' to \
         the mount point (e.g. mount_path = \"/Volumes/share\").  \
         Share: //{}/{}",
        cfg.host, cfg.share
    )))
}

#[async_trait::async_trait]
impl StorageBackend for SmbBackend {
    #[instrument(skip(self), fields(root = ?self.root, path))]
    async fn get(&self, path: &str) -> Result<Vec<u8>> {
        #[cfg(target_os = "linux")]
        if let Some(d) = &self.direct {
            let client = d.client.clone();
            let full = d.full(path);
            return tokio::task::spawn_blocking(move || {
                use std::io::Read;
                let guard = client
                    .lock()
                    .map_err(|_| Error::Internal("SMB client mutex poisoned".to_string()))?;
                let mut f = guard
                    .open_with(&full, pavao::SmbOpenOptions::default().read(true))
                    .map_err(|e| Error::Storage(format!("SMB open {full}: {e}")))?;
                let mut buf = Vec::new();
                f.read_to_end(&mut buf)
                    .map_err(|e| Error::Storage(format!("SMB read {full}: {e}")))?;
                Ok(buf)
            })
            .await
            .map_err(|e| Error::Internal(format!("SMB get spawn_blocking: {e}")))?;
        }

        let full = self.full_path(path);
        fs::read(&full)
            .await
            .map_err(|e| Error::Storage(format!("SMB read {}: {e}", full.display())))
    }

    #[instrument(skip(self), fields(root = ?self.root, path, from, to))]
    async fn get_range(&self, path: &str, from: u64, to: u64) -> Result<Vec<u8>> {
        #[cfg(target_os = "linux")]
        if let Some(d) = &self.direct {
            let client = d.client.clone();
            let full = d.full(path);
            let len = (to - from) as usize;
            return tokio::task::spawn_blocking(move || {
                use std::io::{Read, Seek};
                let guard = client
                    .lock()
                    .map_err(|_| Error::Internal("SMB client mutex poisoned".to_string()))?;
                let mut f = guard
                    .open_with(&full, pavao::SmbOpenOptions::default().read(true))
                    .map_err(|e| Error::Storage(format!("SMB open {full}: {e}")))?;
                f.seek(std::io::SeekFrom::Start(from))
                    .map_err(|e| Error::Storage(format!("SMB seek {full}: {e}")))?;
                let mut buf = vec![0u8; len];
                f.read_exact(&mut buf)
                    .map_err(|e| Error::Storage(format!("SMB read_range {full}: {e}")))?;
                Ok(buf)
            })
            .await
            .map_err(|e| Error::Internal(format!("SMB get_range spawn_blocking: {e}")))?;
        }

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
        // Direct mode: the session is already open, so confirm the storage
        // root exists on the share.  No mount is involved, so a failure here
        // really is a wrong path or a permissions problem.
        #[cfg(target_os = "linux")]
        if let Some(d) = &self.direct {
            let client = d.client.clone();
            let full = d.full("");
            return tokio::task::spawn_blocking(move || {
                let guard = client
                    .lock()
                    .map_err(|_| Error::Internal("SMB client mutex poisoned".to_string()))?;
                let stat = guard
                    .stat(&full)
                    .map_err(|e| Error::Storage(format!("SMB stat {full}: {e}")))?;
                if stat.mode.is_dir() {
                    Ok(())
                } else {
                    Err(Error::Storage(format!(
                        "path not found or not a directory: {full}"
                    )))
                }
            })
            .await
            .map_err(|e| Error::Internal(format!("SMB probe spawn_blocking: {e}")))?;
        }

        let root = self.full_path("");
        if root.is_dir() {
            return Ok(());
        }
        // On Linux/macOS this backend reads through an OS-level mount, so the
        // overwhelmingly common cause is that the share was never mounted -
        // not a mistyped path.  Name that cause and carry the exact mount
        // command; `mount-command:` is the marker bkp-recover's error
        // classifier splits on to surface the command in the GUI.
        #[cfg(not(windows))]
        return Err(Error::Storage(format!(
            "SMB share is not mounted at {}; mount-command: {}",
            root.display(),
            self.mount_hint
        )));
        #[cfg(windows)]
        return Err(Error::Storage(format!(
            "path not found or not a directory: {}",
            root.display()
        )));
    }

    async fn exists(&self, path: &str) -> Result<bool> {
        #[cfg(target_os = "linux")]
        if let Some(d) = &self.direct {
            let client = d.client.clone();
            let full = d.full(path);
            return tokio::task::spawn_blocking(move || {
                let guard = client
                    .lock()
                    .map_err(|_| Error::Internal("SMB client mutex poisoned".to_string()))?;
                // A stat failure here is indistinguishable from "absent" over
                // libsmbclient, which is the same contract the filesystem
                // path has (`Path::exists()` swallows errors too).
                Ok(guard.stat(&full).is_ok())
            })
            .await
            .map_err(|e| Error::Internal(format!("SMB exists spawn_blocking: {e}")))?;
        }

        Ok(self.full_path(path).exists())
    }

    #[instrument(skip(self), fields(root = ?self.root, prefix))]
    async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        #[cfg(target_os = "linux")]
        if let Some(d) = &self.direct {
            let client = d.client.clone();
            let base = d.full("");
            let start = d.full(prefix);
            let prefix = prefix.to_string();
            return tokio::task::spawn_blocking(move || {
                let guard = client
                    .lock()
                    .map_err(|_| Error::Internal("SMB client mutex poisoned".to_string()))?;
                // Listing a prefix that is not itself a directory (or does not
                // exist) yields no objects rather than an error, matching the
                // object-store semantics the other backends present.
                let is_dir = guard.stat(&start).map(|s| s.mode.is_dir()).unwrap_or(false);
                if !is_dir {
                    return Ok(Vec::new());
                }
                let mut out = Vec::new();
                let base = base.trim_end_matches('/');
                walk_smb(&guard, base, &start, &prefix, &mut out)?;
                Ok(out)
            })
            .await
            .map_err(|e| Error::Internal(format!("SMB list spawn_blocking: {e}")))?;
        }

        let root = self.root.clone();
        let prefix = prefix.to_string();
        tokio::task::spawn_blocking(move || list_sync(&root, &prefix))
            .await
            .map_err(|e| Error::Internal(format!("SMB list spawn_blocking: {e}")))?
    }

    #[instrument(skip(self), fields(root = ?self.root, path))]
    async fn size(&self, path: &str) -> Result<u64> {
        #[cfg(target_os = "linux")]
        if let Some(d) = &self.direct {
            let client = d.client.clone();
            let full = d.full(path);
            return tokio::task::spawn_blocking(move || {
                let guard = client
                    .lock()
                    .map_err(|_| Error::Internal("SMB client mutex poisoned".to_string()))?;
                guard
                    .stat(&full)
                    .map(|s| s.size)
                    .map_err(|e| Error::Storage(format!("SMB stat {full}: {e}")))
            })
            .await
            .map_err(|e| Error::Internal(format!("SMB size spawn_blocking: {e}")))?;
        }

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

#[cfg(test)]
mod tests {
    use super::*;

    /// An endpoint for `smb://recovery@nas.example/backups/set-a`.
    fn cfg(mount_path: Option<String>) -> SmbConfig {
        SmbConfig {
            host: "nas.example".to_string(),
            share: "backups".to_string(),
            base_path: "set-a".to_string(),
            domain: None,
            username: Some("recovery".to_string()),
            password: Some("hunter2".to_string()),
            mount_path,
        }
    }

    /// An existing mount still wins: it is the cheaper route, and configs that
    /// already point at one must keep behaving exactly as before.
    #[test]
    fn an_existing_mount_is_preferred_over_a_direct_session() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("set-a")).expect("create base_path");
        let backend =
            SmbBackend::new(cfg(Some(dir.path().display().to_string()))).expect("construct");

        // The storage root is mount_path joined with the URL's sub-path.
        assert!(backend.root.ends_with("set-a"));
        #[cfg(target_os = "linux")]
        assert!(
            backend.direct.is_none(),
            "a usable mount must not open a redundant SMB session"
        );
    }

    /// The point of the libsmbclient backend: with no mount configured at all,
    /// construction still succeeds and yields a direct session.  Recovery must
    /// never require the user to mount the share by hand.
    #[cfg(target_os = "linux")]
    #[test]
    fn no_mount_still_yields_a_usable_backend() {
        let backend = SmbBackend::new(cfg(None)).expect("must not require a mount");
        assert!(backend.direct.is_some());
    }

    /// A configured-but-absent mount point is not a dead end either - it falls
    /// through to the direct session rather than erroring.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_missing_mount_point_falls_back_to_a_direct_session() {
        let backend = SmbBackend::new(cfg(Some("/nonexistent-mount-point".to_string())))
            .expect("a missing mount point must not be fatal");
        assert!(backend.direct.is_some());
    }

    /// Object paths are resolved share-relative, under the URL's sub-path.
    #[cfg(target_os = "linux")]
    #[test]
    fn direct_paths_are_share_relative() {
        let backend = SmbBackend::new(cfg(None)).expect("construct");
        let direct = backend.direct.as_ref().expect("direct session");
        assert_eq!(direct.full(""), "/set-a");
        assert_eq!(direct.full("manifests/"), "/set-a/manifests");
        assert_eq!(direct.full("/packs/x.pack"), "/set-a/packs/x.pack");
    }

    /// With an empty base_path the share root IS the storage root.
    #[cfg(target_os = "linux")]
    #[test]
    fn direct_paths_handle_an_empty_base_path() {
        let mut c = cfg(None);
        c.base_path = String::new();
        let backend = SmbBackend::new(c).expect("construct");
        let direct = backend.direct.as_ref().expect("direct session");
        assert_eq!(direct.full(""), "/");
        assert_eq!(direct.full("manifests/"), "/manifests");
    }

    /// macOS has no packaged libsmbclient, so it still needs an OS mount - and
    /// must say so with a runnable command rather than a bare "not found".
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn macos_reports_an_unmounted_share_with_a_mount_command() {
        let backend =
            SmbBackend::new(cfg(Some("/nonexistent-mount-point".to_string()))).expect("construct");
        let err = backend
            .probe_access()
            .await
            .expect_err("an unmounted share must fail the access probe")
            .to_string();
        assert!(err.contains("is not mounted"), "got: {err}");
        let (_, command) = err
            .split_once("mount-command: ")
            .expect("the error must carry a mount-command marker");
        assert!(command.contains("nas.example"), "host missing: {command}");
    }
}
