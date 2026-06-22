// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! AWS S3 storage backend (also used by S3CompatBackend).
//!
//! Supports STANDARD, STANDARD_IA, and GLACIER storage classes.
//! When `endpoint_url` is set, path-style addressing is forced - enabling
//! use with Wasabi, Minio, Storj S3, Cloudflare R2, and other S3-compatible
//! providers.  HTTP endpoints (e.g. local Minio) are allowed automatically.
//!
//! Uses `object_store` (pure-Rust TLS via ring) instead of the AWS SDK to
//! avoid `aws-lc-sys`, which requires macOS SDK headers when cross-compiling.

use bkp_types::error::{Error, Result};
use futures::StreamExt;
use object_store::{ObjectStore, ObjectStoreExt, aws::AmazonS3Builder, path::Path};

// Cloudflare R2 tuning.  R2 multipart uploads are far more sensitive to high
// concurrency than AWS S3 - exceeding ~3-4 in-flight parts per upload reliably
// produces stalled connections and "error sending request" timeouts.  Detected by endpoint hostname.
const R2_MULTIPART_PART_SIZE: usize = 16 * 1024 * 1024;
const R2_MAX_INFLIGHT_PARTS: usize = 2;

// AWS S3 in-flight cap.  Without a cap, WriteMultipart fires every part
// of a pack in parallel, so an 8-MiB-part / 256-MiB pack
// would produce ~33 concurrent PUTs.  With `upload_workers=8` driving multiple
// packs in parallel that's 264 simultaneous PUTs, which residential / NAT'd
// uplinks routinely choke on (per-IP NAT table exhaustion, packet-loss
// induced retransmit storms) - the observed symptom was multipart PUTs
// timing out after 50-130 s with "HTTP error: error sending request", the
// outer retry wrapper looping `attempt 1/∞`, and the engine stalling at
// ~95%.  Capping per-pack in-flight to 8 keeps the parallelism that AWS
// rewards while staying inside what residential uplinks tolerate.  Bumped
// well above R2's cap of 2 because AWS handles burstiness much better and
// 2-wide on AWS measurably slows datacenter uploads.
const AWS_MAX_INFLIGHT_PARTS: usize = 8;

fn is_r2_endpoint(endpoint_url: Option<&str>) -> bool {
    endpoint_url
        .map(|u| u.contains("r2.cloudflarestorage.com"))
        .unwrap_or(false)
}
use tracing::instrument;

use crate::backend::StorageBackend;

// - AWS Sig V4 (minimal - RestoreObject only) -----------------

mod sigv4 {
    // route SHA-256 / HMAC-SHA256 through `bkp-crypto` so the
    // FIPS fork swaps the implementation in one place.
    use bkp_crypto::hash::sha256_hex;
    use bkp_crypto::hmac::hmac_sha256_raw;

    pub fn hex_sha256(data: &[u8]) -> String {
        sha256_hex(data)
    }

    fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
        hmac_sha256_raw(key, data).to_vec()
    }

    fn signing_key(secret: &str, date: &str, region: &str, service: &str) -> Vec<u8> {
        let k_date = hmac_sha256(format!("AWS4{secret}").as_bytes(), date.as_bytes());
        let k_region = hmac_sha256(&k_date, region.as_bytes());
        let k_service = hmac_sha256(&k_region, service.as_bytes());
        hmac_sha256(&k_service, b"aws4_request")
    }

    /// Sign a POST /{object_key}?restore request and return Authorization + date headers.
    pub fn sign_restore_object(
        access_key_id: &str,
        secret_access_key: &str,
        region: &str,
        host: &str,
        object_key: &str,
        body: &[u8],
    ) -> (String, String) {
        use chrono::Utc;
        let now = Utc::now();
        let date_str = now.format("%Y%m%d").to_string();
        let datetime_str = now.format("%Y%m%dT%H%M%SZ").to_string();

        let body_hash = hex_sha256(body);
        let canonical_headers = format!(
            "content-type:application/xml\nhost:{host}\nx-amz-content-sha256:{body_hash}\nx-amz-date:{datetime_str}\n"
        );
        let signed_headers = "content-type;host;x-amz-content-sha256;x-amz-date";
        let canonical_request = format!(
            "POST\n/{object_key}\nrestore=\n{canonical_headers}\n{signed_headers}\n{body_hash}"
        );

        let scope = format!("{date_str}/{region}/s3/aws4_request");
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{datetime_str}\n{scope}\n{}",
            hex_sha256(canonical_request.as_bytes())
        );

        let key = signing_key(secret_access_key, &date_str, region, "s3");
        let sig = hex::encode(hmac_sha256(&key, string_to_sign.as_bytes()));

        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={access_key_id}/{scope},SignedHeaders={signed_headers},Signature={sig}"
        );
        (authorization, datetime_str)
    }
}

/// Configuration for the S3 (or S3-compatible) backend.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct S3Config {
    /// S3 bucket name.
    pub bucket: String,
    /// Key prefix to prepend to all object paths (empty string for no prefix).
    #[serde(default)]
    pub prefix: String,
    /// AWS region, e.g. `"us-east-1"`.
    #[serde(default = "default_region")]
    pub region: String,
    /// Storage class string, e.g. `"STANDARD"`, `"STANDARD_IA"`, `"GLACIER"`.
    #[serde(default)]
    pub storage_class: Option<String>,
    /// Custom endpoint URL for S3-compatible providers (Minio, Wasabi, …).
    /// Forces path-style addressing when set.
    #[serde(default)]
    pub endpoint_url: Option<String>,
    /// AWS access key ID.  When absent the default credential chain is used.
    #[serde(default)]
    pub access_key_id: Option<String>,
    /// AWS secret access key.  Must be set if `access_key_id` is set.
    #[serde(default)]
    pub secret_access_key: Option<String>,
    /// Glacier retrieval tier used when the
    /// engine calls `initiate_pack_restore` on an archived object.
    /// Valid values: `"Standard"` (3-5 h, default), `"Bulk"` (5-12 h,
    /// ~half the per-GB fee), `"Expedited"` (1-5 min, ~10x the fee;
    /// **not supported on `DEEP_ARCHIVE`** - falls back to Standard
    /// at the AWS side when the user tries).  Empty / None -> Standard.
    #[serde(default)]
    pub retrieval_tier: Option<String>,
    /// how many days the rehydrated copy stays
    /// in the temporary Standard tier before reverting to Glacier.
    /// Clamped to 1..=30; empty / 0 -> 7.
    #[serde(default)]
    pub restore_lifetime_days: Option<u32>,
}

fn default_region() -> String {
    "us-east-1".into()
}

/// Credentials stored for the RestoreObject signed API call.
struct S3Creds {
    access_key_id: String,
    secret_access_key: String,
}

/// S3 storage backend.
pub struct S3Backend {
    store: Box<dyn ObjectStore>,
    bucket: String,
    prefix: String,
    display: String,
    /// Credentials for Sig V4 signing of the RestoreObject API (None = env creds, skips restore initiation).
    creds: Option<S3Creds>,
    /// AWS region, for constructing the RestoreObject endpoint URL.
    region: String,
    /// Custom endpoint URL (S3-compat); None for standard AWS S3.
    endpoint_url: Option<String>,
    /// True when endpoint resolves to Cloudflare R2 - enables R2-specific
    /// multipart tuning (larger parts, capped in-flight concurrency).
    is_r2: bool,
    /// Glacier retrieval tier the engine asks
    /// for via RestoreObject.  Normalised to `"Standard"` / `"Bulk"` /
    /// `"Expedited"` at construct time; out-of-range / missing values
    /// fold to `"Standard"`.  See `S3Config::retrieval_tier`.
    retrieval_tier: String,
    /// how many days the rehydrated copy stays
    /// in temporary Standard tier.  Clamped to 1..=30 at construct
    /// time; 0 / missing -> 7.
    restore_lifetime_days: u32,
}

impl S3Backend {
    /// Construct a new `S3Backend`.  Synchronous - credential resolution is
    /// deferred to the first network call, matching `object_store` semantics.
    pub fn new(cfg: S3Config) -> Result<Self> {
        // When no inline credentials are configured, check for well-known
        // environment variables before letting object_store fall through to
        // the EC2 IMDSv2 credential chain (http://169.254.169.254).  On
        // non-EC2 machines that endpoint is unreachable and object_store will
        // retry for up to ~180 s before surfacing an error.  Fail fast here
        // instead with a clear, actionable message.
        if cfg.access_key_id.is_none() {
            let has_env_creds = std::env::var("AWS_ACCESS_KEY_ID").is_ok()
                || std::env::var("AWS_PROFILE").is_ok()
                || std::env::var("AWS_ROLE_ARN").is_ok()
                || std::env::var("AWS_WEB_IDENTITY_TOKEN_FILE").is_ok()
                // ECS task-credential endpoint (non-169.254 - reachable on ECS/Fargate).
                || std::env::var("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI").is_ok();
            if !has_env_creds {
                return Err(Error::Storage(
                    "S3 credentials required: enter an Access Key ID and Secret Access Key \
                     (or set AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY environment variables)"
                        .into(),
                ));
            }
        }

        let mut builder = AmazonS3Builder::new()
            .with_bucket_name(&cfg.bucket)
            .with_region(&cfg.region);

        if let Some(raw_url) = &cfg.endpoint_url {
            // Normalise: if the user omitted the scheme, prepend https://.
            let url = if raw_url.starts_with("http://") || raw_url.starts_with("https://") {
                raw_url.clone()
            } else {
                format!("https://{raw_url}")
            };
            builder = builder
                .with_endpoint(&url)
                .with_virtual_hosted_style_request(false);
            // Allow plain-HTTP endpoints (local Minio, CI, etc.)
            if url.starts_with("http://") {
                builder = builder.with_allow_http(true);
            }
        }

        match (
            cfg.access_key_id.as_deref(),
            cfg.secret_access_key.as_deref(),
        ) {
            (Some(key_id), Some(secret)) => {
                tracing::debug!(bucket = %cfg.bucket, "S3 using static credentials");
                builder = builder
                    .with_access_key_id(key_id)
                    .with_secret_access_key(secret);
            }
            (Some(_), None) => {
                return Err(Error::Storage(
                    "S3 credentials incomplete: Access Key ID is set but Secret Access Key is missing".into(),
                ));
            }
            (None, Some(_)) => {
                return Err(Error::Storage(
                    "S3 credentials incomplete: Secret Access Key is set but Access Key ID is missing".into(),
                ));
            }
            (None, None) => {
                // Credential check above already verified env vars are present.
                tracing::info!(bucket = %cfg.bucket, "S3 using environment/instance credentials");
            }
        }

        // storage_class is now applied per-PUT via
        // Attribute::StorageClass on PutOptions / PutMultipartOpts so
        // SigV4 canonicalization covers the x-amz-storage-class header.
        // The previous default-headers approach failed AWS signature
        // validation ("There were headers present in the request which
        // were not signed: x-amz-storage-class") because the header was
        // attached by reqwest AFTER the object_store signer ran.  See
        // the put/put_multipart impls for where storage_class threads in.
        if let Some(ref class) = cfg.storage_class {
            tracing::debug!(bucket = %cfg.bucket, storage_class = %class, "S3 storage class configured");
        }

        let store = builder
            .build()
            .map_err(|e| Error::Storage(format!("S3 build: {e}")))?;

        let display = match &cfg.endpoint_url {
            Some(url) => format!("s3-compat://{}/{}/{}", url, cfg.bucket, cfg.prefix),
            None => format!("s3://{}/{}", cfg.bucket, cfg.prefix),
        };

        let creds = match (cfg.access_key_id, cfg.secret_access_key) {
            (Some(kid), Some(sec)) => Some(S3Creds {
                access_key_id: kid,
                secret_access_key: sec,
            }),
            _ => None,
        };

        let is_r2 = is_r2_endpoint(cfg.endpoint_url.as_deref());
        if is_r2 {
            tracing::debug!(
                bucket = %cfg.bucket,
                part_size = R2_MULTIPART_PART_SIZE,
                max_inflight = R2_MAX_INFLIGHT_PARTS,
                "S3: Cloudflare R2 endpoint detected, applying R2 multipart tuning"
            );
        }

        // clamp + normalise retrieval-tier
        // settings.  Anything outside the three valid Glacier tiers
        // collapses to Standard (matches the earlier hardcoded
        // default); missing / zero lifetime defaults to 7 days; the
        // valid lifetime range is 1..=30 per AWS docs.
        let retrieval_tier = match cfg
            .retrieval_tier
            .as_deref()
            .map(|s| s.trim())
            .unwrap_or("")
        {
            "Expedited" | "expedited" => "Expedited".to_string(),
            "Bulk" | "bulk" => "Bulk".to_string(),
            _ => "Standard".to_string(),
        };
        let restore_lifetime_days = cfg.restore_lifetime_days.unwrap_or(7).clamp(1, 30);

        Ok(Self {
            store: Box::new(store),
            bucket: cfg.bucket,
            prefix: cfg.prefix,
            display,
            creds,
            region: cfg.region,
            endpoint_url: cfg.endpoint_url,
            is_r2,
            retrieval_tier,
            restore_lifetime_days,
        })
    }

    /// Prepend the configured prefix to `path`.
    fn full_key(&self, path: &str) -> Path {
        let p = path.trim_start_matches('/');
        if self.prefix.is_empty() {
            Path::from(p)
        } else {
            let prefix = self.prefix.trim_end_matches('/');
            Path::from(format!("{prefix}/{p}").as_str())
        }
    }

    /// Strip the configured prefix from an object key, returning the logical path.
    fn strip_prefix<'a>(&self, key: &'a str) -> &'a str {
        if self.prefix.is_empty() {
            return key;
        }
        let prefix_slash = format!("{}/", self.prefix.trim_end_matches('/'));
        key.strip_prefix(&prefix_slash).unwrap_or(key)
    }
}

#[async_trait::async_trait]
impl StorageBackend for S3Backend {
    #[instrument(skip(self), fields(bucket = %self.bucket, key = path))]
    async fn get(&self, path: &str) -> Result<Vec<u8>> {
        let key = self.full_key(path);
        let result = self
            .store
            .get(&key)
            .await
            .map_err(|e| Error::Storage(format!("S3 get {key}: {e}")))?;
        let bytes = result
            .bytes()
            .await
            .map_err(|e| Error::Storage(format!("S3 get body {key}: {e}")))?;
        Ok(bytes.to_vec())
    }

    #[instrument(skip(self), fields(bucket = %self.bucket, key = path, from, to))]
    async fn get_range(&self, path: &str, from: u64, to: u64) -> Result<Vec<u8>> {
        let key = self.full_key(path);
        let bytes = self
            .store
            .get_range(&key, from..to)
            .await
            .map_err(|e| Error::Storage(format!("S3 get_range {key}: {e}")))?;
        Ok(bytes.to_vec())
    }

    // See StorageBackend::probe_access: a single cheap authed round trip via
    // exists("").  Both Ok(true)/Ok(false) mean reachable + authenticated;
    // only a real connect/auth error propagates.  No content enumeration.
    async fn probe_access(&self) -> Result<()> {
        self.exists("").await.map(|_| ())
    }

    #[instrument(skip(self), fields(bucket = %self.bucket, key = path))]
    async fn exists(&self, path: &str) -> Result<bool> {
        let key = self.full_key(path);
        match self.store.head(&key).await {
            Ok(_) => Ok(true),
            Err(object_store::Error::NotFound { .. }) => Ok(false),
            Err(e) => Err(Error::Storage(format!("S3 exists {key}: {e}"))),
        }
    }

    #[instrument(skip(self), fields(bucket = %self.bucket, prefix = prefix))]
    async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        let full_prefix = self.full_key(prefix);
        let mut stream = self.store.list(Some(&full_prefix));
        let mut paths = Vec::new();
        while let Some(meta) = stream.next().await {
            let meta = meta.map_err(|e| Error::Storage(format!("S3 list {full_prefix}: {e}")))?;
            paths.push(self.strip_prefix(meta.location.as_ref()).to_string());
        }
        Ok(paths)
    }

    #[instrument(skip(self), fields(bucket = %self.bucket, prefix = prefix))]
    async fn list_with_sizes(&self, prefix: &str) -> Result<Vec<(String, u64)>> {
        let full_prefix = self.full_key(prefix);
        let mut stream = self.store.list(Some(&full_prefix));
        let mut results = Vec::new();
        while let Some(meta) = stream.next().await {
            let meta = meta.map_err(|e| Error::Storage(format!("S3 list {full_prefix}: {e}")))?;
            results.push((
                self.strip_prefix(meta.location.as_ref()).to_string(),
                meta.size,
            ));
        }
        Ok(results)
    }

    #[instrument(skip(self), fields(bucket = %self.bucket, key = path))]
    async fn size(&self, path: &str) -> Result<u64> {
        let key = self.full_key(path);
        let meta = self
            .store
            .head(&key)
            .await
            .map_err(|e| Error::Storage(format!("S3 size {key}: {e}")))?;
        Ok(meta.size as u64)
    }

    async fn head_with_hash(&self, path: &str) -> Result<(u64, String, String)> {
        let key = self.full_key(path);
        let meta = self
            .store
            .head(&key)
            .await
            .map_err(|e| Error::Storage(format!("S3 head_with_hash {key}: {e}")))?;
        // S3 ETag for single-part uploads is the MD5 of the content,
        // hex-quoted (e.g. "9bb58f26192e4ba00f01e2e7b136bbd8").  For
        // multipart uploads it's a derived hash-of-part-MD5s suffixed
        // with "-N" (e.g. "9bb...d8-3").  The audit only needs byte-
        // for-byte equality with what we recorded at upload time, so
        // we keep the raw ETag string and tag the algo accordingly.
        let etag = meta
            .e_tag
            .ok_or_else(|| Error::Storage(format!("S3 head_with_hash {key}: no ETag")))?;
        let etag = etag.trim_matches('"').to_string();
        let algo = if etag.contains('-') {
            "etag-multipart"
        } else {
            "md5"
        };
        Ok((meta.size as u64, etag, algo.into()))
    }

    fn display_name(&self) -> String {
        self.display.clone()
    }

    fn concurrency_hint(&self) -> Option<usize> {
        // Returning Some(N) tells download_pack_resilient to fetch the pack
        // as N-wide 16 MiB ranged GETs instead of one single GET for the
        // whole pack.  Two reasons we want that on AWS S3 too:
        //
        //   1) Resilience.  A single GET of a 256 MiB pack on a residential
        //      connection drops mid-stream with surprising regularity (TLS
        //      reset, NAT eviction, transient AWS edge hiccup).  When the
        //      single-GET path retries it restarts from byte 0 - on a slow
        //      uplink that's minutes of wasted bandwidth per drop.  16 MiB
        //      ranged GETs retry just the failing range.
        //   2) Throughput.  Many small GETs against AWS S3 sustain higher
        //      aggregate throughput than one big GET because each GET gets
        //      its own TCP window ramp.  Effect is small on fiber, large on
        //      anything bandwidth-delay-product limited.
        //
        // Cap chosen at 8 to match the upload-side AWS_MAX_INFLIGHT_PARTS:
        // total in-flight HTTP requests stay symmetric across put/get and
        // bounded for residential reliability.
        //
        // Cloudflare R2 keeps its existing tighter cap of 2 - the same per-
        // request stalling that hits R2 on uploads also hits ranged GETs.
        if self.is_r2 {
            Some(2)
        } else {
            Some(AWS_MAX_INFLIGHT_PARTS)
        }
    }

    async fn probe_pack_accessible(&self, path: &str) -> Result<bool> {
        match self.get_range(path, 0, 1).await {
            Ok(_) => Ok(true),
            Err(Error::Storage(msg)) if msg.contains("InvalidObjectState") => {
                tracing::debug!(pack = path, "S3 pack is archived (InvalidObjectState)");
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }

    async fn initiate_pack_restore(&self, path: &str) -> Result<()> {
        let creds = match &self.creds {
            Some(c) => c,
            None => {
                tracing::warn!(
                    pack = path,
                    "S3 archive restore: no static credentials available; skipping RestoreObject"
                );
                return Ok(());
            }
        };

        let object_key = self.full_key(path);
        let object_key_str = object_key.as_ref();

        // Virtual-hosted for standard S3; path-style for S3-compat.
        let (url, host) = if let Some(ref ep) = self.endpoint_url {
            let ep = ep.trim_end_matches('/');
            (
                format!("{ep}/{}/{object_key_str}?restore", self.bucket),
                ep.trim_start_matches("https://")
                    .trim_start_matches("http://")
                    .to_string(),
            )
        } else {
            let host = format!("{}.s3.{}.amazonaws.com", self.bucket, self.region);
            (
                format!("https://{host}/{object_key_str}?restore"),
                host.clone(),
            )
        };

        // retrieval tier + lifetime come from the
        // backend's config (S3Config::retrieval_tier / restore_lifetime_days)
        // rather than the previous hardcoded "Standard" / 1 day.  Days was
        // 1 in the earlier hardcode but the user is paying per-GB
        // retrieval, not per-day - 7 days (the new default) is more
        // forgiving when the engine's poll loop or the user's network
        // hiccup delays the download.
        let body_str = format!(
            "<RestoreRequest><Days>{}</Days>\
             <GlacierJobParameters><Tier>{}</Tier></GlacierJobParameters>\
             </RestoreRequest>",
            self.restore_lifetime_days, self.retrieval_tier,
        );
        let body = body_str.as_bytes();
        let (authorization, amz_date) = sigv4::sign_restore_object(
            &creds.access_key_id,
            &creds.secret_access_key,
            &self.region,
            &host,
            object_key_str,
            body,
        );
        let body_hash = sigv4::hex_sha256(body);

        let client = reqwest::Client::new();
        let resp = client
            .post(&url)
            .header("Authorization", authorization)
            .header("x-amz-date", amz_date)
            .header("x-amz-content-sha256", body_hash)
            .header("Content-Type", "application/xml")
            .body(body.to_vec())
            .send()
            .await
            .map_err(|e| Error::Storage(format!("S3 RestoreObject {object_key_str}: {e}")))?;

        let status = resp.status().as_u16();
        // 202 = restore accepted, 200 = already in progress (both OK)
        if status == 202 || status == 200 {
            tracing::info!(
                pack = path,
                "S3 archive retrieval initiated (HTTP {status})"
            );
            Ok(())
        } else {
            let body_text = resp.text().await.unwrap_or_default();
            Err(Error::Storage(format!(
                "S3 RestoreObject {object_key_str} failed (HTTP {status}): {body_text}"
            )))
        }
    }
}
