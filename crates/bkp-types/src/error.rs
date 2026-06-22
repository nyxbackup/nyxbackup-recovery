// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! Shared error type for the backup application.

use thiserror::Error;

/// Top-level application error.
#[derive(Debug, Error)]
pub enum Error {
    /// I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Serialization error.
    #[error("Serialization error: {0}")]
    Serialization(String),

    /// Encryption or decryption failure.
    #[error("Crypto error: {0}")]
    Crypto(String),

    /// Storage backend error.
    #[error("Storage error: {0}")]
    Storage(String),

    /// Chunk integrity check failed.
    #[error("Integrity error: expected hash {expected}, got {actual}")]
    IntegrityMismatch {
        /// Expected SHA-256 hex hash.
        expected: String,
        /// Actual SHA-256 hex hash.
        actual: String,
    },

    /// Database error.
    #[error("Database error: {0}")]
    Database(String),

    /// Configuration error.
    #[error("Configuration error: {0}")]
    Config(String),

    /// License error.
    #[error("License error: {0}")]
    License(String),

    /// Feature not supported on this platform.
    #[error("Platform not supported: {0}")]
    PlatformUnsupported(String),

    /// Generic internal error.
    #[error("Internal error: {0}")]
    Internal(String),

    /// Run was cancelled by the user.
    #[error("Cancelled")]
    Cancelled,

    /// One or more source paths are unavailable (disk removed, volume unmounted).
    /// The run is skipped; no files are marked as deleted.
    #[error("Source unavailable: {0}")]
    SourceUnavailable(String),

    /// A source path that was accessible when the run started became unavailable
    /// mid-run (disk physically removed during backup).  The run is aborted;
    /// any packs already uploaded are left as orphans for the next GC pass.
    #[error("Source lost mid-run: {0}")]
    SourceLost(String),

    /// Backup run was paused.
    #[error("Backup paused")]
    Paused,

    /// One or more packs are in an archive storage tier and cannot be read until retrieved.
    #[error("Archive retrieval required for {} pack(s) in {storage_class}", packs.len())]
    ArchiveRetrievalRequired {
        /// Remote paths of the affected packs (e.g. "packs/<uuid>.pack").
        packs: Vec<String>,
        /// S3 storage class string (e.g. "GLACIER", "DEEP_ARCHIVE").
        storage_class: String,
    },
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, Error>;

// - Error classifier --------------------

/// User-facing category of a failure surfaced to GUI / TUI / CLI.
///
/// Each category corresponds to a localizable message + a hint about whether
/// the operation is worth retrying immediately.  The category is emitted as a
/// stable string code so it survives the tonic boundary intact (where the
/// engine's structured `Error` would otherwise collapse to a free-form string).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCategory {
    /// Transient network hiccup - TLS reset, response body cut off,
    /// connection closed mid-stream.  Retry is usually safe.
    NetworkTransient,
    /// Authentication / credential failure on the remote backend.
    /// User must re-enter credentials or re-authorize the OAuth backend.
    AuthExpired,
    /// Remote object referenced by the manifest cannot be found.
    /// Backup data is incomplete - usually means a pack was deleted
    /// out-of-band (manual rm, lifecycle policy, retention on another machine).
    StorageMissingObject,
    /// One or more packs are in an archive storage tier (Glacier, Deep
    /// Archive) and must be retrieved before the restore can proceed.
    ArchiveRetrievalRequired,
    /// Decrypt or chunk-hash verification failed.  Indicates remote-side
    /// corruption or the wrong master key.
    CryptoFailed,
    /// Destination filesystem ran out of space mid-restore.
    IoOutOfSpace,
    /// Destination filesystem refused a write (read-only mount, ACL,
    /// missing parent dir, etc.).
    IoPermissionDenied,
    /// Source path missing (removable drive unplugged, network share
    /// unmounted, etc.).  Run is skipped.
    SourceUnavailable,
    /// Run was cancelled by the user or by daemon shutdown.
    Cancelled,
    /// Run was paused.
    Paused,
    /// License has expired or trial elapsed.
    LicenseExpired,
    /// Catch-all for daemon-internal failures that don't map to a
    /// user-visible category.  Surface the raw message and let the user
    /// file a support report.
    Internal,
}

impl ErrorCategory {
    /// Stable string code emitted at the tonic boundary.
    /// Matches the keys under `gui.error.*` in the locale files.
    pub fn code(&self) -> &'static str {
        match self {
            ErrorCategory::NetworkTransient => "network_transient",
            ErrorCategory::AuthExpired => "auth_expired",
            ErrorCategory::StorageMissingObject => "storage_missing_object",
            ErrorCategory::ArchiveRetrievalRequired => "archive_retrieval_required",
            ErrorCategory::CryptoFailed => "crypto_failed",
            ErrorCategory::IoOutOfSpace => "io_out_of_space",
            ErrorCategory::IoPermissionDenied => "io_permission_denied",
            ErrorCategory::SourceUnavailable => "source_unavailable",
            ErrorCategory::Cancelled => "cancelled",
            ErrorCategory::Paused => "paused",
            ErrorCategory::LicenseExpired => "license_expired",
            ErrorCategory::Internal => "internal",
        }
    }

    /// True when the operation is worth retrying without user intervention.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            ErrorCategory::NetworkTransient | ErrorCategory::Cancelled | ErrorCategory::Paused,
        )
    }
}

/// Classify an `Error` into a user-facing category.
///
/// The classification is intentionally conservative: ambiguous storage
/// errors fall through to `NetworkTransient` only when the message text
/// matches well-known transient patterns; otherwise they go to `Internal`
/// so users see the original message instead of misleading suggestions.
pub fn classify_error(err: &Error) -> ErrorCategory {
    match err {
        Error::Cancelled => ErrorCategory::Cancelled,
        Error::Paused => ErrorCategory::Paused,
        Error::SourceUnavailable(_) | Error::SourceLost(_) => ErrorCategory::SourceUnavailable,
        Error::ArchiveRetrievalRequired { .. } => ErrorCategory::ArchiveRetrievalRequired,
        Error::IntegrityMismatch { .. } => ErrorCategory::CryptoFailed,
        Error::Crypto(_) => ErrorCategory::CryptoFailed,
        Error::License(_) => ErrorCategory::LicenseExpired,
        Error::Io(e) => {
            use std::io::ErrorKind;
            match e.kind() {
                ErrorKind::PermissionDenied => ErrorCategory::IoPermissionDenied,
                _ => {
                    // Out-of-space surfaces as `Other` on some platforms;
                    // detect via message substring as a fallback.
                    let s = e.to_string().to_lowercase();
                    if s.contains("no space") || s.contains("disk full") {
                        ErrorCategory::IoOutOfSpace
                    } else {
                        ErrorCategory::Internal
                    }
                }
            }
        }
        Error::Storage(msg) => {
            let s = msg.to_lowercase();
            // Cold-storage InvalidObjectState surfaces inside a 403
            // response on S3 ("not valid for the object's storage class").
            // Must precede the generic 403 -> AuthExpired branch below or
            // the GUI shows "credentials expired" when the real issue is
            // that the object (often the snapshot-index itself, on legacy
            // sets without a per-path storage-class filter) is in
            // GLACIER / DEEP_ARCHIVE / AZURE_ARCHIVE and needs rehydration.
            if s.contains("invalidobjectstate")
                || s.contains("invalid for the object's storage class")
                || s.contains("blobarchived")
                || s.contains("archived state")
            {
                ErrorCategory::ArchiveRetrievalRequired
            }
            // Auth / credential failures next - these are not transient.
            else if s.contains("access denied")
                || s.contains("invalid credentials")
                || s.contains("expired token")
                || s.contains("unauthorized")
                || s.contains("403")
                || s.contains("401")
            {
                ErrorCategory::AuthExpired
            } else if s.contains("not found")
                || s.contains("nosuchkey")
                || s.contains("no such file")     // SFTP, local
                || s.contains("no such directory")
                || s.contains("404")
            {
                ErrorCategory::StorageMissingObject
            } else if s.contains("error sending request")
                || s.contains("error decoding response body")
                || s.contains("connection closed")
                || s.contains("connection reset")
                || s.contains("stream reset")
                || s.contains("timed out")
                || s.contains("timeout")
            {
                ErrorCategory::NetworkTransient
            } else {
                ErrorCategory::Internal
            }
        }
        _ => ErrorCategory::Internal,
    }
}

/// Format an `Error` for transmission across the tonic boundary so the
/// client can extract the structured category back out.  Format:
///
/// ```text
/// [bkp:<category_code>] <original message>
/// ```
///
/// The bracketed prefix is recognised by the GUI's error decoder; clients
/// that don't recognise it just show the message portion verbatim, which
/// is itself an improvement over a raw `tonic::Status::internal` dump.
pub fn format_classified(err: &Error) -> String {
    let cat = classify_error(err);
    format!("[bkp:{}] {}", cat.code(), err)
}
