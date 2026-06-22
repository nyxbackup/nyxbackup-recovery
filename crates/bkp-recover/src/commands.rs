// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! Tauri command handlers for the Recovery Tool GUI.  Thin wrappers over
//! the engine crates - everything runs in-process; no daemon, no IPC.

use crate::checkpoint::{Checkpoint, EndpointConfig as CheckpointEndpointConfig};
use crate::errors::{Ctx, ue, user_error};
use crate::recent::{RecentEndpoint, RecentList};
use crate::session::{EndpointParams, Phase, RestoreProgressView, SharedSession, SnapshotSummary};
use crate::settings::Settings;
use bkp_crypto::keys::MasterKey;
use bkp_manifest::{
    decode_manifest, decode_snapshot_index, manifest_remote_path, snapshot_index_remote_path,
};
use bkp_restore::{OverwriteMode, RestoreEngine, RestoreOwner, RestoreTarget};
use bkp_storage::backend::StorageBackend;
use bkp_storage::rate_limited::RateLimitedBackend;
use bkp_storage::registry;
use bkp_types::backup_set::BackupSetId;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tauri::State;
use tokio::sync::mpsc;
use uuid::Uuid;

use bkp_config::EndpointConfig;
use bkp_config::endpoint_url::{build_endpoint, endpoint_to_toml as shared_endpoint_to_toml};
use bkp_types::endpoint::EndpointType as TypesEndpointType;

/// Convert recovery-tool `EndpointParams` (collected from the Connect screen)
/// into a `bkp_config::EndpointConfig` by delegating to the same
/// `bkp_config::endpoint_url::build_endpoint` the main app's
/// AddBackupSet / UpdateBackupSet RPCs call.  Single source of truth for
/// URL parsing - recovery accepts every URL form the main app saves.
fn params_to_endpoint_config(p: &EndpointParams) -> EndpointConfig {
    let ep_type = match p.endpoint_type.as_str() {
        "local" => TypesEndpointType::Local,
        "s3" => TypesEndpointType::S3,
        "s3_compat" | "s3_compatible" => TypesEndpointType::S3Compatible,
        "azure_blob" => TypesEndpointType::AzureBlob,
        "backblaze_b2" => TypesEndpointType::BackblazeB2,
        "gcs" => TypesEndpointType::Gcs,
        "sftp" => TypesEndpointType::Sftp,
        "smb" => TypesEndpointType::Smb,
        "webdav" => TypesEndpointType::WebDav,
        "google_drive" => TypesEndpointType::GoogleDrive,
        "onedrive" => TypesEndpointType::OneDrive,
        "dropbox" => TypesEndpointType::Dropbox,
        _ => TypesEndpointType::Local,
    };

    // `build_endpoint` follows the main app's convention that the s3_compat
    // endpoint URL travels in the `region` slot (build_endpoint maps that
    // param into `endpoint_url`).  The Connect screen's "Endpoint URL" field
    // is bound to `storageRegion`, which arrives here as `p.region` - so for
    // s3_compat we must forward `p.region`, NOT `p.endpoint_url` (which is
    // unset for s3_compat).  Reading the wrong slot dropped the endpoint URL,
    // making s3_compat fall back to the default AWS endpoint and hang.
    let region_for_s3_compat = p.region.clone().unwrap_or_default();

    // WebDAV: the full base URL is the top `url` field.  Accept it from
    // either slot - prefer a non-empty `endpoint_url`, otherwise fall back to
    // `url`.  (The fallback must treat an empty string like None, not just
    // None, so a blank `endpoint_url` does not win over a populated `url`.)
    let url_for_build = if ep_type == TypesEndpointType::WebDav {
        match p.endpoint_url.as_deref() {
            Some(u) if !u.trim().is_empty() => u.to_string(),
            _ => p.url.clone(),
        }
    } else {
        p.url.clone()
    };

    build_endpoint(
        "ep-recover",
        &ep_type,
        &url_for_build,
        &p.key_id,
        &p.secret,
        &region_for_s3_compat,
        "", // storage_class - irrelevant for read-only recovery
    )
}

/// Recovery-tool wrapper around `bkp_config::endpoint_url::endpoint_to_toml`.
/// The shared producer is the single source of truth for TOML shape across
/// `bkp-daemon` and `bkp-recover`.
///
/// The shared producer has no `private_key_path` slot (that field is not part of
/// this fork's `EndpointConfig`), so the SFTP private-key / WebDAV client-cert
/// path is appended here, where the raw `EndpointParams` is available.
fn endpoint_to_toml(p: &EndpointParams) -> String {
    let key_path = p
        .private_key_path
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    // SFTP key auth: the `secret` field is the key *passphrase*, not a password.
    // Build the shared TOML with the secret cleared so its SFTP arm does not emit
    // `password = ...` (which would win over the key in the backend's auth
    // precedence: password is tried before the private key).  WebDAV keeps any
    // password AND may add a client certificate, so it is not suppressed.
    let suppress_secret = key_path.is_some() && p.endpoint_type == "sftp";
    let ep = if suppress_secret {
        let mut p2 = p.clone();
        p2.secret.clear();
        params_to_endpoint_config(&p2)
    } else {
        params_to_endpoint_config(p)
    };

    let mut t = shared_endpoint_to_toml(&ep);

    if let Some(path) = key_path {
        match p.endpoint_type.as_str() {
            // SftpConfig fields: private_key_path + private_key_passphrase.
            "sftp" => {
                t.push_str(&format!("private_key_path = {path:?}\n"));
                let pass = p.secret.trim();
                if !pass.is_empty() {
                    t.push_str(&format!("private_key_passphrase = {pass:?}\n"));
                }
            }
            // WebDavConfig field: client_cert_path (mutual-TLS PEM).
            "webdav" => {
                t.push_str(&format!("client_cert_path = {path:?}\n"));
            }
            _ => {}
        }
    }

    // s3_compat region override.  The shared TOML producer emits no `region`
    // line for s3_compat (the region slot carries the endpoint URL), so a real
    // region override is appended here; S3CompatConfig parses `region` and
    // otherwise defaults to `us-east-1`.
    if (p.endpoint_type == "s3_compat" || p.endpoint_type == "s3_compatible")
        && let Some(region) = p
            .s3_region
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
    {
        t.push_str(&format!("region = {region:?}\n"));
    }

    t
}

#[derive(Deserialize)]
pub struct ConnectArgs {
    pub endpoint_type: String,
    pub url: String,
    pub key_id: String,
    pub secret: String,
    pub label: Option<String>,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub endpoint_url: Option<String>,
    /// Optional S3 region override for S3-compatible endpoints (the `region`
    /// slot carries the endpoint URL for s3_compat, so a real region override
    /// travels here).
    #[serde(default)]
    pub s3_region: Option<String>,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub username: Option<String>,
    /// SFTP private-key file path, or WebDAV client-certificate PEM path.
    #[serde(default)]
    pub private_key_path: Option<String>,
}

/// Build a storage backend with the supplied args and probe it.  Does NOT
/// touch session state - this is the "Test connection" button's backing
/// command.  Returns Ok(()) on success; the descriptive error is the
/// `format!` payload on Err.
#[tauri::command]
pub async fn rec_test_connection(args: ConnectArgs) -> Result<(), String> {
    let params = EndpointParams {
        endpoint_type: args.endpoint_type.clone(),
        url: args.url.clone(),
        key_id: args.key_id.clone(),
        secret: args.secret,
        region: args.region.clone(),
        endpoint_url: args.endpoint_url.clone(),
        s3_region: args.s3_region.clone(),
        host: args.host.clone(),
        port: args.port,
        username: args.username.clone(),
        private_key_path: args.private_key_path.clone(),
    };
    let toml_blob = endpoint_to_toml(&params);
    tracing::info!(
        endpoint_type = %params.endpoint_type,
        url = %params.url,
        key_id_set = !params.key_id.is_empty(),
        secret_set = !params.secret.is_empty(),
        "rec_test_connection: probing"
    );
    // "Test connection" must fail FAST, not hang.  Two guards, because the
    // default retry policy is INFINITE and object_store's HTTP client has no
    // request timeout - a stalled connection (e.g. Cloudflare R2 can leave a
    // socket open) would otherwise spin the "Testing" spinner forever:
    //   1. a bounded retry config (a couple of quick tries, not u32::MAX), and
    //   2. an overall wall-clock timeout around the probe.
    // Restore operations keep the resilient infinite-retry default elsewhere.
    let retry = bkp_storage::retry::RetryConfig {
        max_retries: 2,
        base_delay: Duration::from_millis(400),
        max_delay: Duration::from_secs(2),
        ..bkp_storage::retry::RetryConfig::default()
    };
    let backend = registry::build_backend_with_retry(&params.endpoint_type, &toml_blob, retry)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "rec_test_connection: build_backend failed");
            ue(Ctx::Connect, &e)
        })?;

    const TEST_TIMEOUT: Duration = Duration::from_secs(20);
    match tokio::time::timeout(TEST_TIMEOUT, backend.list("manifests/")).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "rec_test_connection: manifests/ probe failed");
            return Err(ue(Ctx::Connect, &e));
        }
        Err(_) => {
            tracing::warn!("rec_test_connection: timed out");
            return Err(user_error(
                Ctx::Connect,
                &format!(
                    "test connection timed out after {}s",
                    TEST_TIMEOUT.as_secs()
                ),
            ));
        }
    }
    tracing::info!("rec_test_connection: ok");

    // Persist the verified credentials to the recents cache so the user
    // doesn't have to re-type them next session.  Same shape rec_connect
    // writes; the touch() call dedupes on (endpoint_type, url, key_id).
    let mut recent = RecentList::load();
    let label = args
        .label
        .unwrap_or_else(|| format!("{}: {}", params.endpoint_type, params.url));
    recent.touch(RecentEndpoint {
        endpoint_type: params.endpoint_type.clone(),
        url: params.url.clone(),
        key_id: params.key_id.clone(),
        secret: params.secret.clone(),
        region: params.region.clone().unwrap_or_default(),
        endpoint_url: params.endpoint_url.clone().unwrap_or_default(),
        label,
        last_used: 0,
    });
    let _ = recent.save();

    Ok(())
}

/// Build the storage backend + test connection.  Caches the backend in
/// session state so subsequent commands (list snapshots, restore) reuse it.
#[tauri::command]
pub async fn rec_connect(
    args: ConnectArgs,
    session: State<'_, SharedSession>,
) -> Result<Value, String> {
    let params = EndpointParams {
        endpoint_type: args.endpoint_type.clone(),
        url: args.url.clone(),
        key_id: args.key_id.clone(),
        secret: args.secret,
        region: args.region.clone(),
        endpoint_url: args.endpoint_url.clone(),
        s3_region: args.s3_region.clone(),
        host: args.host.clone(),
        port: args.port,
        username: args.username.clone(),
        private_key_path: args.private_key_path.clone(),
    };
    let toml_blob = endpoint_to_toml(&params);

    tracing::info!(
        endpoint_type = %params.endpoint_type,
        url = %params.url,
        key_id_set = !params.key_id.is_empty(),
        secret_set = !params.secret.is_empty(),
        region = ?params.region,
        endpoint_url = ?params.endpoint_url,
        host = ?params.host,
        port = ?params.port,
        "rec_connect: building backend",
    );

    let backend = registry::build_backend(&params.endpoint_type, &toml_blob)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "rec_connect: build_backend failed");
            ue(Ctx::Connect, &e)
        })?;

    tracing::info!("rec_connect: backend built, probing manifests/...");
    match backend.list("manifests/").await {
        Ok(objs) => {
            tracing::info!(count = objs.len(), "rec_connect: manifests/ probe ok");
        }
        Err(e) => {
            tracing::warn!(error = %e, "rec_connect: manifests/ probe failed");
            return Err(ue(Ctx::Connect, &e));
        }
    }

    // Persist to the recent-endpoints cache. this carries
    // the secret + region + endpoint_url for one-click re-connect; see
    // crate::recent for the file-ACL discussion.  Master key still NEVER
    // persisted.
    let mut recent = RecentList::load();
    let label = args
        .label
        .unwrap_or_else(|| format!("{}: {}", params.endpoint_type, params.url));
    recent.touch(RecentEndpoint {
        endpoint_type: params.endpoint_type.clone(),
        url: params.url.clone(),
        key_id: params.key_id.clone(),
        secret: params.secret.clone(),
        region: params.region.clone().unwrap_or_default(),
        endpoint_url: params.endpoint_url.clone().unwrap_or_default(),
        label: label.clone(),
        last_used: 0,
    });
    let _ = recent.save();

    {
        let mut s = session.write().await;
        s.endpoint = Some(params);
        s.backend = Some(backend);
        s.phase = Phase::Unlock;
    }

    Ok(json!({ "phase": "unlock", "label": label }))
}

/// Drop the endpoint + backend + key from session state.  Idempotent.
#[tauri::command]
pub async fn rec_disconnect(session: State<'_, SharedSession>) -> Result<(), String> {
    let mut s = session.write().await;
    s.endpoint = None;
    s.backend = None;
    s.master_key = None;
    s.snapshots.clear();
    s.phase = Phase::Connect;
    Ok(())
}

#[derive(Deserialize)]
pub struct UnlockArgs {
    /// Hex-encoded master key, OR a `KEY=<hex>` blob.  When non-empty, this
    /// takes precedence over the passphrase path.
    #[serde(default)]
    pub master_key_hex: String,
}

/// Unlock with a directly-supplied master key (Mode A).  Strips an optional
/// `KEY=` prefix so the user can paste the same `KEY=<hex>` file format the
/// main app produces from `nyx_bkp_cli export-key`.
#[tauri::command]
pub async fn rec_unlock(
    args: UnlockArgs,
    session: State<'_, SharedSession>,
) -> Result<Value, String> {
    let raw = args.master_key_hex.trim();
    let hex = raw.strip_prefix("KEY=").unwrap_or(raw).trim();
    let bytes = hex_decode(hex)
        .map_err(|e| user_error(Ctx::Unlock, &format!("invalid master key: {e}")))?;
    if bytes.len() != 32 {
        return Err(user_error(
            Ctx::Unlock,
            &format!(
                "invalid master key: expected 64 hex chars, got {}",
                bytes.len() * 2
            ),
        ));
    }
    let mut k = [0u8; 32];
    k.copy_from_slice(&bytes);

    {
        let mut s = session.write().await;
        if s.endpoint.is_none() {
            return Err(user_error(Ctx::Generic, "not connected"));
        }
        s.master_key = Some(MasterKey::from_bytes(k));
        s.phase = Phase::Browse;
    }
    Ok(json!({ "phase": "browse" }))
}

/// Discover all backup sets reachable from the current endpoint, then load
/// each set's snapshot index, decrypting with the in-session master key.
/// Sets whose snapshot-index cannot be decrypted are silently skipped - they
/// belong to a different master key.
#[tauri::command]
pub async fn rec_list_snapshots(
    session: State<'_, SharedSession>,
) -> Result<Vec<SnapshotSummary>, String> {
    let (backend, master_key) = {
        let s = session.read().await;
        let b = s
            .backend
            .clone()
            .ok_or_else(|| user_error(Ctx::Generic, "not connected"))?;
        let m = s
            .master_key
            .as_ref()
            .map(|k| MasterKey::from_bytes(*k.as_bytes()))
            .ok_or_else(|| user_error(Ctx::Generic, "not unlocked"))?;
        (b, m)
    };

    // Enumerate set IDs from the `indexes/<set_id>/snapshot-index` namespace.
    tracing::info!("rec_list_snapshots: listing indexes/");
    let raw_paths = backend.list("indexes/").await.map_err(|e| {
        tracing::warn!(error = %e, "rec_list_snapshots: list indexes/ failed");
        ue(Ctx::ListSnapshots, &e)
    })?;
    tracing::info!(
        count = raw_paths.len(),
        "rec_list_snapshots: indexes/ listed"
    );

    let mut set_ids: Vec<BackupSetId> = Vec::new();
    for p in &raw_paths {
        // Path shape: `indexes/<uuid>/snapshot-index`
        let after = match p.strip_prefix("indexes/") {
            Some(s) => s,
            None => continue,
        };
        let Some(slash) = after.find('/') else {
            continue;
        };
        let uuid_str = &after[..slash];
        if let Ok(uuid) = Uuid::parse_str(uuid_str) {
            let id = BackupSetId::from_uuid(uuid);
            if !set_ids.contains(&id) {
                set_ids.push(id);
            }
        }
    }
    tracing::info!(
        set_count = set_ids.len(),
        "rec_list_snapshots: discovered set IDs"
    );

    let mut snapshots: Vec<SnapshotSummary> = Vec::new();
    let mut decode_failures = 0u32;
    for set_id in &set_ids {
        let key = match bkp_crypto::subkey::derive_subkey(
            &master_key,
            bkp_crypto::keys::KeyLabel::SnapshotIndex,
            set_id,
        ) {
            Ok(k) => k,
            Err(e) => {
                tracing::warn!(set_id = %set_id.as_uuid(), error = %e, "derive_subkey failed");
                continue;
            }
        };
        let path = snapshot_index_remote_path(set_id);
        let data = match backend.get_critical(&path).await {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(set_id = %set_id.as_uuid(), error = %e, "fetch snapshot-index failed");
                continue;
            }
        };
        let index = match decode_snapshot_index(&data, &key) {
            Ok(i) => i,
            Err(e) => {
                // Distinguish "envelope can't even be parsed" (legacy
                // cipher_id != 0, magic mismatch, body length wrong)
                // from "envelope parses but auth tag fails" (wrong
                // master key).  The former needs a different fix
                // (rebuild remote snapshot-index by running a backup
                // on the source machine); the latter is a key
                // problem.  The SAME master key can appear to work
                // elsewhere while the snapshot-index on B2 is still in
                // cipher_id=2 legacy format: a local DB cache can hide
                // it, but recovery has no cache and reads the encrypted
                // object directly.
                let msg = format!("{e}");
                let legacy = msg.contains("cipher_id");
                tracing::warn!(
                    set_id = %set_id.as_uuid(),
                    error = %e,
                    legacy_cipher = legacy,
                    "decode snapshot-index failed"
                );
                decode_failures += 1;
                continue;
            }
        };
        tracing::info!(set_id = %set_id.as_uuid(), entries = index.entries.len(),
            "decoded snapshot-index");

        // Pull set_name + hostname from the LATEST manifest in this set
        // (one extra storage round-trip per set; manifests are ~tens of
        // KiB).  Older snapshots have an empty
        // `set_name`; the GUI falls back to hostname, then "Set N".
        let mut set_name_label = String::new();
        let mut hostname_label = String::new();
        if let Some(latest) = index.entries.last() {
            let manifest_subkey = match bkp_crypto::subkey::derive_subkey(
                &master_key,
                bkp_crypto::keys::KeyLabel::ManifestEncryption,
                set_id,
            ) {
                Ok(k) => Some(k),
                Err(e) => {
                    tracing::warn!(set_id = %set_id.as_uuid(), error = %e,
                        "derive manifest subkey for label failed");
                    None
                }
            };
            if let Some(mk) = manifest_subkey {
                let mpath = manifest_remote_path(set_id, &latest.snapshot_id);
                match backend.get_critical(&mpath).await {
                    Ok(bytes) => match decode_manifest(&bytes, &mk) {
                        Ok(m) => {
                            set_name_label = m.set_name;
                            hostname_label = m.hostname;
                        }
                        Err(e) => tracing::warn!(set_id = %set_id.as_uuid(),
                            error = %e, "decode manifest for label failed"),
                    },
                    Err(e) => tracing::warn!(set_id = %set_id.as_uuid(),
                        error = %e, "fetch manifest for label failed"),
                }
            }
        }

        for entry in &index.entries {
            snapshots.push(SnapshotSummary {
                snapshot_id: entry.snapshot_id.as_uuid().to_string(),
                set_id: set_id.as_uuid().to_string(),
                // SnapshotEntry.created_at is doc'd as "Unix nanoseconds"
                // but the engine (bkp-engine/src/lib.rs:1438) actually
                // stores `created_at_ns / 1_000_000_000` - i.e. SECONDS.
                // The daemon's local-DB path stores nanoseconds and
                // converts there, which is why the main app shows
                // correct dates.  The cross-machine + recovery paths
                // read the snapshot-index directly and previously
                // double-divided to 0 (-> 1970-01-01).  Pass through
                // as-is.
                created_at: entry.created_at,
                files_total: entry.files_total,
                bytes_total: entry.bytes_total,
                set_name: set_name_label.clone(),
                hostname: hostname_label.clone(),
            });
        }
    }
    tracing::info!(
        snapshots = snapshots.len(),
        decode_failures = decode_failures,
        "rec_list_snapshots: done",
    );

    // If we found backup sets but couldn't decrypt ANY of them, the master
    // key bytes don't match the ones used to encrypt these snapshot
    // indices.  Common causes (most likely first):
    //
    // 1. The pasted key is from a DIFFERENT machine than the one that ran
    //    the backups.  Each machine has its own master key generated at
    //    first-run.
    // 2. The pasted key has a transcription error (extra/missing chars,
    //    swapped digits).
    // 3. The pasted key is encoded differently (e.g. base64 instead of
    //    hex - we accept 64 hex chars, optionally prefixed with `KEY=`).
    //
    // The error message addresses both the same-machine and
    // different-machine scenarios so the user can tell which to check.
    if snapshots.is_empty() && decode_failures > 0 {
        return Err(format!(
            "Found {decode_failures} backup set(s) at this endpoint, but \
             could not decode any of them.  Possible causes:\n\n\
             1. The remote snapshot-index is encrypted with a legacy \
             cipher (earlier builds used AES-256-GCM-SIV; current \
             builds use AES-256-GCM only).  Fix: on the source machine, \
             run ONE more backup against the set - the engine rewrites \
             the snapshot-index in the new format on every successful \
             run.  Then retry recovery.\n\n\
             2. The master key is for a DIFFERENT machine than the one \
             that wrote these backups (each machine has its own master \
             key generated at first-run).  Fix: on the SOURCE machine \
             run `nyx_bkp_cli.exe export-key --output <file>` and paste \
             that file's contents (or use 'Load from file...').\n\n\
             3. The pasted key has a typo / extra whitespace.  Expected \
             64 hex characters, optionally prefixed with `KEY=`.\n\n\
             The recovery.log file at \
             %LOCALAPPDATA%\\NyxBackup\\Recover\\logs\\ has per-set \
             detail (`legacy_cipher=true` means cause #1; auth-tag \
             error means cause #2 or #3)."
        ));
    }

    // Newest first.
    snapshots.sort_by_key(|s| std::cmp::Reverse(s.created_at));

    // Cache for downstream lookups.
    {
        let mut s = session.write().await;
        s.snapshots = snapshots.clone();
    }

    Ok(snapshots)
}

#[derive(Serialize)]
pub struct SnapshotFileEntryDto {
    pub path: String,
    pub size: u64,
    pub mtime_ns: u64,
    pub is_dir: bool,
    pub is_symlink: bool,
}

#[derive(Deserialize)]
pub struct ListFilesArgs {
    pub set_id: String,
    pub snapshot_id: String,
}

/// Load the file tree for a chosen snapshot.  Uses `bkp_restore::RestoreEngine`
/// directly (no IPC) and returns a flat path list sorted lexicographically.
#[tauri::command]
pub async fn rec_list_snapshot_files(
    args: ListFilesArgs,
    session: State<'_, SharedSession>,
) -> Result<Vec<SnapshotFileEntryDto>, String> {
    let (backend, master_key) = {
        let s = session.read().await;
        let b = s
            .backend
            .clone()
            .ok_or_else(|| user_error(Ctx::Generic, "not connected"))?;
        let m = s
            .master_key
            .as_ref()
            .map(|k| MasterKey::from_bytes(*k.as_bytes()))
            .ok_or_else(|| user_error(Ctx::Generic, "not unlocked"))?;
        (b, m)
    };
    let set_id = BackupSetId::from_uuid(
        Uuid::parse_str(&args.set_id)
            .map_err(|e| user_error(Ctx::Generic, &format!("set_id: {e}")))?,
    );
    let snapshot_id = bkp_types::snapshot::SnapshotId::from_uuid(
        Uuid::parse_str(&args.snapshot_id)
            .map_err(|e| user_error(Ctx::Generic, &format!("snapshot_id: {e}")))?,
    );
    let manifest_key = bkp_crypto::subkey::derive_subkey(
        &master_key,
        bkp_crypto::keys::KeyLabel::ManifestEncryption,
        &set_id,
    )
    .map_err(|e| user_error(Ctx::Generic, &format!("decrypt: derive manifest key: {e}")))?;

    let engine = bkp_restore::RestoreEngine::new(backend);
    let entries = engine
        .list_snapshot_files(&snapshot_id, &set_id, &manifest_key)
        .await
        .map_err(|e| ue(Ctx::ListFiles, &e))?;

    Ok(entries
        .into_iter()
        .map(|e| SnapshotFileEntryDto {
            path: e.path,
            size: e.size,
            mtime_ns: e.mtime_ns,
            is_dir: e.is_dir,
            is_symlink: e.is_symlink,
        })
        .collect())
}

#[derive(Deserialize)]
pub struct StartRestoreArgs {
    pub set_id: String,
    pub snapshot_id: String,
    pub dest_path: String,
    #[serde(default)]
    pub filter_paths: Vec<String>,
    #[serde(default)]
    pub excluded_paths: Vec<String>,
}

/// Spawn the restore on a background tokio task.  Writes progress to a
/// poll-from-memory view in session state; the GUI polls
/// `rec_get_progress` every 500 ms.  Also writes a checkpoint file so a
/// crash or app restart can resume the operation.
#[tauri::command]
pub async fn rec_start_restore(
    args: StartRestoreArgs,
    session: State<'_, SharedSession>,
) -> Result<(), String> {
    let (backend, master_key, endpoint) = {
        let s = session.read().await;
        let b = s
            .backend
            .clone()
            .ok_or_else(|| user_error(Ctx::Generic, "not connected"))?;
        let m = s
            .master_key
            .as_ref()
            .map(|k| MasterKey::from_bytes(*k.as_bytes()))
            .ok_or_else(|| user_error(Ctx::Generic, "not unlocked"))?;
        let ep = s
            .endpoint
            .clone()
            .ok_or_else(|| user_error(Ctx::Restore, "not connected"))?;
        (b, m, ep)
    };

    let set_id = BackupSetId::from_uuid(
        Uuid::parse_str(&args.set_id)
            .map_err(|e| user_error(Ctx::Generic, &format!("set_id: {e}")))?,
    );
    let snapshot_id = bkp_types::snapshot::SnapshotId::from_uuid(
        Uuid::parse_str(&args.snapshot_id)
            .map_err(|e| user_error(Ctx::Generic, &format!("snapshot_id: {e}")))?,
    );
    let dest = PathBuf::from(&args.dest_path);
    if !dest.exists() {
        std::fs::create_dir_all(&dest).map_err(|e| {
            user_error(
                Ctx::Restore,
                &format!("create dest dir {}: {e}", dest.display()),
            )
        })?;
    }

    // Per-set subkeys.
    let chunk_key = bkp_crypto::subkey::derive_subkey(
        &master_key,
        bkp_crypto::keys::KeyLabel::ChunkEncryption,
        &set_id,
    )
    .map_err(|e| user_error(Ctx::Restore, &format!("decrypt: derive chunk key: {e}")))?;
    let chunk_id_key = bkp_crypto::subkey::derive_subkey(
        &master_key,
        bkp_crypto::keys::KeyLabel::ChunkIdentity,
        &set_id,
    )
    .map_err(|e| user_error(Ctx::Restore, &format!("decrypt: derive chunk-id key: {e}")))?;
    let manifest_key = bkp_crypto::subkey::derive_subkey(
        &master_key,
        bkp_crypto::keys::KeyLabel::ManifestEncryption,
        &set_id,
    )
    .map_err(|e| user_error(Ctx::Generic, &format!("decrypt: derive manifest key: {e}")))?;

    // Apply per-session bandwidth throttling.
    let settings = Settings::load();
    // Captured by copy into the restore spawn below (settings is not moved in).
    let restore_sparse = settings.restore_sparse;
    let throttled: Arc<dyn StorageBackend> = if settings.download_bandwidth_kbps > 0 {
        Arc::new(RateLimitedBackend::new(
            backend,
            0,
            settings.download_bandwidth_kbps as u64,
        ))
    } else {
        backend
    };

    // Initial progress state.
    let session_id = Uuid::new_v4().to_string();
    let started_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // Pause / cancel watch channels.  Replacing any prior senders means
    // a previously-paused (but never resumed) session can't deadlock a
    // fresh restore - the old engine is gone and its receivers were
    // dropped with it.
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let (pause_tx, pause_rx) = tokio::sync::watch::channel(false);

    {
        let mut s = session.write().await;
        s.progress = Some(RestoreProgressView {
            status: "running".into(),
            files_done: 0,
            files_total: 0,
            bytes_done: 0,
            bytes_total: 0,
            current_file: String::new(),
            error_detail: String::new(),
            paused: false,
        });
        s.cancel_tx = Some(cancel_tx);
        s.pause_tx = Some(pause_tx);
        s.paused = false;
    }

    // Write the initial checkpoint - resume on next start picks this up.
    let checkpoint = Checkpoint {
        session_id: session_id.clone(),
        snapshot_id: args.snapshot_id.clone(),
        set_id: args.set_id.clone(),
        endpoint: CheckpointEndpointConfig {
            endpoint_type: endpoint.endpoint_type.clone(),
            url: endpoint.url.clone(),
            key_id: endpoint.key_id.clone(),
            label: String::new(),
        },
        destination: dest.clone(),
        filter_paths: args.filter_paths.clone(),
        excluded_paths: args.excluded_paths.clone(),
        completed_files: Vec::new(),
        bytes_total: 0,
        bytes_done: 0,
        started_at,
        last_updated: started_at,
    };
    let _ = checkpoint.save();

    // Spawn the restore task.  The Tauri command returns immediately;
    // progress is observable via rec_get_progress.
    let session_arc: SharedSession = (*session).clone();
    let filter_paths = args.filter_paths.clone();
    let excluded_paths = args.excluded_paths.clone();
    tokio::spawn(async move {
        let (tx, mut rx) = mpsc::channel::<bkp_restore::RestoreFileProgress>(64);
        let target = RestoreTarget::Custom(dest.clone());

        // Build the chunk -> (pack_id, offset, size) map by walking every
        // pack on remote storage.  Without this the engine 404s on every
        // chunk because chunks live inside pack files, not at
        // chunks/<hash>.  Source-machine restores get this from the
        // local SQLite chunks DB; recovery has no DB so we build it on
        // demand by reading every pack's footer + index CBOR.
        tracing::info!("rec_start_restore: building remote pack map (one-time scan)");
        let pack_map = match bkp_restore::build_pack_map_from_storage(&*throttled).await {
            Ok(m) => {
                tracing::info!(packs = m.len(), "rec_start_restore: pack map built");
                m
            }
            Err(e) => {
                tracing::warn!(error = %e, "rec_start_restore: build_pack_map_from_storage failed");
                let mut s = session_arc.write().await;
                if let Some(ref mut v) = s.progress {
                    v.status = "error".into();
                    v.error_detail = user_error(Ctx::Restore, &e.to_string());
                }
                return;
            }
        };

        let mut engine = RestoreEngine::new_with_pack_cache(throttled, pack_map);
        engine.set_cancel_pause(cancel_rx, pause_rx);
        engine.set_sparse(restore_sparse);

        // Progress receiver: updates session state as files complete.
        let progress_session = session_arc.clone();
        let progress_session_id = session_id.clone();
        let progress_handle = tokio::spawn(async move {
            let mut completed_files: Vec<String> = Vec::new();
            while let Some(p) = rx.recv().await {
                completed_files.push(p.current_file.clone());
                let mut s = progress_session.write().await;
                if let Some(ref mut v) = s.progress {
                    v.files_done = p.files_done;
                    v.files_total = p.files_total;
                    v.bytes_done = p.bytes_done;
                    v.bytes_total = p.bytes_total;
                    v.current_file = p.current_file.clone();
                }
                // Periodic checkpoint update so a crash mid-restore
                // leaves a resume-able state.
                if completed_files.len().is_multiple_of(16)
                    && let Ok(text) = std::fs::read_to_string(
                        crate::paths::checkpoint_dir().join(format!("{progress_session_id}.json")),
                    )
                    && let Ok(mut cp) = serde_json::from_str::<Checkpoint>(&text)
                {
                    cp.completed_files = completed_files.clone();
                    cp.bytes_done = p.bytes_done;
                    cp.bytes_total = p.bytes_total;
                    cp.last_updated = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    let _ = cp.save();
                }
            }
        });

        let result = engine
            .restore_all(
                &snapshot_id,
                &set_id,
                target,
                OverwriteMode::Skip,
                &chunk_key,
                &chunk_id_key,
                &manifest_key,
                &filter_paths,
                &excluded_paths,
                Some(tx),
                RestoreOwner {
                    owner_sid: String::new(),
                    unix_uid: 0,
                    unix_gid: 0,
                },
            )
            .await;
        drop(progress_handle.await);

        // Finalise.  Cancelled is a clean terminal state (user pressed
        // Cancel), distinct from a hard error; the checkpoint is kept
        // so the same session can be resumed from the Connect screen.
        let mut s = session_arc.write().await;
        // Drop watch senders so a follow-up restore starts with fresh
        // channels (otherwise a stale `pause=true` could carry into
        // the next session).
        s.cancel_tx = None;
        s.pause_tx = None;
        s.paused = false;
        match result {
            Ok(()) => {
                if let Some(ref mut v) = s.progress {
                    v.status = "complete".into();
                    v.paused = false;
                }
                let _ = Checkpoint::discard(&session_id);
            }
            Err(e) => {
                let msg = format!("{e}");
                let cancelled = msg.contains("cancel") || msg.contains("Cancel");
                if let Some(ref mut v) = s.progress {
                    v.status = if cancelled {
                        "cancelled".into()
                    } else {
                        "error".into()
                    };
                    v.error_detail = if cancelled {
                        String::new()
                    } else {
                        user_error(Ctx::Restore, &msg)
                    };
                    v.paused = false;
                }
            }
        }
    });

    Ok(())
}

/// Poll the live progress of the spawned restore task.  Returns None when no
/// restore is active.
#[tauri::command]
pub async fn rec_get_progress(
    session: State<'_, SharedSession>,
) -> Result<Option<RestoreProgressView>, String> {
    let s = session.read().await;
    Ok(s.progress.clone())
}

/// Return any interrupted-restore checkpoints found in the checkpoint dir.
/// Surfaced on the Connect screen so the user can resume a restore that was
/// interrupted by an app restart or system crash.
#[tauri::command]
pub async fn rec_list_checkpoints() -> Result<Vec<Checkpoint>, String> {
    Ok(Checkpoint::list_all())
}

/// Delete a checkpoint without resuming - the user chose to discard.
#[tauri::command]
pub async fn rec_discard_checkpoint(session_id: String) -> Result<(), String> {
    Checkpoint::discard(&session_id).map_err(|e| format!("discard: {e}"))
}

#[tauri::command]
pub async fn rec_get_recent() -> Result<Vec<RecentEndpoint>, String> {
    Ok(RecentList::load().items)
}

/// Remove a recently-used endpoint, identified by `(endpoint_type, url,
/// key_id)`.  Returns the updated list so the UI can refresh in place.
#[tauri::command]
pub async fn rec_remove_recent(
    endpoint_type: String,
    url: String,
    key_id: String,
) -> Result<Vec<RecentEndpoint>, String> {
    let mut recent = RecentList::load();
    recent.remove(&endpoint_type, &url, &key_id);
    recent
        .save()
        .map_err(|e| format!("save recent list: {e}"))?;
    Ok(recent.items)
}

#[tauri::command]
pub async fn rec_get_settings() -> Result<Settings, String> {
    Ok(Settings::load())
}

#[tauri::command]
pub async fn rec_save_settings(settings: Settings) -> Result<(), String> {
    settings.save().map_err(|e| format!("save settings: {e}"))
}

/// Read a `KEY=<hex>` file the user picks via the OS file picker.
/// Tauri's fs plugin isn't enabled (smaller capability surface); this
/// command provides a tightly-scoped read of one short text file.
#[tauri::command]
pub async fn rec_read_key_file(path: String) -> Result<String, String> {
    let body = std::fs::read_to_string(&path).map_err(|e| ue(Ctx::ReadKeyFile, &e))?;
    // Cap at 1 KiB - master-key files are 64-68 chars; anything larger is
    // not what the user thinks it is.
    if body.len() > 1024 {
        return Err("file too large to be a key (over 1 KiB)".into());
    }
    Ok(body)
}

/// Minimal hex decoder.  Avoids pulling in the `hex` crate for 64 chars.
fn hex_decode(s: &str) -> Result<Vec<u8>, String> {
    let s = s.trim();
    if !s.len().is_multiple_of(2) {
        return Err("odd number of hex chars".into());
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    for i in (0..bytes.len()).step_by(2) {
        let hi = nibble(bytes[i]).ok_or_else(|| format!("non-hex char at position {i}"))?;
        let lo =
            nibble(bytes[i + 1]).ok_or_else(|| format!("non-hex char at position {}", i + 1))?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}
fn nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

// - OAuth one-click flows (Dropbox + Google Drive) -----------------------------
//
// Lifted from bkp-gui's Tauri command shape so the Recovery Tool has the
// same "Connect with Dropbox / Google" UX as the main app's BackupSetEditor.
// The shared OAuth logic lives in bkp_oauth::dropbox / bkp_oauth::google;
// dropbox_oauth.rs / google_oauth.rs in this crate are thin Tauri-side
// adapters identical to bkp-gui's copies (same env!() client-id pulls).

#[tauri::command]
pub async fn rec_dropbox_oauth(app: tauri::AppHandle) -> Result<Value, String> {
    match crate::dropbox_oauth::run_oauth_flow(app).await {
        Ok(r) => Ok(json!({ "refresh_token": r.refresh_token, "email": r.email })),
        Err(e) => Err(format!("Dropbox OAuth failed: {e}")),
    }
}

#[tauri::command]
pub async fn rec_google_oauth(folder_url: String, app: tauri::AppHandle) -> Result<Value, String> {
    match crate::google_oauth::run_oauth_flow(folder_url, app).await {
        Ok(r) => Ok(json!({
            "folder_id":     r.folder_id,
            "refresh_token": r.refresh_token,
            "email":         r.email,
        })),
        Err(e) => Err(format!("Google Drive OAuth failed: {e}")),
    }
}

// --- Manual (no-local-browser) OAuth relay -------------------------------
//
// For headless rescue machines with no usable browser.  Step 1 returns a
// sign-in URL the user opens on ANY browser; after authorizing, the provider
// redirects to http://localhost:<port>?code=... which fails to load there, but
// the code is in the address bar.  The user pastes that URL (or the bare code)
// back; step 2 exchanges it.  `redirect_uri` is round-tripped from step 1 so
// the exchange matches the value the code was issued for.

/// Pull the `code` out of a pasted value: the full redirect URL
/// (`http://localhost:PORT/?code=XXX&...`) or the bare code.
fn extract_oauth_code(pasted: &str) -> String {
    let s = pasted.trim();
    match s.find("code=") {
        Some(i) => s[i + "code=".len()..]
            .split(['&', '#', ' '])
            .next()
            .unwrap_or("")
            .trim()
            .to_string(),
        None => s.to_string(),
    }
}

#[tauri::command]
pub async fn rec_dropbox_oauth_url() -> Result<Value, String> {
    let (auth_url, redirect_uri) =
        crate::dropbox_oauth::manual_auth_url().map_err(|e| format!("Dropbox sign-in URL: {e}"))?;
    Ok(json!({ "auth_url": auth_url, "redirect_uri": redirect_uri }))
}

#[tauri::command]
pub async fn rec_dropbox_oauth_exchange(
    pasted: String,
    redirect_uri: String,
) -> Result<Value, String> {
    let code = extract_oauth_code(&pasted);
    if code.is_empty() {
        return Err("No authorization code found in the pasted value.".into());
    }
    match crate::dropbox_oauth::exchange_code(code, redirect_uri).await {
        // `secret` is the value that goes into the storage-secret field
        // (Dropbox uses the bare refresh token).
        Ok(r) => Ok(json!({ "secret": r.refresh_token, "email": r.email })),
        Err(e) => Err(format!("Dropbox OAuth failed: {e}")),
    }
}

#[tauri::command]
pub async fn rec_google_oauth_url() -> Result<Value, String> {
    let (auth_url, redirect_uri) =
        crate::google_oauth::manual_auth_url().map_err(|e| format!("Google sign-in URL: {e}"))?;
    Ok(json!({ "auth_url": auth_url, "redirect_uri": redirect_uri }))
}

#[tauri::command]
pub async fn rec_google_oauth_exchange(
    folder_url: String,
    pasted: String,
    redirect_uri: String,
) -> Result<Value, String> {
    let code = extract_oauth_code(&pasted);
    if code.is_empty() {
        return Err("No authorization code found in the pasted value.".into());
    }
    match crate::google_oauth::exchange_code(folder_url, code, redirect_uri).await {
        // Google uses the bare refresh token as the secret; folder_id replaces
        // the URL field.
        Ok(r) => Ok(json!({
            "secret":    r.refresh_token,
            "folder_id": r.folder_id,
            "email":     r.email,
        })),
        Err(e) => Err(format!("Google Drive OAuth failed: {e}")),
    }
}

// OneDrive's backend reads the secret as a JSON blob
// (parse_oauth_blob_onedrive), NOT a bare token - so emit
// {"refresh_token","tenant_id"} (public client: no client_secret).
fn onedrive_secret_blob(refresh_token: &str, tenant_id: &str) -> String {
    json!({ "refresh_token": refresh_token, "tenant_id": tenant_id }).to_string()
}

/// One-click OneDrive sign-in (loopback).  `tenant_id`: common / consumers /
/// organizations / a tenant GUID.
#[tauri::command]
pub async fn rec_onedrive_oauth(tenant_id: String, app: tauri::AppHandle) -> Result<Value, String> {
    match crate::onedrive_oauth::run_oauth_flow(tenant_id, app).await {
        Ok(r) => Ok(json!({
            "secret": onedrive_secret_blob(&r.refresh_token, &r.tenant_id),
            "email":  r.email,
        })),
        Err(e) => Err(format!("OneDrive OAuth failed: {e}")),
    }
}

#[tauri::command]
pub async fn rec_onedrive_oauth_url(tenant_id: String) -> Result<Value, String> {
    let (auth_url, redirect_uri) = crate::onedrive_oauth::manual_auth_url(tenant_id)
        .map_err(|e| format!("OneDrive sign-in URL: {e}"))?;
    Ok(json!({ "auth_url": auth_url, "redirect_uri": redirect_uri }))
}

#[tauri::command]
pub async fn rec_onedrive_oauth_exchange(
    tenant_id: String,
    pasted: String,
    redirect_uri: String,
) -> Result<Value, String> {
    let code = extract_oauth_code(&pasted);
    if code.is_empty() {
        return Err("No authorization code found in the pasted value.".into());
    }
    match crate::onedrive_oauth::exchange_code(tenant_id, code, redirect_uri).await {
        Ok(r) => Ok(json!({
            "secret": onedrive_secret_blob(&r.refresh_token, &r.tenant_id),
            "email":  r.email,
        })),
        Err(e) => Err(format!("OneDrive OAuth failed: {e}")),
    }
}

#[derive(Serialize)]
pub struct AppInfo {
    pub name: &'static str,
    pub version: &'static str,
    pub target: &'static str,
}

/// About-screen feed.  Static.
#[tauri::command]
pub fn rec_app_info() -> AppInfo {
    AppInfo {
        name: "Nyx Backup Recovery",
        version: env!("CARGO_PKG_VERSION"),
        target: std::env::consts::OS,
    }
}

// Suppress an unused-import warning for Arc when only used transitively.
#[allow(dead_code)]
fn _arc_anchor(_: Arc<()>) {}

// - Restore control (Pause / Resume / Cancel) ---------------------------------
//
// Mirrors the main app's three-button restore controls so the user has
// the same affordances inside the Recovery Tool.
// Each command flips the corresponding watch::Sender; the engine reacts
// at the next file boundary.

#[tauri::command]
pub async fn rec_pause_restore(session: State<'_, SharedSession>) -> Result<(), String> {
    let mut s = session.write().await;
    if let Some(tx) = s.pause_tx.as_ref() {
        let _ = tx.send(true);
    }
    s.paused = true;
    if let Some(ref mut v) = s.progress {
        v.paused = true;
    }
    Ok(())
}

#[tauri::command]
pub async fn rec_resume_restore(session: State<'_, SharedSession>) -> Result<(), String> {
    let mut s = session.write().await;
    if let Some(tx) = s.pause_tx.as_ref() {
        let _ = tx.send(false);
    }
    s.paused = false;
    if let Some(ref mut v) = s.progress {
        v.paused = false;
    }
    Ok(())
}

#[tauri::command]
pub async fn rec_cancel_restore(session: State<'_, SharedSession>) -> Result<(), String> {
    let s = session.read().await;
    if let Some(tx) = s.cancel_tx.as_ref() {
        let _ = tx.send(true);
    }
    // Also lift any active pause so the engine actually reaches its
    // next cancel check instead of sleeping in the pause loop.
    if let Some(tx) = s.pause_tx.as_ref() {
        let _ = tx.send(false);
    }
    Ok(())
}

// - Filesystem helpers --------------------------------------------------------

/// Open a folder in the OS's default file manager.  Bypasses Tauri's
/// shell-plugin scope (which would force every conceivable restore
/// destination into a regex allowlist baked into the capabilities
/// file - impossible for arbitrary user-picked paths).  Matches
/// bkp-gui's `open_folder` command.
#[tauri::command]
pub async fn rec_open_folder(path: String) -> Result<(), String> {
    use std::process::Command;
    if path.trim().is_empty() {
        return Err("empty path".into());
    }
    #[cfg(target_os = "windows")]
    {
        let path = path.replace('/', "\\");
        Command::new("explorer.exe")
            .arg(&path)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("open folder {path}: {e}"))
    }
    #[cfg(target_os = "macos")]
    {
        Command::new("open")
            .arg(&path)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("open folder {path}: {e}"))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        for cmd in [
            "xdg-open", "nautilus", "dolphin", "nemo", "thunar", "pcmanfm", "caja",
        ] {
            if Command::new(cmd).arg(&path).spawn().is_ok() {
                return Ok(());
            }
        }
        Err(format!("no file manager found to open {path}"))
    }
}

/// Resolve the platform-default restore base directory.  Mirrors
/// bkp-gui's `get_local_desktop` command so the Recovery Tool radio
/// lands on the same default as the main app (Windows:
/// `%SystemDrive%\NyxRestore`; macOS / Linux: `$HOME/Desktop`,
/// falling back to `$HOME`).
#[tauri::command]
pub async fn rec_local_desktop() -> String {
    #[cfg(windows)]
    {
        let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_string());
        format!("{drive}\\NyxRestore")
    }
    #[cfg(not(windows))]
    {
        let home = dirs_next::home_dir();
        let candidates: [Option<std::path::PathBuf>; 2] =
            [home.as_ref().map(|h| h.join("Desktop")), home.clone()];
        candidates
            .into_iter()
            .flatten()
            .find(|p| p.exists())
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

#[derive(Serialize)]
pub struct DestFreeSpace {
    pub free_bytes: u64,
    pub total_bytes: u64,
    pub determinable: bool,
}

/// Free + total bytes on the volume holding `path` (or its nearest
/// existing ancestor when `path` doesn't exist yet).  Mirrors the main
/// daemon's `GetDestinationFreeSpace` RPC so the Recovery Tool right
/// pane can show "Selected: X · Free at dest: Y of Z" just like the
/// main app.  Best-effort; on any failure we return `determinable=false`
/// rather than a wrong number.
#[tauri::command]
pub async fn rec_get_free_space(path: String) -> DestFreeSpace {
    use std::path::Path;
    let mut p = std::path::PathBuf::from(&path);
    while !p.exists() {
        match p.parent() {
            Some(parent) if parent != Path::new("") => p = parent.to_path_buf(),
            _ => {
                return DestFreeSpace {
                    free_bytes: 0,
                    total_bytes: 0,
                    determinable: false,
                };
            }
        }
    }
    match fs2_disk_info(&p) {
        Some((free, total)) => DestFreeSpace {
            free_bytes: free,
            total_bytes: total,
            determinable: true,
        },
        None => DestFreeSpace {
            free_bytes: 0,
            total_bytes: 0,
            determinable: false,
        },
    }
}

/// Cross-platform disk-space probe.  Uses platform-specific syscalls so
/// we don't pull in another crate just for this; if any call fails we
/// return None and the GUI hides the readout.
fn fs2_disk_info(path: &std::path::Path) -> Option<(u64, u64)> {
    #[cfg(windows)]
    {
        use std::iter::once;
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(once(0)).collect();
        let mut free_caller: u64 = 0;
        let mut total: u64 = 0;
        let mut free_total: u64 = 0;
        let ok = unsafe {
            windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
                wide.as_ptr(),
                &mut free_caller,
                &mut total,
                &mut free_total,
            )
        };
        if ok == 0 {
            None
        } else {
            Some((free_caller, total))
        }
    }
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let cpath = CString::new(path.as_os_str().as_bytes()).ok()?;
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::statvfs(cpath.as_ptr(), &mut st) };
        if rc != 0 {
            return None;
        }
        let bsize = st.f_frsize as u64;
        let free = st.f_bavail as u64 * bsize;
        let total = st.f_blocks as u64 * bsize;
        Some((free, total))
    }
}
