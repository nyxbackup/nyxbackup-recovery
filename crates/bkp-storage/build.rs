// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com
//
// bkp-storage embeds OAuth client_id / client_secret constants for
// Dropbox, Google Drive, and OneDrive via `option_env!` so a `cargo
// build` without the env vars set produces an empty-string fallback
// rather than a hard compile error.  That fallback is correct for the
// "I'm just hacking on the local file backend" case, but it means a
// daemon binary built without those env vars in scope will SILENTLY
// produce OAuth refresh failures at runtime ("Invalid client_id"
// 400s from Dropbox) the user has no way to diagnose from the
// daemon log alone.
//
// Two things this build.rs does to close that gap:
//
// 1. `cargo:rerun-if-env-changed=...` for each var.  Without it,
//    cargo's incremental-build cache happily reuses an old compile
//    that captured an EMPTY value of the env var even after the
//    caller sets the var and re-runs cargo build.  The
//    `rerun-if-env-changed` directive tells cargo to invalidate the
//    compile output whenever the env var's value changes.
//
// 2. A loud INFO line during build that prints which OAuth providers
//    have credentials baked in.  Showing this in `cargo build --verbose`
//    catches the "I forgot to source .env" mistake early instead of
//    at first OAuth refresh six hours later.

fn main() {
    for var in &[
        "DROPBOX_APP_KEY",
        "DROPBOX_APP_SECRET",
        "GOOGLE_OAUTH_CLIENT_ID",
        "GOOGLE_OAUTH_CLIENT_SECRET",
        "ONEDRIVE_OAUTH_CLIENT_ID",
        "ONEDRIVE_OAUTH_CLIENT_SECRET",
    ] {
        println!("cargo:rerun-if-env-changed={var}");
    }
}
