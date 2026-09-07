// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! bkp-config - TOML configuration load, save, and validation.
//!
//! The configuration file format is defined in data format spec Section 6.
//! The `access_key_id` / `secret_access_key` fields in [`EndpointConfig`] are
//! in-memory only (`#[serde(skip_serializing)]`) - they are never written to
//! the TOML file.

// Unsafe code is denied crate-wide; the single exception is `set_windows_acl`
// which calls Win32 security APIs that have no safe wrapper in the ecosystem.
#![deny(unsafe_code)]
#![deny(missing_docs)]
#![deny(clippy::unwrap_used)]

pub mod endpoint_url;
pub mod local_safety;

use std::path::{Path, PathBuf};

use bkp_types::endpoint::EndpointType;
use bkp_types::error::{Error, Result};
use bkp_types::secret::Secret;
use serde::{Deserialize, Serialize};

// - Endpoint config ------------------------------

/// Configuration for a single backup endpoint within a backup set.
///
/// Corresponds to the `[[backup_set.endpoint]]` array-of-tables in `config.toml`
/// (data format spec Section 6.2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndpointConfig {
    /// Stable string identifier for this endpoint (e.g. `"ep-0001"`).
    pub id: String,
    /// Provider type string (e.g. `"s3"`, `"azure_blob"`, `"local"`).
    #[serde(rename = "type")]
    pub endpoint_type: EndpointType,
    /// Bucket or container name (S3, Azure, B2, GCS).
    #[serde(default)]
    pub bucket: Option<String>,
    /// Key prefix / path within the bucket.
    #[serde(default)]
    pub prefix: Option<String>,
    /// AWS region (S3 only).
    #[serde(default)]
    pub region: Option<String>,
    /// Provider-specific storage tier (e.g. `"STANDARD_IA"`, `"Cool"`).
    #[serde(default)]
    pub storage_class: Option<String>,
    /// Glacier retrieval tier the engine asks for
    /// when restoring archived packs.  Valid values: `"Standard"` (3-5 h,
    /// default), `"Bulk"` (5-12 h, cheaper), `"Expedited"` (1-5 min,
    /// pricier; not supported on `DEEP_ARCHIVE`).  Empty -> Standard.
    /// S3 / S3-compatible only - silently ignored on other backends.
    #[serde(default)]
    pub retrieval_tier: Option<String>,
    /// how many days the rehydrated copy stays in
    /// the temporary Standard tier before reverting to Glacier.
    /// Clamped to 1..=30 in the backend; empty / 0 -> 7.
    #[serde(default)]
    pub restore_lifetime_days: Option<u32>,
    /// Endpoint URL override (S3-compatible providers).
    #[serde(default)]
    pub endpoint_url: Option<String>,
    /// Hostname (SFTP, SMB).
    #[serde(default)]
    pub host: Option<String>,
    /// Port (SFTP).
    #[serde(default)]
    pub port: Option<u16>,
    /// Remote path (SFTP, SMB, local).
    #[serde(default)]
    pub remote_path: Option<PathBuf>,
    /// Username (SFTP, SMB).
    #[serde(default)]
    pub username: Option<String>,
    /// Access key ID (S3 access key, B2 key ID, etc.).
    ///
    /// **Never serialized to TOML** - loaded from the OS keychain at runtime
    /// via the OS keychain keyed on [`keychain_handle`](Self::keychain_handle).
    /// May still be present in old config files for migration; the main app
    /// will move them to the keychain automatically.
    /// Wrapped in [`Secret`]: zeroed-on-drop + redacted in
    /// `Debug` output.  Most read sites work unchanged via `Deref`
    /// (e.g. `ep.access_key_id.as_deref()` still returns `Option<&str>`).
    /// Assignment sites must wrap with `Secret::new(...)`.
    #[serde(default, skip_serializing)]
    pub access_key_id: Option<Secret<String>>,
    /// Secret (S3 secret key, B2 app key, Azure connection string, etc.).
    ///
    /// **Never serialized to TOML** - see [`access_key_id`](Self::access_key_id).
    #[serde(default, skip_serializing)]
    pub secret_access_key: Option<Secret<String>>,
    /// OS keychain handle from which credentials are fetched at runtime.
    pub keychain_handle: String,
    /// account identifier for OAuth backends
    /// (Google Drive, OneDrive, Dropbox).  Populated from the OAuth
    /// result's email at connect time and used by destination-overlap
    /// detection so two backup sets pointing at the same folder path
    /// but on **different OAuth accounts** are not flagged as
    /// overlapping.  Stored lower-cased; empty / None matches anything
    /// (the earlier behavior - "every OAuth set of this type
    /// overlaps" - is preserved for sets created before this field
    /// existed).  Never used as a credential.
    #[serde(default)]
    pub oauth_account_id: Option<String>,
}

// - File permission hardening ------------------

/// Restrict `path` so that only the current user (daemon account) can read
/// or write it.
///
/// | Platform     | Result                                      |
/// |-------------|---------------------------------------------|
/// | Linux/macOS  | `chmod 0600` - owner read/write only       |
/// | Windows      | Warning logged; full ACL restriction is a  |
/// |              | Phase-2 item requiring Win32 API calls.     |
///
/// This is called automatically by the main app.  A failure to restrict
/// permissions is returned as an error so the caller can decide whether to
/// abort or warn - for most callers an overly-permissive config file is a
/// security concern that warrants attention.
/// Restrict `path` to owner/SYSTEM + Administrators only.
///
/// Use for **secret files** (credential blobs, private keys).
/// On Unix: mode 0600. On Windows: SYSTEM + Administrators full access only.
pub fn restrict_file_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|e| {
            Error::Config(format!(
                "cannot restrict permissions on {}: {e}",
                path.display()
            ))
        })?;
    }

    // D:P - SYSTEM full access, Administrators full access, no others.
    #[cfg(target_os = "windows")]
    set_windows_acl(path, "D:P(A;;FA;;;SY)(A;;FA;;;BA)\0")?;

    #[cfg(not(any(unix, target_os = "windows")))]
    let _ = path;

    Ok(())
}

/// Restrict `path` so SYSTEM + Administrators can write; any authenticated
/// user can read.
///
/// Use for **config files** that contain no secrets but must not be
/// writable by ordinary users.  On Unix: mode 0644.
/// On Windows: SYSTEM + Administrators full access; Authenticated Users read.
pub fn restrict_config_file_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).map_err(|e| {
            Error::Config(format!(
                "cannot restrict permissions on {}: {e}",
                path.display()
            ))
        })?;
    }

    // D:P - SYSTEM full, Administrators full, Authenticated Users read.
    // AU (S-1-5-11) covers any logged-in user regardless of elevation level,
    // so the non-elevated GUI process can read config.toml.
    #[cfg(target_os = "windows")]
    set_windows_acl(path, "D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FR;;;AU)\0")?;

    #[cfg(not(any(unix, target_os = "windows")))]
    let _ = path;

    Ok(())
}

/// Restrict the admin-managed backup-hook scripts directory so only
/// SYSTEM + Administrators (Windows) / root (Unix) can **write** it, while the
/// daemon can read and execute the scripts placed there.
///
/// This is the load-bearing security boundary of the hook-script redesign: a
/// per-set hook names a script in this directory, so an unprivileged user must
/// not be able to introduce one.  Standard users have no direct access on
/// Windows - they reach scripts only via the daemon's `ListHookScripts` /
/// execution, both of which run as SYSTEM.
///
/// Unix: mode 0755 on a root-owned directory - others may read/traverse but not
/// write.  Windows: protected DACL (no inheritance from ProgramData, whose
/// default lets standard users create files), SYSTEM + Administrators full,
/// inherited by contained scripts.
pub fn restrict_hooks_dir_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).map_err(|e| {
            Error::Config(format!(
                "cannot restrict permissions on {}: {e}",
                path.display()
            ))
        })?;
    }

    // D:P - protected (drop ProgramData's inherited ACEs); SYSTEM + Builtin
    // Administrators full, inherited by files (OICI); nobody else.
    #[cfg(target_os = "windows")]
    set_windows_acl(path, "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)\0")?;

    #[cfg(not(any(unix, target_os = "windows")))]
    let _ = path;

    Ok(())
}

/// Apply a Windows DACL to `path` from a null-terminated SDDL string.
///
/// Uses `ConvertStringSecurityDescriptorToSecurityDescriptorW` to parse the
/// SDDL, extracts the DACL, then applies it via `SetNamedSecurityInfoW`.
/// `D:P` in the SDDL marks the DACL as protected so parent-directory
/// inheritance cannot override it.
#[cfg(target_os = "windows")]
#[allow(unsafe_code)]
fn set_windows_acl(path: &Path, sddl: &str) -> Result<()> {
    use std::ptr;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SE_FILE_OBJECT, SetNamedSecurityInfoW,
    };
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, PROTECTED_DACL_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR,
    };

    let sddl_w: Vec<u16> = sddl.encode_utf16().collect();

    // Build a null-terminated wide path string.
    let path_str = path.to_string_lossy();
    let path_w: Vec<u16> = path_str
        .encode_utf16()
        .chain(std::iter::once(0u16))
        .collect();

    unsafe {
        // Parse SDDL into an allocated SECURITY_DESCRIPTOR.
        let mut psd: PSECURITY_DESCRIPTOR = ptr::null_mut();
        let ok = ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl_w.as_ptr(),
            1, // SDDL_REVISION_1
            &mut psd,
            ptr::null_mut(),
        );
        if ok == 0 {
            return Err(Error::Config(format!(
                "ConvertStringSecurityDescriptor failed for {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            )));
        }

        // Extract the DACL pointer from the security descriptor.
        let mut dacl_present: i32 = 0;
        let mut dacl_ptr = ptr::null_mut();
        let mut dacl_defaulted: i32 = 0;
        GetSecurityDescriptorDacl(psd, &mut dacl_present, &mut dacl_ptr, &mut dacl_defaulted);

        // Apply the DACL to the file: replace existing DACL with our protected one.
        let rc = SetNamedSecurityInfoW(
            path_w.as_ptr() as *mut u16,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            dacl_ptr,
            ptr::null_mut(),
        );

        LocalFree(psd);

        if rc != 0 {
            return Err(Error::Config(format!(
                "SetNamedSecurityInfoW failed for {}: {}",
                path.display(),
                std::io::Error::from_raw_os_error(rc as i32)
            )));
        }
    }
    Ok(())
}
