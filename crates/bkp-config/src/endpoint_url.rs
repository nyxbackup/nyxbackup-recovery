// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! Shared parsing helpers for the `<scheme>://...` URL forms the editor /
//! config writes to disk for each storage backend.  Lives in `bkp-config`
//! so the main daemon (`bkp-daemon/src/services/config.rs::build_endpoint`)
//! and the standalone Recovery Tool
//! (`bkp-recover/src/commands.rs::endpoint_to_toml`) can both call it and
//! stay in lock-step.
//!
//! Centralizing URL parsing here prevents a malformed-config class of
//! bug where the whole URL is written into the bucket field (e.g.
//! `bucket = "b2://nyx-backup-1/ant_b2"`), which makes the B2 backend.s
//! `GET /file/<bucket>/<filename>` resolve to an invalid path and
//! return 400.

/// Parse `<scheme>://<bucket>[/<prefix>]` into `(bucket, prefix)`.
///
/// - `s3://my-bucket/data` -> `("my-bucket", "data")`
/// - `b2://nyx-backup-1/ant_b2` -> `("nyx-backup-1", "ant_b2")`
/// - `s3://my-bucket` -> `("my-bucket", "")`
/// - `my-bucket/data` (no scheme) -> `("my-bucket", "data")` - tolerant of users
///   who paste just the bucket-and-prefix form.
/// - `my-bucket` -> `("my-bucket", "")`
///
/// Borrowed slices into the input - callers control allocation.
pub fn parse_bucket_prefix<'a>(url: &'a str, scheme: &str) -> (&'a str, &'a str) {
    let rest = url.strip_prefix(scheme).unwrap_or(url);
    rest.find('/')
        .map(|i| (&rest[..i], &rest[i + 1..]))
        .unwrap_or((rest, ""))
}

/// Parse `azure://<account>/<container>[/<prefix>]` into the three parts.
///
/// - `azure://acme/backups/2026` -> `("acme", "backups", "2026")`
/// - `azure://acme/backups` -> `("acme", "backups", "")`
/// - `acme/backups` (no scheme) -> `("acme", "backups", "")`
pub fn parse_azure_url(url: &str) -> (&str, &str, &str) {
    let rest = url.strip_prefix("azure://").unwrap_or(url);
    let mut parts = rest.splitn(3, '/');
    let account = parts.next().unwrap_or("");
    let container = parts.next().unwrap_or("");
    let prefix = parts.next().unwrap_or("");
    (account, container, prefix)
}

// - OAuth blob helpers ---------------------------------------------------------
//
// These were previously in `bkp_storage::backends::oauth` (still re-exported
// there for the storage backends).  Pulled up to `bkp-config` so the shared
// `endpoint_to_toml` below doesn't need to depend on `bkp-storage`, which
// would create a storage->config edge in the dependency graph.

/// Parse the secret JSON blob stored in `secret_access_key` for Google Drive
/// and Dropbox: `{"client_secret":"...","refresh_token":"..."}`.
/// Returns `(client_secret_or_app_secret, refresh_token)`.
pub fn parse_oauth_blob(blob: Option<&str>) -> (String, String) {
    let blob = match blob {
        Some(b) if !b.is_empty() => b,
        _ => return (String::new(), String::new()),
    };
    let v: serde_json::Value = serde_json::from_str(blob).unwrap_or_default();
    let secret = v["client_secret"]
        .as_str()
        .or_else(|| v["app_secret"].as_str())
        .unwrap_or("")
        .to_string();
    let rt = v["refresh_token"].as_str().unwrap_or("").to_string();
    (secret, rt)
}

/// Like `parse_oauth_blob` but also extracts `tenant_id` (for OneDrive).
/// Returns `(client_secret, refresh_token, tenant_id)`.
pub fn parse_oauth_blob_onedrive(
    blob: Option<&str>,
    region_fallback: Option<&str>,
) -> (String, String, String) {
    let blob = match blob {
        Some(b) if !b.is_empty() => b,
        _ => {
            return (
                String::new(),
                String::new(),
                region_fallback.unwrap_or("common").to_string(),
            );
        }
    };
    let v: serde_json::Value = serde_json::from_str(blob).unwrap_or_default();
    let secret = v["client_secret"].as_str().unwrap_or("").to_string();
    let rt = v["refresh_token"].as_str().unwrap_or("").to_string();
    let tenant = v["tenant_id"]
        .as_str()
        .or(region_fallback)
        .unwrap_or("common")
        .to_string();
    (secret, rt, tenant)
}

// - Single source of truth for URL -> EndpointConfig parsing ------------------

/// Build a fully-populated [`EndpointConfig`] from the user-typed inputs the
/// editor / proto / recovery form supplies: URL string, key ID, secret,
/// region (which carries the endpoint URL for s3_compat and the SMB mount
/// path for SMB - editor conventions kept stable for proto compat), and
/// storage class.  Shared by:
///
/// - `bkp-daemon` (when AddBackupSet / UpdateBackupSet / RetierBackupSet
///   constructs a config from incoming proto fields), and
/// - `bkp-recover` (when the Connect screen's typed-by-user fields land in
///   the recovery flow).
///
/// All URL parsing (s3://, b2://, gcs://, azure://, sftp://, smb://) goes
/// through this one function so URL parsing stays consistent everywhere
/// (a single source of truth for the `bucket = b2://...` parse).
pub fn build_endpoint(
    ep_id: &str,
    ep_type: &bkp_types::endpoint::EndpointType,
    url: &str,
    key_id: &str,
    secret: &str,
    region: &str,
    storage_class: &str,
) -> crate::EndpointConfig {
    use bkp_types::endpoint::EndpointType;
    use bkp_types::secret::Secret;
    use std::path::PathBuf;

    let ep_id = ep_id.to_string();
    let keychain_handle = ep_id.clone();

    let opt_key_id = if key_id.is_empty() {
        None
    } else {
        Some(key_id.to_string())
    };
    let opt_secret = if secret.is_empty() {
        None
    } else {
        Some(secret.to_string())
    };
    let opt_region = if region.is_empty() {
        None
    } else {
        Some(region.to_string())
    };
    let opt_class = if storage_class.is_empty() {
        None
    } else {
        Some(storage_class.to_string())
    };

    match ep_type {
        EndpointType::Local => crate::EndpointConfig {
            id: ep_id,
            endpoint_type: EndpointType::Local,
            bucket: None,
            prefix: None,
            region: None,
            storage_class: None,
            retrieval_tier: None,
            restore_lifetime_days: None,
            endpoint_url: None,
            host: None,
            port: None,
            remote_path: Some(PathBuf::from(url)),
            username: None,
            access_key_id: None,
            secret_access_key: None,
            keychain_handle,
            oauth_account_id: None,
        },
        EndpointType::S3Compatible => {
            let (bucket, prefix) = parse_bucket_prefix(url, "s3://");
            crate::EndpointConfig {
                id: ep_id,
                endpoint_type: EndpointType::S3Compatible,
                bucket: Some(bucket.to_string()),
                prefix: if prefix.is_empty() {
                    None
                } else {
                    Some(prefix.to_string())
                },
                region: None,
                storage_class: opt_class,
                retrieval_tier: None,
                restore_lifetime_days: None,
                // The UI sends the endpoint URL in storage_region for s3_compat.
                endpoint_url: opt_region,
                host: None,
                port: None,
                remote_path: None,
                username: None,
                access_key_id: opt_key_id.map(Secret::new),
                secret_access_key: opt_secret.map(Secret::new),
                keychain_handle,
                oauth_account_id: None,
            }
        }
        EndpointType::S3 | EndpointType::BackblazeB2 => {
            let (bucket, prefix) = if url.starts_with("b2://") {
                parse_bucket_prefix(url, "b2://")
            } else {
                parse_bucket_prefix(url, "s3://")
            };
            crate::EndpointConfig {
                id: ep_id,
                endpoint_type: ep_type.clone(),
                bucket: Some(bucket.to_string()),
                prefix: if prefix.is_empty() {
                    None
                } else {
                    Some(prefix.to_string())
                },
                region: opt_region,
                storage_class: opt_class,
                retrieval_tier: None,
                restore_lifetime_days: None,
                endpoint_url: None,
                host: None,
                port: None,
                remote_path: None,
                username: None,
                access_key_id: opt_key_id.map(Secret::new),
                secret_access_key: opt_secret.map(Secret::new),
                keychain_handle,
                oauth_account_id: None,
            }
        }
        EndpointType::AzureBlob => {
            let (account, container, prefix) = parse_azure_url(url);
            crate::EndpointConfig {
                id: ep_id,
                endpoint_type: EndpointType::AzureBlob,
                bucket: Some(container.to_string()),
                prefix: if prefix.is_empty() {
                    None
                } else {
                    Some(prefix.to_string())
                },
                region: None,
                storage_class: None,
                retrieval_tier: None,
                restore_lifetime_days: None,
                endpoint_url: None,
                host: None,
                port: None,
                remote_path: None,
                username: None,
                access_key_id: if account.is_empty() {
                    opt_key_id.map(Secret::new)
                } else {
                    Some(Secret::new(account.to_string()))
                },
                secret_access_key: opt_secret.map(Secret::new),
                keychain_handle,
                oauth_account_id: None,
            }
        }
        EndpointType::Sftp => {
            let rest = url.strip_prefix("sftp://").unwrap_or(url);
            let (userhost, path) = rest
                .find('/')
                .map(|i| (&rest[..i], &rest[i..]))
                .unwrap_or((rest, "/"));
            let (user, host) = userhost
                .find('@')
                .map(|i| (&userhost[..i], &userhost[i + 1..]))
                .unwrap_or(("", userhost));
            let host = host.trim_end_matches(':');
            crate::EndpointConfig {
                id: ep_id,
                endpoint_type: EndpointType::Sftp,
                bucket: None,
                prefix: None,
                region: None,
                storage_class: None,
                retrieval_tier: None,
                restore_lifetime_days: None,
                endpoint_url: None,
                host: Some(host.to_string()),
                port: None,
                remote_path: Some(PathBuf::from(path)),
                username: if user.is_empty() {
                    None
                } else {
                    Some(user.to_string())
                },
                access_key_id: opt_key_id.map(Secret::new),
                secret_access_key: opt_secret.map(Secret::new),
                keychain_handle,
                oauth_account_id: None,
            }
        }
        EndpointType::Smb => {
            let rest = url.strip_prefix("smb://").unwrap_or(url);
            let (userhost, after_host) = rest
                .find('/')
                .map(|i| (&rest[..i], &rest[i + 1..]))
                .unwrap_or((rest, ""));
            let (user, host) = userhost
                .find('@')
                .map(|i| (&userhost[..i], &userhost[i + 1..]))
                .unwrap_or(("", userhost));
            let (share, prefix) = after_host.split_once('/').unwrap_or((after_host, ""));
            let mount_path = if region.is_empty() {
                None
            } else {
                Some(PathBuf::from(region))
            };
            crate::EndpointConfig {
                id: ep_id,
                endpoint_type: EndpointType::Smb,
                bucket: Some(share.to_string()),
                prefix: if prefix.is_empty() {
                    None
                } else {
                    Some(prefix.to_string())
                },
                region: None,
                storage_class: None,
                retrieval_tier: None,
                restore_lifetime_days: None,
                endpoint_url: None,
                host: Some(host.to_string()),
                port: None,
                remote_path: mount_path,
                username: if user.is_empty() {
                    None
                } else {
                    Some(user.to_string())
                },
                access_key_id: opt_key_id.map(Secret::new),
                secret_access_key: opt_secret.map(Secret::new),
                keychain_handle,
                oauth_account_id: None,
            }
        }
        EndpointType::Gcs => {
            let (bucket, prefix) = parse_bucket_prefix(url, "gcs://");
            crate::EndpointConfig {
                id: ep_id,
                endpoint_type: EndpointType::Gcs,
                bucket: Some(bucket.to_string()),
                prefix: if prefix.is_empty() {
                    None
                } else {
                    Some(prefix.to_string())
                },
                region: None,
                storage_class: opt_class,
                retrieval_tier: None,
                restore_lifetime_days: None,
                endpoint_url: None,
                host: None,
                port: None,
                remote_path: None,
                username: None,
                access_key_id: None,
                secret_access_key: opt_secret.map(Secret::new),
                keychain_handle,
                oauth_account_id: None,
            }
        }
        EndpointType::GoogleDrive => crate::EndpointConfig {
            id: ep_id,
            endpoint_type: EndpointType::GoogleDrive,
            bucket: if url.is_empty() {
                None
            } else {
                Some(url.to_string())
            },
            prefix: None,
            region: None,
            storage_class: None,
            retrieval_tier: None,
            restore_lifetime_days: None,
            endpoint_url: None,
            host: None,
            port: None,
            remote_path: None,
            username: None,
            access_key_id: opt_key_id.map(Secret::new),
            secret_access_key: opt_secret.map(Secret::new),
            keychain_handle,
            oauth_account_id: None,
        },
        EndpointType::OneDrive => crate::EndpointConfig {
            id: ep_id,
            endpoint_type: EndpointType::OneDrive,
            bucket: if url.is_empty() {
                None
            } else {
                Some(url.to_string())
            },
            prefix: None,
            region: opt_region, // tenant_id slot
            storage_class: None,
            retrieval_tier: None,
            restore_lifetime_days: None,
            endpoint_url: None,
            host: None,
            port: None,
            remote_path: None,
            username: None,
            access_key_id: opt_key_id.map(Secret::new),
            secret_access_key: opt_secret.map(Secret::new),
            keychain_handle,
            oauth_account_id: None,
        },
        EndpointType::Dropbox => crate::EndpointConfig {
            id: ep_id,
            endpoint_type: EndpointType::Dropbox,
            bucket: if url.is_empty() {
                None
            } else {
                Some(url.to_string())
            },
            prefix: None,
            region: None,
            storage_class: None,
            retrieval_tier: None,
            restore_lifetime_days: None,
            endpoint_url: None,
            host: None,
            port: None,
            remote_path: None,
            username: None,
            access_key_id: opt_key_id.map(Secret::new),
            secret_access_key: opt_secret.map(Secret::new),
            keychain_handle,
            oauth_account_id: None,
        },
        EndpointType::WebDav => crate::EndpointConfig {
            id: ep_id,
            endpoint_type: EndpointType::WebDav,
            bucket: None,
            prefix: None,
            region: None,
            storage_class: None,
            retrieval_tier: None,
            restore_lifetime_days: None,
            endpoint_url: if url.is_empty() {
                None
            } else {
                Some(url.to_string())
            },
            host: None,
            port: None,
            remote_path: None,
            username: opt_key_id.clone(),
            access_key_id: opt_key_id.map(Secret::new),
            secret_access_key: opt_secret.map(Secret::new),
            keychain_handle,
            oauth_account_id: None,
        },
        _ => crate::EndpointConfig {
            id: ep_id,
            endpoint_type: ep_type.clone(),
            bucket: None,
            prefix: None,
            region: None,
            storage_class: None,
            retrieval_tier: None,
            restore_lifetime_days: None,
            endpoint_url: None,
            host: None,
            port: None,
            remote_path: Some(PathBuf::from(url)),
            username: None,
            access_key_id: None,
            secret_access_key: None,
            keychain_handle,
            oauth_account_id: None,
        },
    }
}

// - Single source of truth for TOML emission ----------------------------------

/// Build a minimal TOML config string for `bkp_storage::registry::build_backend`
/// from an [`EndpointConfig`].  Single source of truth shared by:
///
/// - `bkp-daemon` (when the daemon needs to construct a backend ad-hoc for
///   a `RunBackup` / `Restore` / `Retention` / `Integrity` operation), and
/// - `bkp-recover` (the standalone Recovery Tool, which constructs an
///   `EndpointConfig` from user-typed credentials and calls this function
///   to produce the same TOML the daemon would).
///
/// Per-backend shape mirrors the legacy daemon-local implementation
/// byte-for-byte; the only structural change is the move out of
/// `bkp-daemon/src/services/backup.rs` into this shared crate so the
/// Recovery Tool can't drift.
pub fn endpoint_to_toml(ep: &crate::EndpointConfig) -> String {
    use bkp_types::endpoint::EndpointType;
    match ep.endpoint_type {
        EndpointType::Local => {
            let path = ep
                .remote_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "/tmp/bkp-local".to_string());
            format!("root = {:?}\n", path)
        }
        EndpointType::S3 => {
            let mut t = format!(
                "bucket = {:?}\nregion = {:?}\nprefix = {:?}\n",
                ep.bucket.as_deref().unwrap_or(""),
                ep.region.as_deref().unwrap_or("us-east-1"),
                ep.prefix.as_deref().unwrap_or(""),
            );
            if let (Some(kid), Some(sec)) = (&ep.access_key_id, &ep.secret_access_key) {
                t.push_str(&format!(
                    "access_key_id = {:?}\nsecret_access_key = {:?}\n",
                    kid.expose(),
                    sec.expose()
                ));
            }
            if let Some(class) = ep.storage_class.as_deref()
                && !class.is_empty()
            {
                t.push_str(&format!("storage_class = {:?}\n", class));
            }
            // Glacier retrieval tier + restore
            // lifetime, S3 only.  Persisted only when explicitly set
            // so legacy configs continue to behave like the earlier
            // hardcoded defaults (Standard / 7 days, applied at backend
            // construction).
            if let Some(t_str) = ep.retrieval_tier.as_deref()
                && !t_str.is_empty()
            {
                t.push_str(&format!("retrieval_tier = {:?}\n", t_str));
            }
            if let Some(days) = ep.restore_lifetime_days
                && days > 0
            {
                t.push_str(&format!("restore_lifetime_days = {}\n", days));
            }
            t
        }
        EndpointType::S3Compatible => {
            let mut t = format!(
                "bucket = {:?}\nendpoint_url = {:?}\nprefix = {:?}\n",
                ep.bucket.as_deref().unwrap_or(""),
                ep.endpoint_url.as_deref().unwrap_or(""),
                ep.prefix.as_deref().unwrap_or(""),
            );
            if let (Some(kid), Some(sec)) = (&ep.access_key_id, &ep.secret_access_key) {
                t.push_str(&format!(
                    "access_key_id = {:?}\nsecret_access_key = {:?}\n",
                    kid.expose(),
                    sec.expose()
                ));
            }
            if let Some(class) = ep.storage_class.as_deref()
                && !class.is_empty()
            {
                t.push_str(&format!("storage_class = {:?}\n", class));
            }
            // forward Glacier tier + lifetime to
            // S3-compat too; harmless on providers that ignore them.
            if let Some(t_str) = ep.retrieval_tier.as_deref()
                && !t_str.is_empty()
            {
                t.push_str(&format!("retrieval_tier = {:?}\n", t_str));
            }
            if let Some(days) = ep.restore_lifetime_days
                && days > 0
            {
                t.push_str(&format!("restore_lifetime_days = {}\n", days));
            }
            t
        }
        EndpointType::AzureBlob => {
            let mut t = format!(
                "account = {:?}\ncontainer = {:?}\nprefix = {:?}\n",
                ep.access_key_id.as_deref().unwrap_or(""),
                ep.bucket.as_deref().unwrap_or(""),
                ep.prefix.as_deref().unwrap_or(""),
            );
            if let Some(sec) = &ep.secret_access_key {
                t.push_str(&format!("access_key = {:?}\n", sec.expose()));
            }
            if let Some(class) = ep.storage_class.as_deref()
                && !class.is_empty()
            {
                t.push_str(&format!("storage_class = {:?}\n", class));
            }
            t
        }
        EndpointType::BackblazeB2 => {
            let mut t = format!(
                "bucket = {:?}\nprefix = {:?}\n",
                ep.bucket.as_deref().unwrap_or(""),
                ep.prefix.as_deref().unwrap_or(""),
            );
            if let (Some(kid), Some(sec)) = (&ep.access_key_id, &ep.secret_access_key) {
                t.push_str(&format!(
                    "application_key_id = {:?}\napplication_key = {:?}\n",
                    kid.expose(),
                    sec.expose()
                ));
            }
            t
        }
        EndpointType::Gcs => {
            let mut t = format!(
                "bucket = {:?}\nprefix = {:?}\n",
                ep.bucket.as_deref().unwrap_or(""),
                ep.prefix.as_deref().unwrap_or(""),
            );
            if let Some(json) = &ep.secret_access_key
                && !json.is_empty()
            {
                t.push_str(&format!("service_account_key_json = {:?}\n", json.expose()));
            }
            if let Some(class) = ep.storage_class.as_deref()
                && !class.is_empty()
            {
                t.push_str(&format!("storage_class = {:?}\n", class));
            }
            t
        }
        EndpointType::Sftp => {
            let mut t = format!(
                "host = {:?}\nport = {}\nusername = {:?}\nbase_path = {:?}\n",
                ep.host.as_deref().unwrap_or(""),
                ep.port.unwrap_or(22),
                ep.username.as_deref().unwrap_or(""),
                ep.remote_path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
            );
            if let Some(pw) = &ep.secret_access_key
                && !pw.is_empty()
            {
                t.push_str(&format!("password = {:?}\n", pw.expose()));
            }
            t
        }
        EndpointType::GoogleDrive => {
            let folder_id = ep.bucket.as_deref().unwrap_or("");
            let refresh_token = ep.secret_access_key.as_deref().unwrap_or("");
            format!(
                "folder_id = {:?}\nrefresh_token = {:?}\n",
                folder_id, refresh_token
            )
        }
        EndpointType::OneDrive => {
            // OneDrive public-client OAuth: no client_secret
            // is sent to Microsoft, so we don't emit one on the
            // backend-config TOML either.  parse_oauth_blob_onedrive
            // still returns the historic 3-tuple; we drop the secret
            // field with `let _`.
            let folder_path = ep.bucket.as_deref().unwrap_or("/NyxBackup");
            let client_id = ep.access_key_id.as_deref().unwrap_or("");
            let (_unused_secret, refresh_token, tenant_id) =
                parse_oauth_blob_onedrive(ep.secret_access_key.as_deref(), ep.region.as_deref());
            format!(
                "folder_path = {:?}\nclient_id = {:?}\ntenant_id = {:?}\nrefresh_token = {:?}\n",
                folder_path, client_id, tenant_id, refresh_token,
            )
        }
        EndpointType::Dropbox => {
            let folder_path = ep.bucket.as_deref().unwrap_or("/NyxBackup");
            let raw = ep.secret_access_key.as_deref().unwrap_or("");
            let refresh_token = if raw.starts_with('{') {
                let (_, rt) = parse_oauth_blob(Some(raw));
                rt
            } else {
                raw.to_string()
            };
            format!(
                "folder_path = {:?}\nrefresh_token = {:?}\n",
                folder_path, refresh_token
            )
        }
        EndpointType::Smb => {
            let mut t = format!(
                "host = {:?}\nshare = {:?}\nbase_path = {:?}\n",
                ep.host.as_deref().unwrap_or(""),
                ep.bucket.as_deref().unwrap_or(""),
                ep.prefix.as_deref().unwrap_or(""),
            );
            if let Some(user) = &ep.username
                && !user.is_empty()
            {
                t.push_str(&format!("username = {:?}\n", user));
            }
            if let Some(pw) = &ep.secret_access_key
                && !pw.is_empty()
            {
                t.push_str(&format!("password = {:?}\n", pw.expose()));
            }
            if let Some(mp) = &ep.remote_path {
                let mp_str = mp.display().to_string();
                if !mp_str.is_empty() {
                    t.push_str(&format!("mount_path = {:?}\n", mp_str));
                }
            }
            t
        }
        EndpointType::WebDav => {
            let mut t = format!(
                "endpoint_url = {:?}\n",
                ep.endpoint_url.as_deref().unwrap_or(""),
            );
            let user = ep
                .username
                .as_deref()
                .or(ep.access_key_id.as_deref())
                .unwrap_or("");
            if !user.is_empty() {
                t.push_str(&format!("username = {:?}\n", user));
            }
            if let Some(pw) = &ep.secret_access_key
                && !pw.is_empty()
            {
                t.push_str(&format!("password = {:?}\n", pw.expose()));
            }
            t
        }
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b2_full_url_split() {
        assert_eq!(
            parse_bucket_prefix("b2://nyx-backup-1/ant_b2", "b2://"),
            ("nyx-backup-1", "ant_b2"),
        );
    }

    #[test]
    fn s3_full_url_split() {
        assert_eq!(
            parse_bucket_prefix("s3://acme/data", "s3://"),
            ("acme", "data")
        );
        assert_eq!(parse_bucket_prefix("s3://acme", "s3://"), ("acme", ""));
    }

    #[test]
    fn no_scheme_tolerated() {
        assert_eq!(parse_bucket_prefix("acme/data", "s3://"), ("acme", "data"));
        assert_eq!(parse_bucket_prefix("acme", "s3://"), ("acme", ""));
    }

    #[test]
    fn azure_three_parts() {
        assert_eq!(
            parse_azure_url("azure://acme/backups/2026"),
            ("acme", "backups", "2026"),
        );
        assert_eq!(
            parse_azure_url("azure://acme/backups"),
            ("acme", "backups", "")
        );
        assert_eq!(parse_azure_url("acme/backups"), ("acme", "backups", ""));
    }
}
