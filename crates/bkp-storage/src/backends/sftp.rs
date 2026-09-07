// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! SFTP storage backend.
//!
//! Uses the `ssh2` crate (synchronous).  Every operation spawns a blocking
//! task so it does not block the async executor.  A new SSH session is
//! created per operation - connection pooling can be added later.

use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;

use bkp_types::error::{Error, Result};

use crate::backend::StorageBackend;

/// Configuration for the SFTP backend.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct SftpConfig {
    /// Remote hostname or IP.
    pub host: String,
    /// TCP port (default 22).
    #[serde(default = "default_port")]
    pub port: u16,
    /// SSH username.
    pub username: String,
    /// Password authentication.
    #[serde(default)]
    pub password: Option<String>,
    /// Path to a PEM private key file on the local machine.
    #[serde(default)]
    pub private_key_path: Option<PathBuf>,
    /// Passphrase for an encrypted private key.
    #[serde(default)]
    pub private_key_passphrase: Option<String>,
    /// Base directory on the remote host.
    pub base_path: PathBuf,
}

fn default_port() -> u16 {
    22
}

#[derive(Clone)]
struct SftpState(Arc<SftpConfig>);

impl SftpState {
    fn connect(&self) -> Result<ssh2::Session> {
        let cfg = &self.0;
        let addr = format!("{}:{}", cfg.host, cfg.port);
        let stream = std::net::TcpStream::connect(&addr)
            .map_err(|e| Error::Storage(format!("SFTP TCP connect {addr}: {e}")))?;
        // Mark the socket as "scavenger" priority + low-priority TCP
        // congestion so a large backup yields to foreground traffic
        // (video calls, streaming).  Best-effort.
        let _ = crate::nice_net::apply_to_tcp_stream(&stream);
        let mut session = ssh2::Session::new()
            .map_err(|e| Error::Storage(format!("SFTP session create: {e}")))?;
        session.set_tcp_stream(stream);
        session
            .handshake()
            .map_err(|e| Error::Storage(format!("SFTP handshake: {e}")))?;

        if let Some(password) = &cfg.password {
            session
                .userauth_password(&cfg.username, password)
                .map_err(|e| Error::Storage(format!("SFTP password auth: {e}")))?;
        } else if let Some(key_path) = &cfg.private_key_path {
            let passphrase = cfg.private_key_passphrase.as_deref();
            session
                .userauth_pubkey_file(&cfg.username, None, key_path, passphrase)
                .map_err(|e| Error::Storage(format!("SFTP pubkey auth: {e}")))?;
        } else {
            return Err(Error::Storage(
                "SFTP: no authentication method configured (set password or private_key_path)"
                    .into(),
            ));
        }

        if !session.authenticated() {
            return Err(Error::Storage("SFTP: authentication failed".into()));
        }
        Ok(session)
    }

    fn remote_path(&self, path: &str) -> PathBuf {
        // SFTP paths are always POSIX (forward slash) regardless of
        // client OS.  `PathBuf::join` uses the OS separator, so on
        // Windows it produced `/home/foo\packs/bar.pack` which the
        // server then failed to find.  Build the joined path as a
        // string with `/` and round-trip through PathBuf so callers
        // that take `&Path` still work.
        let base = self.0.base_path.to_string_lossy();
        let base = base.trim_end_matches(['/', '\\']);
        let rel = path.trim_start_matches('/').replace('\\', "/");
        PathBuf::from(format!("{}/{}", base, rel))
    }
}

/// SFTP storage backend.
pub struct SftpBackend {
    state: SftpState,
}

impl SftpBackend {
    /// Create a new `SftpBackend`.
    pub fn new(cfg: SftpConfig) -> Self {
        Self {
            state: SftpState(Arc::new(cfg)),
        }
    }
}

#[async_trait::async_trait]
impl StorageBackend for SftpBackend {
    async fn get(&self, path: &str) -> Result<Vec<u8>> {
        let state = self.state.clone();
        let remote = state.remote_path(path);
        tokio::task::spawn_blocking(move || {
            // Retry up to 3 times on transient connection failures.  A new TCP
            // session is opened per call, so temporary server-side limits or
            // brief network hiccups can cause intermittent errors.
            let mut last_err = None;
            for attempt in 0..3u32 {
                if attempt > 0 {
                    std::thread::sleep(std::time::Duration::from_millis(200 * attempt as u64));
                }
                match (|| -> Result<Vec<u8>> {
                    let session = state.connect()?;
                    let sftp = session
                        .sftp()
                        .map_err(|e| Error::Storage(format!("SFTP channel: {e}")))?;
                    let mut f = sftp.open(&remote).map_err(|e| {
                        Error::Storage(format!("SFTP open {}: {e}", remote.display()))
                    })?;
                    let mut buf = Vec::new();
                    f.read_to_end(&mut buf).map_err(|e| {
                        Error::Storage(format!("SFTP read {}: {e}", remote.display()))
                    })?;
                    Ok(buf)
                })() {
                    Ok(data) => return Ok(data),
                    Err(e) => {
                        // Short-circuit on definitively-non-transient errors:
                        // retrying "no such file" or auth failures 3 times only
                        // wastes wall-clock during a bulk restore where many
                        // files reference the same missing pack.  Burning the
                        // SSH session setup time per attempt was making a
                        // single missing pack throttle a 100K-file restore.
                        let msg = e.to_string().to_lowercase();
                        let permanent = msg.contains("no such file")
                            || msg.contains("no such directory")
                            || msg.contains("permission denied")
                            || msg.contains("access denied");
                        last_err = Some(e);
                        if permanent {
                            break;
                        }
                    }
                }
            }
            Err(last_err.unwrap_or_else(|| Error::Storage("SFTP get: no attempts".into())))
        })
        .await
        .map_err(|e| Error::Internal(format!("SFTP get spawn: {e}")))?
    }

    // libssh2's SFTP seek interacts badly with its read-ahead pipeline on
    // some server implementations, producing wrong bytes or UnexpectedEof
    // for chunks that are not at the start of a pack file.  So we
    // deliberately fall back to a full download + slice here.  SFTP is
    // typically used over a local network where downloading the full pack
    // is fast enough.  Made explicit (rather than relying on a trait
    // default) so the next reader of this code can see the trade-off
    // instead of inheriting silent behavior.
    async fn get_range(&self, path: &str, from: u64, to: u64) -> Result<Vec<u8>> {
        let data = self.get(path).await?;
        let from = from as usize;
        let to = (to as usize).min(data.len());
        if from >= data.len() {
            return Ok(Vec::new());
        }
        Ok(data[from..to].to_vec())
    }

    // See StorageBackend::probe_access: a single cheap authed round trip via
    // exists("") (an SFTP stat of the configured root, which also drives the
    // SSH connect+auth).  Both Ok(true)/Ok(false) mean reachable; only a real
    // connect/auth error propagates.  No directory enumeration.
    async fn probe_access(&self) -> Result<()> {
        self.exists("").await.map(|_| ())
    }

    async fn exists(&self, path: &str) -> Result<bool> {
        let state = self.state.clone();
        let remote = state.remote_path(path);
        tokio::task::spawn_blocking(move || {
            let session = state.connect()?;
            let sftp = session
                .sftp()
                .map_err(|e| Error::Storage(format!("SFTP channel: {e}")))?;
            match sftp.stat(&remote) {
                Ok(_) => Ok(true),
                Err(e) => {
                    let msg = e.message().to_lowercase();
                    if msg.contains("no such file")
                        || msg.contains("not found")
                        || msg.contains("does not exist")
                    {
                        Ok(false)
                    } else {
                        Err(Error::Storage(format!(
                            "SFTP stat {}: {e}",
                            remote.display()
                        )))
                    }
                }
            }
        })
        .await
        .map_err(|e| Error::Internal(format!("SFTP exists spawn: {e}")))?
    }

    async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        let state = self.state.clone();
        let base = state.0.base_path.clone();
        let dir = state.remote_path(prefix);
        let prefix = prefix.to_string();
        tokio::task::spawn_blocking(move || {
            let session = state.connect()?;
            let sftp = session
                .sftp()
                .map_err(|e| Error::Storage(format!("SFTP channel: {e}")))?;
            let mut results = Vec::new();
            sftp_list_recursive(&sftp, &dir, &base, &prefix, &mut results)?;
            Ok(results)
        })
        .await
        .map_err(|e| Error::Internal(format!("SFTP list spawn: {e}")))?
    }

    async fn size(&self, path: &str) -> Result<u64> {
        let state = self.state.clone();
        let remote = state.remote_path(path);
        tokio::task::spawn_blocking(move || {
            let session = state.connect()?;
            let sftp = session
                .sftp()
                .map_err(|e| Error::Storage(format!("SFTP channel: {e}")))?;
            let stat = sftp
                .stat(&remote)
                .map_err(|e| Error::Storage(format!("SFTP stat {}: {e}", remote.display())))?;
            Ok(stat.size.unwrap_or(0))
        })
        .await
        .map_err(|e| Error::Internal(format!("SFTP size spawn: {e}")))?
    }

    /// Limit concurrent operations to 2.  Each operation opens a new SSH
    /// session; most SFTP servers (OpenSSH default: MaxSessions 10) reject
    /// further connections when the limit is hit, producing handshake failures.
    fn concurrency_hint(&self) -> Option<usize> {
        Some(2)
    }

    fn display_name(&self) -> String {
        let cfg = &self.state.0;
        format!(
            "sftp://{}@{}:{}/{}",
            cfg.username,
            cfg.host,
            cfg.port,
            cfg.base_path.display()
        )
    }
}

// - Helpers ----------------------------------

fn sftp_list_recursive(
    sftp: &ssh2::Sftp,
    dir: &std::path::Path,
    base: &std::path::Path,
    prefix: &str,
    results: &mut Vec<String>,
) -> Result<()> {
    let entries = match sftp.readdir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    for (path, stat) in entries {
        if stat.is_dir() {
            sftp_list_recursive(sftp, &path, base, prefix, results)?;
        } else {
            let rel = path
                .strip_prefix(base)
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
