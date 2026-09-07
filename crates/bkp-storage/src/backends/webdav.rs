// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! WebDAV (RFC 4918) storage backend.
//!
//! Targets self-hosted file servers that speak WebDAV: Nextcloud,
//! ownCloud, Seafile, Synology, QNAP, Hetzner Storage Box, Apache
//! `mod_dav`, nginx + dav_ext_module, SabreDAV, mailbox.org, Box, pCloud,
//! IONOS HiDrive, and the long tail of others.  Authentication is HTTP
//! Basic over HTTPS (the universal pattern across these providers); we
//! deliberately ignore the rarely-deployed `Digest` / `Bearer` variants
//! in v1.
//!
//! The trait maps cleanly onto WebDAV verbs:
//! - `put`           -> `PUT` body
//! - `get`           -> `GET`
//! - `get_range`     -> `GET` with `Range: bytes=from-to`
//! - `delete`        -> `DELETE`
//! - `delete_prefix` -> recursive `DELETE` on the collection (most servers honour it; the trait fallback isn't used here because empty collections would otherwise leak)
//! - `list`          -> `PROPFIND` Depth: 1 + XML parse for hrefs
//! - `exists`/`size` -> `HEAD`
//!
//! Path construction: the user-supplied endpoint URL (e.g.
//! `https://nextcloud.example.com/remote.php/dav/files/alice/NyxBackup/`)
//! is taken as the storage root.  Object keys (`packs/abc.pack`,
//! `manifests/<set>/<snap>`) are appended after percent-encoding each
//! segment.  Trailing-slash semantics are normalised on construction so
//! we don't double-slash or miss-slash later.
//!
//! Parent directories are auto-created on `put` (`MKCOL`) to mirror the
//! Local / SMB / SFTP backends - WebDAV does NOT auto-create parents on
//! `PUT` (most servers return 409 Conflict).  We walk path components,
//! `MKCOL` each, and treat both `201 Created` and `405 Method Not
//! Allowed` (already exists) as success.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, HeaderMap, HeaderValue, RANGE};
use reqwest::{Client, Method, StatusCode};
use tracing::{debug, instrument, warn};

use bkp_types::error::{Error, Result};

use crate::backend::StorageBackend;

/// Configuration for the WebDAV backend.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct WebDavConfig {
    /// Base URL pointing at the storage root, e.g.
    /// `https://nextcloud.example.com/remote.php/dav/files/alice/NyxBackup/`.
    /// Both `http://` and `https://` are accepted; HTTPS is strongly
    /// recommended since Basic auth credentials are sent on every request.
    pub endpoint_url: String,
    /// HTTP Basic auth username (optional - some self-hosted servers
    /// allow anonymous writes inside dedicated guest-share roots).
    #[serde(default)]
    pub username: Option<String>,
    /// HTTP Basic auth password.  Stored in the OS keychain at rest;
    /// in memory only for the lifetime of the daemon process.
    #[serde(default)]
    pub password: Option<String>,
    /// Optional local path to a PEM file (client certificate + its private key)
    /// for TLS client-certificate (mutual-TLS) auth, for WebDAV servers that
    /// require it.  A filesystem path, not a secret.  When absent, password /
    /// anonymous auth is used.  May be combined with a password.
    #[serde(default)]
    pub client_cert_path: Option<std::path::PathBuf>,
}

/// WebDAV storage backend.
pub struct WebDavBackend {
    client: Client,
    /// Always-trailing-slash form of the user-supplied endpoint URL.
    /// All object-key appends rely on this invariant.
    base_url: String,
    /// Pre-rendered `Authorization: Basic ...` value (empty when no
    /// credentials configured).  Cheaper than re-encoding on every
    /// request and keeps the password out of every per-call closure.
    auth_header: String,
    /// Pretty-printed form for logs and error messages; never includes
    /// credentials.
    display: String,
}

impl WebDavBackend {
    /// Construct a new `WebDavBackend`.
    ///
    /// Validates the endpoint URL shape and pre-computes the `Authorization`
    /// header.  Does NOT perform a network round-trip - the first real
    /// operation (`put`, `list`, etc.) will surface any auth or
    /// connectivity issues.
    pub fn new(cfg: WebDavConfig) -> Result<Self> {
        let base_url = normalise_base_url(&cfg.endpoint_url)?;

        // Pre-render `Authorization: Basic <b64>` once at construction.
        // Empty when no creds are configured (some self-hosted setups
        // serve guest-share-mode WebDAV anonymously).
        let auth_header = match (&cfg.username, &cfg.password) {
            (Some(u), Some(p)) if !u.is_empty() => {
                use base64::Engine as _;
                let pair = format!("{u}:{}", p.as_str());
                let encoded = base64::engine::general_purpose::STANDARD.encode(pair.as_bytes());
                format!("Basic {encoded}")
            }
            _ => String::new(),
        };

        // The reqwest client is shared across operations; tuned for the
        // backup workload (long-running uploads, no per-request timeout
        // so big PUTs can complete).  Connect timeout exists so a wrong
        // host fails fast rather than hanging the whole backup loop.
        let mut builder = Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .pool_idle_timeout(Duration::from_secs(90))
            .user_agent(concat!(
                "NyxBackup/",
                env!("CARGO_PKG_VERSION"),
                " (+https://nyxbackup.com)"
            ));

        // mTLS: attach a client-certificate identity when configured.  This fork
        // builds reqwest with the `native-tls` feature.  Two file formats are
        // accepted, chosen by extension so mutual-TLS WebDAV recovery works on
        // every platform:
        //
        //   - `.p12` / `.pfx` (PKCS#12): `Identity::from_pkcs12_der(der, pass)`.
        //     This is the portable choice and the only one that parses on
        //     Windows-SChannel.  The bundle password, if any, is taken from the
        //     WebDAV `password` field (leave it blank for an unencrypted export).
        //   - anything else (`.pem`): a combined PEM holding BOTH the certificate
        //     chain and the PKCS#8 private key, via `Identity::from_pkcs8_pem`.
        //     Parses on the OpenSSL / SecureTransport backends (Linux/macOS) but
        //     NOT on Windows-SChannel - Windows users should supply a `.p12`.
        //
        // (The main app uses rustls' `Identity::from_pem`, which does not exist
        // under native-tls, hence this split.)
        if let Some(ref cert_path) = cfg.client_cert_path
            && !cert_path.as_os_str().is_empty()
        {
            let bytes = std::fs::read(cert_path).map_err(|e| {
                Error::Storage(format!(
                    "WebDAV client certificate read {}: {e}",
                    cert_path.display()
                ))
            })?;
            let ext = cert_path
                .extension()
                .and_then(|e| e.to_str())
                .map(str::to_ascii_lowercase)
                .unwrap_or_default();
            let identity = if ext == "p12" || ext == "pfx" {
                let pass = cfg.password.as_deref().unwrap_or("");
                reqwest::Identity::from_pkcs12_der(&bytes, pass).map_err(|e| {
                    Error::Storage(format!(
                        "WebDAV client certificate parse (PKCS#12): {e} - \
                         if the .p12 has an export password, put it in the password field"
                    ))
                })?
            } else {
                reqwest::Identity::from_pkcs8_pem(&bytes, &bytes).map_err(|e| {
                    Error::Storage(format!(
                        "WebDAV client certificate parse (PEM): {e} - the PEM must \
                         contain both the certificate and its private key; on Windows use a .p12"
                    ))
                })?
            };
            builder = builder.identity(identity);
        }

        let client = builder
            .build()
            .map_err(|e| Error::Storage(format!("WebDAV client build: {e}")))?;

        // Mask the URL for display in case the user accidentally embedded
        // credentials as `https://user:pass@host/...` - reqwest tolerates
        // it but we don't want it appearing in our logs.
        let display = mask_credentials_in_url(&base_url);

        Ok(Self {
            client,
            base_url,
            auth_header,
            display,
        })
    }

    /// Concatenate the storage root with an object key, percent-encoding
    /// each path segment so `pack-id-with-special-chars` keys can't break
    /// the URL.  Returns the absolute URL ready for an HTTP request.
    fn url_for(&self, path: &str) -> String {
        let mut url = self.base_url.clone();
        for seg in path.trim_start_matches('/').split('/') {
            if seg.is_empty() {
                continue;
            }
            url.push_str(&percent_encode_segment(seg));
            url.push('/');
        }
        // The trailing slash is correct for collections (folders) but
        // wrong for resources (files).  Trim it for file targets; the
        // caller knows the difference because `prefix` calls into
        // `list` / `delete_prefix` pass the trailing slash already.
        if !path.ends_with('/') {
            url.pop();
        }
        url
    }

    /// Build a request with the auth header attached when configured.
    fn request(&self, method: Method, url: &str) -> reqwest::RequestBuilder {
        let mut req = self.client.request(method, url);
        if !self.auth_header.is_empty() {
            req = req.header(AUTHORIZATION, &self.auth_header);
        }
        req
    }

    /// One `PROPFIND` Depth: 1 against `url`; returns the body, or `None`
    /// when the collection 404s.  Building block for the recursive
    /// [`list`](StorageBackend::list) walker.
    async fn propfind_depth1(&self, url: &str) -> Result<Option<String>> {
        // Ask for just <resourcetype> - enough to tell collections from
        // files.  Depth: 1 (Apache mod_dav forbids Depth: infinity).
        const PROPFIND_BODY: &str = concat!(
            r#"<?xml version="1.0" encoding="utf-8"?>"#,
            r#"<propfind xmlns="DAV:"><prop><resourcetype/><getcontentlength/></prop></propfind>"#,
        );

        let mut headers = HeaderMap::new();
        headers.insert("Depth", HeaderValue::from_static("1"));
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/xml; charset=utf-8"),
        );

        let resp = self
            .request(
                Method::from_bytes(b"PROPFIND").expect("PROPFIND is a valid method"),
                url,
            )
            .headers(headers)
            .body(PROPFIND_BODY)
            .send()
            .await
            .map_err(|e| Error::Storage(format!("WebDAV PROPFIND {url}: {e}")))?;

        match resp.status() {
            // 207 Multi-Status is normal; some servers return 200 for an
            // empty collection.
            StatusCode::MULTI_STATUS | StatusCode::OK => {}
            StatusCode::NOT_FOUND => return Ok(None),
            s => return Err(Error::Storage(format!("WebDAV PROPFIND {url}: HTTP {s}"))),
        }

        resp.text()
            .await
            .map(Some)
            .map_err(|e| Error::Storage(format!("WebDAV PROPFIND {url} read body: {e}")))
    }
}

#[async_trait]
impl StorageBackend for WebDavBackend {
    #[instrument(skip(self), fields(base = %self.display, path))]
    async fn get(&self, path: &str) -> Result<Vec<u8>> {
        let url = self.url_for(path);
        let resp = self
            .request(Method::GET, &url)
            .send()
            .await
            .map_err(|e| Error::Storage(format!("WebDAV GET {path}: {e}")))?;
        match resp.status() {
            StatusCode::OK => {
                let bytes = resp
                    .bytes()
                    .await
                    .map_err(|e| Error::Storage(format!("WebDAV GET {path} read body: {e}")))?;
                Ok(bytes.to_vec())
            }
            StatusCode::NOT_FOUND => {
                Err(Error::Storage(format!("WebDAV GET {path}: 404 Not Found")))
            }
            s => Err(Error::Storage(format!("WebDAV GET {path}: HTTP {s}"))),
        }
    }

    #[instrument(skip(self), fields(base = %self.display, path, from, to))]
    async fn get_range(&self, path: &str, from: u64, to: u64) -> Result<Vec<u8>> {
        // RFC 7233 bytes range header.  `to` is inclusive in HTTP land
        // but the trait contract treats `to` as exclusive (end-of-slice
        // style).  Subtract 1 to bridge the two conventions; the storage
        // layer uses [from, to) semantics, HTTP uses [from, to_inclusive].
        if to <= from {
            return Ok(Vec::new());
        }
        let range = format!("bytes={}-{}", from, to.saturating_sub(1));
        let url = self.url_for(path);
        let resp = self
            .request(Method::GET, &url)
            .header(
                RANGE,
                HeaderValue::from_str(&range).map_err(|e| {
                    Error::Storage(format!("WebDAV invalid range header {range}: {e}"))
                })?,
            )
            .send()
            .await
            .map_err(|e| Error::Storage(format!("WebDAV GET range {path}: {e}")))?;
        match resp.status() {
            StatusCode::PARTIAL_CONTENT | StatusCode::OK => {
                // 200 OK from a server that doesn't honour Range still
                // delivers the full body - we slice client-side as a
                // fallback so the call doesn't blow up against
                // non-compliant servers.  Logged at debug; not worth
                // surfacing unless the user files a bug.
                let status = resp.status();
                let bytes = resp
                    .bytes()
                    .await
                    .map_err(|e| Error::Storage(format!("WebDAV GET range {path} read: {e}")))?;
                if status == StatusCode::OK {
                    debug!("WebDAV range request returned 200 (full body); slicing client-side");
                    let from = from as usize;
                    let to = to as usize;
                    let lo = from.min(bytes.len());
                    let hi = to.min(bytes.len());
                    if lo > hi {
                        return Ok(Vec::new());
                    }
                    return Ok(bytes[lo..hi].to_vec());
                }
                Ok(bytes.to_vec())
            }
            StatusCode::NOT_FOUND => Err(Error::Storage(format!(
                "WebDAV GET range {path}: 404 Not Found"
            ))),
            s => Err(Error::Storage(format!("WebDAV GET range {path}: HTTP {s}"))),
        }
    }

    fn display_name(&self) -> String {
        self.display.clone()
    }

    #[instrument(skip(self), fields(base = %self.display, path))]
    // See StorageBackend::probe_access: a single cheap authed round trip via
    // exists("") (a HEAD on the configured root).  Both Ok(true)/Ok(false)
    // mean reachable + authenticated; only a real connect/auth error
    // propagates.  No collection enumeration.
    async fn probe_access(&self) -> Result<()> {
        self.exists("").await.map(|_| ())
    }

    async fn exists(&self, path: &str) -> Result<bool> {
        let url = self.url_for(path);
        let resp = self
            .request(Method::HEAD, &url)
            .send()
            .await
            .map_err(|e| Error::Storage(format!("WebDAV HEAD {path}: {e}")))?;
        match resp.status() {
            StatusCode::OK | StatusCode::NO_CONTENT => Ok(true),
            StatusCode::NOT_FOUND => Ok(false),
            s => Err(Error::Storage(format!("WebDAV HEAD {path}: HTTP {s}"))),
        }
    }

    #[instrument(skip(self), fields(base = %self.display, path))]
    async fn size(&self, path: &str) -> Result<u64> {
        let url = self.url_for(path);
        let resp = self
            .request(Method::HEAD, &url)
            .send()
            .await
            .map_err(|e| Error::Storage(format!("WebDAV HEAD {path}: {e}")))?;
        match resp.status() {
            StatusCode::OK | StatusCode::NO_CONTENT => {
                let len = resp
                    .headers()
                    .get(CONTENT_LENGTH)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .ok_or_else(|| {
                        Error::Storage(format!("WebDAV HEAD {path}: Content-Length missing"))
                    })?;
                Ok(len)
            }
            StatusCode::NOT_FOUND => {
                Err(Error::Storage(format!("WebDAV HEAD {path}: 404 Not Found")))
            }
            s => Err(Error::Storage(format!("WebDAV HEAD {path}: HTTP {s}"))),
        }
    }

    /// List every FILE object under `prefix`, recursively.
    ///
    /// Object stores (S3/B2/...) expose a flat key namespace, so a single
    /// `list("indexes/")` returns `indexes/<set-id>/snapshot-index` keys and
    /// callers discover snapshot sets from them.  WebDAV is hierarchical and
    /// only supports `Depth: 1`, so to honour the same `StorageBackend::list`
    /// contract we walk the tree one collection at a time, descending into
    /// child collections and returning only the files.  Without this, a set
    /// stored as `indexes/<set-id>/snapshot-index` is invisible - a Depth: 1
    /// listing of `indexes/` returns only the `<set-id>/` collection, which
    /// gets filtered out, so discovery finds nothing.  The Nyx layout is
    /// shallow, so this is a handful of requests.
    #[instrument(skip(self), fields(base = %self.display, prefix))]
    async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        let base_path = url_path_only(&self.base_url);
        let mut files: Vec<String> = Vec::new();
        // Directory keys are stored WITHOUT a trailing slash, matching the
        // canonicalised paths the parser emits, so a collection's self-entry
        // compares equal to the key being listed and is skipped (not
        // re-queued).
        let mut stack: Vec<String> = vec![prefix.trim_matches('/').to_string()];
        let mut visited: HashSet<String> = HashSet::new();

        while let Some(dir) = stack.pop() {
            if !visited.insert(dir.clone()) {
                continue;
            }
            let dir_slash = if dir.is_empty() {
                String::new()
            } else {
                format!("{dir}/")
            };
            let url = self.url_for(&dir_slash);
            let body = match self.propfind_depth1(&url).await? {
                Some(b) => b,
                None => continue,
            };
            for (path, is_collection) in parse_propfind_entries(&body, &base_path) {
                if path == dir {
                    continue; // the collection listing itself
                }
                if is_collection {
                    stack.push(path);
                } else {
                    files.push(path);
                }
            }
        }
        Ok(files)
    }
}

/// Normalise an endpoint URL to have a trailing slash and remove any
/// embedded credentials (those go through `username`/`password` instead).
/// Rejects URLs without an `http://` / `https://` scheme.
fn normalise_base_url(raw: &str) -> Result<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(Error::Config("WebDAV endpoint URL is empty".into()));
    }
    if !raw.starts_with("http://") && !raw.starts_with("https://") {
        return Err(Error::Config(format!(
            "WebDAV endpoint URL must start with http:// or https:// (got {raw:?})"
        )));
    }
    // We don't strip embedded creds here - reqwest does the right thing
    // with them - but we DO ensure the trailing slash so url_for can
    // concatenate without ambiguity.
    let mut s = raw.to_string();
    if !s.ends_with('/') {
        s.push('/');
    }
    Ok(s)
}

/// Replace any embedded `user:pass@` portion of a URL with `***@` for
/// safe display in logs.  Used only when logging, never for the actual
/// HTTP requests (those use the parsed credentials directly).
fn mask_credentials_in_url(url: &str) -> String {
    // Match the simplest case: scheme://user:pass@host/...
    let (scheme, rest) = match url.split_once("://") {
        Some(p) => p,
        None => return url.to_string(),
    };
    let (authority, tail) = match rest.split_once('/') {
        Some((a, t)) => (a, format!("/{t}")),
        None => (rest, String::new()),
    };
    match authority.split_once('@') {
        Some((_creds, host)) => format!("{scheme}://***@{host}{tail}"),
        None => url.to_string(),
    }
}

/// Extract the path component of a URL (everything after the host).
/// Returns "/" for URLs with no path.  Used by `parse_propfind_xml` to
/// strip the server-side prefix from each `<d:href>` it sees.
fn url_path_only(url: &str) -> String {
    let after_scheme = match url.split_once("://") {
        Some((_, r)) => r,
        None => url,
    };
    match after_scheme.find('/') {
        Some(i) => after_scheme[i..].to_string(),
        None => "/".to_string(),
    }
}

/// RFC 3986 unreserved-set percent-encoder for a single path segment.
/// We intentionally don't pull in the `percent-encoding` crate for this
/// - the alphabet is small and the implementation is ~10 lines.
fn percent_encode_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let safe = b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.' || b == b'~';
        if safe {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

/// Parse a WebDAV PROPFIND response body.  Returns every entry as
/// `(path, is_collection)`, where `path` is relative to the storage root
/// (the collection being listed is itself returned, as are child
/// collections - the recursive `list` walker needs both to descend).
/// Hrefs may be absolute (`/remote.php/dav/...`) or relative; we
/// canonicalise by stripping the `base_path` prefix when present.
///
/// The parser is intentionally permissive about namespace prefixes -
/// some servers emit `d:href`, others `D:href` or just `href`.  We
/// match on the local-name only and ignore the prefix.
fn parse_propfind_entries(body: &str, base_path: &str) -> Vec<(String, bool)> {
    use quick_xml::Reader;
    use quick_xml::events::Event;

    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);

    let mut out: Vec<(String, bool)> = Vec::new();

    // Per-response state: current href and whether the resourcetype
    // child indicated a collection.  The XML is structured as a series
    // of `<response>` elements each containing one `<href>` and one
    // `<propstat>`/`<prop>`/`<resourcetype>`.
    let mut current_href: Option<String> = None;
    let mut current_is_collection: bool = false;
    let mut in_response: bool = false;
    let mut in_href: bool = false;
    let mut in_resourcetype: bool = false;

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let qname = e.name();
                let local = local_name(qname.as_ref());
                match local {
                    b"response" => {
                        in_response = true;
                        current_href = None;
                        current_is_collection = false;
                    }
                    b"href" if in_response => {
                        in_href = true;
                    }
                    b"resourcetype" if in_response => {
                        in_resourcetype = true;
                    }
                    b"collection" if in_resourcetype => {
                        current_is_collection = true;
                    }
                    _ => {}
                }
            }
            Ok(Event::Empty(e)) => {
                let qname = e.name();
                let local = local_name(qname.as_ref());
                if local == b"collection" && in_resourcetype {
                    current_is_collection = true;
                }
            }
            Ok(Event::End(e)) => {
                let qname = e.name();
                let local = local_name(qname.as_ref());
                match local {
                    b"response" => {
                        if let Some(href) = current_href.take()
                            && let Some(rel) = canonicalise_href(&href, base_path)
                        {
                            out.push((rel, current_is_collection));
                        }
                        in_response = false;
                        current_is_collection = false;
                    }
                    b"href" => {
                        in_href = false;
                    }
                    b"resourcetype" => {
                        in_resourcetype = false;
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(t)) => {
                if in_href {
                    let s = t.unescape().unwrap_or_default().into_owned();
                    let s = s.trim().to_string();
                    if !s.is_empty() {
                        current_href = Some(s);
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                warn!("WebDAV PROPFIND XML parse error: {e}");
                break;
            }
            _ => {}
        }
        buf.clear();
    }
    out
}

/// Strip namespace prefix and return only the local element name.
/// Permissive matching across `d:href`, `D:href`, and bare `href`.
fn local_name(qname: &[u8]) -> &[u8] {
    match qname.iter().position(|&b| b == b':') {
        Some(i) => &qname[i + 1..],
        None => qname,
    }
}

/// Convert an absolute or relative `<d:href>` value into a path relative
/// to the storage root.  Returns `None` when the href IS the root
/// (which we don't want in the listing - that's the collection itself,
/// not a child).
fn canonicalise_href(raw_href: &str, base_path: &str) -> Option<String> {
    // Strip scheme://host/ if the server emitted an absolute URL.
    let after_host = match raw_href
        .find("://")
        .and_then(|i| raw_href[i + 3..].find('/').map(|j| i + 3 + j))
    {
        Some(start) => &raw_href[start..],
        None => raw_href,
    };
    // Percent-decode roughly: leave as-is for now; many servers
    // already return decoded paths and our listing is opaque to
    // the storage layer anyway.  We trim the base_path prefix and
    // hand the remainder back.
    let stripped = after_host.strip_prefix(base_path).unwrap_or(after_host);
    let trimmed = stripped.trim_start_matches('/').trim_end_matches('/');
    if trimmed.is_empty() {
        None
    } else {
        Some(percent_decode_lossy(trimmed))
    }
}

/// Lazy percent-decoder for the bits of paths that come back from
/// servers as `%20` / `%2F` etc.  Invalid sequences pass through
/// unchanged - we don't want a malformed href to crash listing.
fn percent_decode_lossy(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            let parsed = hex.and_then(|h| u8::from_str_radix(h, 16).ok());
            if let Some(b) = parsed {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(&e.into_bytes()).into_owned())
}

/// Construct a backend behind an `Arc<dyn StorageBackend>` for the
/// registry.  Matches the convention used by the other backends.
pub fn build(cfg: WebDavConfig) -> Result<Arc<dyn StorageBackend>> {
    Ok(Arc::new(WebDavBackend::new(cfg)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_encode_basic() {
        assert_eq!(percent_encode_segment("abc"), "abc");
        assert_eq!(percent_encode_segment("a b"), "a%20b");
        assert_eq!(percent_encode_segment("a/b"), "a%2Fb");
        assert_eq!(percent_encode_segment("a.b-c_d~e"), "a.b-c_d~e");
    }

    #[test]
    fn percent_decode_basic() {
        assert_eq!(percent_decode_lossy("abc"), "abc");
        assert_eq!(percent_decode_lossy("a%20b"), "a b");
        assert_eq!(percent_decode_lossy("packs%2Fabc.pack"), "packs/abc.pack");
        // Malformed sequences pass through unchanged.
        assert_eq!(percent_decode_lossy("a%2"), "a%2");
        assert_eq!(percent_decode_lossy("a%ZZ"), "a%ZZ");
    }

    #[test]
    fn url_path_extract() {
        assert_eq!(url_path_only("https://example.com/dav/path/"), "/dav/path/");
        assert_eq!(url_path_only("https://example.com"), "/");
        assert_eq!(url_path_only("http://localhost:8080/"), "/");
    }

    #[test]
    fn base_url_normalisation() {
        assert_eq!(
            normalise_base_url("https://example.com/dav").unwrap(),
            "https://example.com/dav/"
        );
        assert_eq!(
            normalise_base_url("https://example.com/dav/").unwrap(),
            "https://example.com/dav/"
        );
        assert!(normalise_base_url("example.com").is_err());
        assert!(normalise_base_url("").is_err());
    }

    #[test]
    fn mask_credentials() {
        assert_eq!(
            mask_credentials_in_url("https://user:pass@host/path"),
            "https://***@host/path"
        );
        assert_eq!(
            mask_credentials_in_url("https://host/path"),
            "https://host/path"
        );
    }

    #[test]
    fn parse_propfind_minimal() {
        // Nextcloud-style response (D: namespace prefix, hrefs with
        // server-side full path).  One collection (the root) and two
        // files; expect the two files in the output.
        let body = r#"<?xml version="1.0" encoding="UTF-8"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/dav/files/alice/NyxBackup/</d:href>
    <d:propstat>
      <d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>/dav/files/alice/NyxBackup/packs/abc.pack</d:href>
    <d:propstat>
      <d:prop><d:resourcetype/><d:getcontentlength>1234</d:getcontentlength></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>/dav/files/alice/NyxBackup/manifests/snap-001</d:href>
    <d:propstat>
      <d:prop><d:resourcetype/><d:getcontentlength>50</d:getcontentlength></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#;
        let entries = parse_propfind_entries(body, "/dav/files/alice/NyxBackup/");
        let files: Vec<String> = entries
            .iter()
            .filter(|(_, is_coll)| !is_coll)
            .map(|(p, _)| p.clone())
            .collect();
        assert_eq!(files.len(), 2);
        assert!(files.contains(&"packs/abc.pack".to_string()));
        assert!(files.contains(&"manifests/snap-001".to_string()));
    }

    #[test]
    fn parse_propfind_nested_collection() {
        // Apache mod_dav (uppercase D: prefix) Depth:1 listing of indexes/:
        // the collection itself plus one child SUBDIRECTORY (the set id).
        // The child must be reported as a collection so the recursive list
        // walker descends into it - the bug was that set-id directories were
        // dropped, so `indexes/<id>/snapshot-index` was never found.
        let body = r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:" xmlns:lp1="DAV:">
  <D:response>
    <D:href>/indexes/</D:href>
    <D:propstat><D:prop><lp1:resourcetype><D:collection/></lp1:resourcetype></D:prop></D:propstat>
  </D:response>
  <D:response>
    <D:href>/indexes/1c72c8a5-acb6-417c-b3f1-8180bc64bf69/</D:href>
    <D:propstat><D:prop><lp1:resourcetype><D:collection/></lp1:resourcetype></D:prop></D:propstat>
  </D:response>
</D:multistatus>"#;
        let entries = parse_propfind_entries(body, "/");
        // The self-entry (indexes) and the child set-id dir are both
        // collections; the walker skips self and recurses into the child.
        assert!(
            entries.iter().any(|(p, is_coll)| *is_coll
                && p == "indexes/1c72c8a5-acb6-417c-b3f1-8180bc64bf69"),
            "child set-id collection must be parsed: {entries:?}"
        );
    }
}
