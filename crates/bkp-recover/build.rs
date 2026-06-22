// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! Build script for bkp-recover.  Mirrors bkp-gui's build.rs: walks the
//! frontend output directory and emits `cargo:rerun-if-changed` for every
//! file so cargo correctly invalidates this crate when the embedded
//! Svelte frontend changes.  Without these directives a frontend-only
//! edit (`vite build` after editing a component) would leave a stale
//! binary embedded with the previous frontend bytes.

use std::path::Path;

fn main() {
    rerun_if_tree_changed(Path::new("ui/dist"));
    rerun_if_tree_changed(Path::new("ui/src"));
    println!("cargo:rerun-if-changed=ui/index.html");
    println!("cargo:rerun-if-changed=ui/package.json");
    println!("cargo:rerun-if-changed=ui/vite.config.ts");
    println!("cargo:rerun-if-changed=tauri.conf.json");

    tauri_build::build();
}

/// Recursively walk `dir` and emit a `cargo:rerun-if-changed` directive
/// for every file found.  Silently skips when the directory does not
/// exist (e.g. before the first frontend build).
fn rerun_if_tree_changed(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rerun_if_tree_changed(&path);
        } else {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}
