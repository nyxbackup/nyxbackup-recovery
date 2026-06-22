// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! Size-bounded log appender shared by every binary in the workspace.
//!
//! One type, [`SizeRollingAppender`]: a `Write`-implementing file appender
//! that rotates when the current file exceeds a byte threshold.  On rotation
//! the live file is gzip-compressed into `<name>.1.gz`, prior archives are
//! shifted (`.1.gz` -> `.2.gz`, ...), and archives beyond `keep` are pruned.
//!
//! Plugged into `tracing` via `tracing_appender::non_blocking`; the daemon
//! also wires this through `reload::Layer<LevelFilter>` so the GUI's
//! "Set log level" RPC can hot-reload severity without restarting.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use flate2::Compression;
use flate2::write::GzEncoder;

/// A `Write`-implementing log appender that rotates when the current file
/// exceeds `max_bytes`.  On rotation the current file is gzip-compressed to
/// `<name>.1.gz`; any existing archive is cycled (`.1.gz` → `.2.gz`, …);
/// archives beyond `keep` are deleted.
///
/// Use with `tracing_appender::non_blocking`:
/// ```ignore
/// let appender = SizeRollingAppender::new(&log_dir, "service.log", 6 * 1024 * 1024, 2)?;
/// let (nb, _guard) = tracing_appender::non_blocking(appender);
/// ```
pub struct SizeRollingAppender {
    file: File,
    current_size: u64,
    log_path: PathBuf,
    max_bytes: u64,
    keep: usize,
}

impl SizeRollingAppender {
    /// Open or create the live log at `<dir>/<name>` for appending.  Creates
    /// `<dir>` if missing.  `max_bytes` is the rotation threshold; `keep` is
    /// the maximum number of `.N.gz` archives retained (older are deleted).
    pub fn new(dir: &Path, name: &str, max_bytes: u64, keep: usize) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let log_path = dir.join(name);
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)?;
        let current_size = file.metadata()?.len();
        Ok(Self {
            file,
            current_size,
            log_path,
            max_bytes,
            keep,
        })
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.file.flush()?;

        // Cycle existing archives oldest-first: for each slot, renaming
        // `.i.gz -> .(i+1).gz` first removes the existing `.(i+1).gz`, so the
        // highest slot (`.keep.gz`) is pruned as `.keep-1.gz` shifts into it.
        // This prunes the oldest archive AND makes room in one pass - do NOT
        // add a separate "delete .keep" step afterwards: that deletes the
        // archive just cycled in and silently reduces retention to keep-1
        // (keep=2 kept only 1 archive).
        for i in (1..self.keep).rev() {
            let from = archive_path(&self.log_path, i);
            let to = archive_path(&self.log_path, i + 1);
            if from.exists() {
                if to.exists() {
                    fs::remove_file(&to)?;
                }
                fs::rename(&from, &to)?;
            }
        }

        // Compress current log → .1.gz  (overwrites any prior .1.gz, which
        // covers the keep == 1 case where the loop above does nothing).
        {
            let mut src = File::open(&self.log_path)?;
            let archive = archive_path(&self.log_path, 1);
            let dst = File::create(&archive)?;
            let mut enc = GzEncoder::new(dst, Compression::default());
            io::copy(&mut src, &mut enc)?;
            enc.finish()?;
        }

        // Truncate log and reopen for appending
        self.file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&self.log_path)?;
        self.current_size = 0;

        Ok(())
    }
}

impl Write for SizeRollingAppender {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.current_size >= self.max_bytes
            && let Err(e) = self.rotate()
        {
            eprintln!("[bkp-log] log rotation failed: {e}");
        }
        let n = self.file.write(buf)?;
        self.current_size += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

fn archive_path(log_path: &Path, n: usize) -> PathBuf {
    let mut s = log_path.as_os_str().to_owned();
    s.push(format!(".{n}.gz"));
    PathBuf::from(s)
}
