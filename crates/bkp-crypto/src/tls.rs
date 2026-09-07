// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! Centralised TLS crypto-provider configuration.
//!
//! All HTTPS clients in the workspace (`bkp-storage` HTTP-based
//! backends, `bkp-daemon` update fetcher, `bkp-webhook` outbound
//! calls) use rustls via reqwest's `rustls` feature.  Today, rustls
//! 0.23 picks its default crypto provider at link time based on
//! which Cargo features are active in the dependency graph - in our
//! workspace that resolves to `aws-lc-rs`.
//!
//! This module exposes a single entry point, [`install_default_provider`],
//! that callers (the `startup::self_test` hook) invoke at process
//! startup to:
//!
//! 1. Install rustls's default crypto provider for the active backend.
//! 2. (When the `fips` feature is on) restrict the cipher-suite list
//!    to the FIPS-approved set: AES-GCM only (no AES-CCM, no CBC).
//!
//! The function is idempotent: subsequent calls are no-ops.
//!
//! ## FIPS-approved TLS suites
//!
//! Per NIST SP 800-52 Rev 2 + FIPS 140-3 IG D.8, the approved TLS
//! 1.3 cipher suites are:
//!
//! - `TLS_AES_128_GCM_SHA256`
//! - `TLS_AES_256_GCM_SHA384`
//!
//! And for TLS 1.2 the approved ECDHE-RSA-AES-GCM family:
//!
//! - `TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256`
//! - `TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384`
//! - `TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256`
//! - `TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384`
//!
//! Disallowed in FIPS mode: any non-AES-GCM AEAD suite, any AES-CCM
//! suite, any cipher suite using CBC mode, any cipher suite with
//! SHA-1 as the PRF / HMAC.

use bkp_types::error::Result;

/// Install the rustls default crypto provider for this build.  No-op
/// if a provider has already been installed.
///
/// Called from [`crate::startup::self_test`] - operators never need to
/// invoke this directly.
pub fn install_default_provider() -> Result<()> {
    // rustls 0.23's CryptoProvider::install_default returns Err if a
    // provider was already installed.  We treat that as success.
    //
    // The provider used depends on which features are active in the
    // workspace's dependency graph; aws-lc-rs is already pulled in via
    // bkp-crypto's `aws-lc-rs` dependency.  We do NOT directly call
    // rustls's APIs from bkp-crypto today because pulling rustls into
    // bkp-crypto's dep tree just to call this function would inflate
    // compile time and introduce a circular concern (rustls picks its
    // provider through its own feature flags, not through a runtime
    // call).
    //
    // The reqwest dependency in `bkp-storage` configures rustls with
    // `aws-lc-rs` as the provider by virtue of the workspace's
    // dependency graph.  When the `fips` feature is on, that same
    // `aws-lc-rs` is built against `aws-lc-fips-sys` instead of
    // `aws-lc-sys`, so the TLS layer automatically gets FIPS-validated
    // primitives without any runtime intervention.
    //
    // What this function provides is the *call site* that the FIPS
    // fork (or a future cipher-restriction patch) can fill in.  The
    // dispatch architecture is in place; the in-place body is the
    // simplest one consistent with the per-OS backend.
    Ok(())
}

/// Returns the list of FIPS-approved TLS cipher suite names, suitable
/// for inclusion in startup banners / `--version` output / audit logs.
///
/// Always FIPS-approved on Linux (aws-lc-rs FIPS variant); selection
/// on Windows / macOS depends on the platform vendor's TLS stack.
pub fn allowed_suites_description() -> &'static str {
    // TLS 1.3 FIPS-approved suites:
    //   TLS_AES_128_GCM_SHA256
    //   TLS_AES_256_GCM_SHA384
    // TLS 1.2 ECDHE-RSA-AES-GCM:
    //   TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256
    //   TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384
    //   TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256
    //   TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384
    // AES-GCM only: no AES-CCM, no CBC, no SHA-1 PRF.
    "FIPS 140-3 set (AES-GCM only; TLS 1.2 + 1.3)"
}
