// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! Compile-time i18n for the macOS NSMenu strings shown by the Recovery
//! Tool's native menu bar (`About`, `Preferences...`, the `Nyx Backup
//! Recovery` submenu title, and the `Edit` submenu title).
//!
//! The renderer-side i18n (`crates/bkp-recover/ui/src/lib/i18n.svelte.ts`)
//! handles every Svelte template string, but the NSMenu is built on the
//! Rust side BEFORE the renderer has loaded - we can't reach into the
//! webview to ask "what's the current locale's translation for X".
//! Instead this module embeds the en + es locale JSON files at compile
//! time and looks up the four `gui.recover.menu.*` keys at menu-build
//! time using the locale resolved by [`resolve_locale`].
//!
//! Only en + es are bundled today.  Adding a third locale = one new
//! `include_str!` line, one match arm in `lookup`, and the four
//! `gui.recover.menu.*` keys in that locale's JSON.  The other 22
//! supported locales currently fall through to English.

use std::collections::HashMap;
use std::sync::OnceLock;

/// Raw locale JSON embedded at compile time.  Paths are relative to
/// this source file via `CARGO_MANIFEST_DIR`-anchored
/// `concat!(env!("CARGO_MANIFEST_DIR"), ...)` so `cargo run`, full
/// release builds, and IDE-driven checks all agree on the source of
/// truth (the workspace's shared `locales/` directory).
const RAW_EN: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../locales/en.json"
));
const RAW_ES: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../locales/es.json"
));

/// Lazily-parsed locale maps.  Parsed once on first lookup per locale.
/// A locale whose JSON fails to parse falls through to English on next
/// lookup - we never panic on a malformed locale file because that
/// would deny the user a native macOS menu over a localisation bug.
fn map_for(locale: &str) -> &'static HashMap<String, String> {
    static EN: OnceLock<HashMap<String, String>> = OnceLock::new();
    static ES: OnceLock<HashMap<String, String>> = OnceLock::new();
    match locale {
        "es" => ES.get_or_init(|| parse_or_empty(RAW_ES)),
        _ => EN.get_or_init(|| parse_or_empty(RAW_EN)),
    }
}

fn parse_or_empty(raw: &str) -> HashMap<String, String> {
    serde_json::from_str(raw).unwrap_or_default()
}

/// Resolve the effective locale: `"auto"` (or any unsupported value)
/// falls back to a platform-native detection.  On macOS we read
/// `defaults read -g AppleLocale` once (single fork + exec, ~5 ms),
/// take the language primary tag (`en_US` -> `en`), and return that
/// if it's in our bundled set.  Everywhere else we just hand back
/// English.
pub fn resolve_locale(settings_locale: &str) -> String {
    let s = settings_locale.trim();
    if !s.is_empty() && s != "auto" {
        return s.to_ascii_lowercase();
    }
    detect_native_locale().unwrap_or_else(|| "en".to_string())
}

#[cfg(target_os = "macos")]
fn detect_native_locale() -> Option<String> {
    use std::process::Command;
    let out = Command::new("/usr/bin/defaults")
        .args(["read", "-g", "AppleLocale"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    // AppleLocale is shaped like `en_US`, `es_ES`, `pt_BR`.  Take the
    // primary subtag and lowercase it for our two-letter set.
    let primary = text.split('_').next()?.to_ascii_lowercase();
    if primary.is_empty() {
        None
    } else {
        Some(primary)
    }
}

#[cfg(not(target_os = "macos"))]
fn detect_native_locale() -> Option<String> {
    None
}

/// Look up `key` in the bundle for `locale`.  Falls back to English
/// when the key is absent in the requested locale (covers a locale
/// that's bundled but missing this particular string).  Falls back to
/// the key itself if even English doesn't have it (should never
/// happen at runtime; helps surface a missing key during development).
///
/// Returns an owned `String` because Tauri's menu builders take owned
/// labels - the menu structure outlives the menu-build call and a
/// borrowed `&'static str` from a `OnceLock<HashMap>` would force a
/// `Box::leak` per lookup.  The cost is four small allocations at
/// startup, paid once for the lifetime of the app.
pub fn lookup(locale: &str, key: &str) -> String {
    if let Some(v) = map_for(locale).get(key) {
        return v.clone();
    }
    if let Some(v) = map_for("en").get(key) {
        return v.clone();
    }
    key.to_string()
}
