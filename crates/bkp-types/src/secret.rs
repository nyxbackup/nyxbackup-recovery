// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! Sensitive-value wrapper.
//!
//! `Secret<T>` is a transparent newtype that wraps any zeroize-able value
//! with two safety properties:
//!
//! 1. **Memory hygiene**: the inner value is zeroed on `Drop`, so a process
//!    coredump or memory inspection after the secret leaves scope finds
//!    only zero bytes rather than the live credential.  Backed internally
//!    by [`zeroize::Zeroizing`].
//!
//! 2. **Debug-print safety**: the [`std::fmt::Debug`] implementation prints
//!    `Secret(<redacted>)` instead of the inner value, so a careless
//!    `tracing::error!("{:?}", cfg)` or `dbg!(cfg)` on a struct that
//!    contains a `Secret<T>` field cannot leak the credential into a log
//!    file.
//!
//! `Secret<T>` derefs to `&T` so most read-only call sites work unchanged.
//! Sites that assign a fresh secret must wrap with `Secret::new(...)`.
//!
//! Serde is implemented transparently (the inner value serializes /
//! deserializes as `T`), so a `Secret<String>` round-trips through TOML
//! / JSON / CBOR exactly as a plain `String` would.  Callers wanting to
//! avoid writing a secret to disk should add `#[serde(skip_serializing)]`
//! on the field itself, exactly as they would for an unwrapped `String`.

use std::fmt;
use std::ops::Deref;

use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

/// Transparent wrapper around `T` that:
/// - zeroes the inner value on `Drop`
/// - prints `Secret(<redacted>)` when formatted with `{:?}`
/// - serializes / deserializes as `T`
/// - derefs to `&T` for ergonomic read access
///
/// See [module docs](self) for the full rationale.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret<T: Zeroize + Clone> {
    inner: T,
}

impl<T: Zeroize + Clone + DefaultForTake> Secret<T> {
    /// Wrap a value in a `Secret`.
    pub fn new(value: T) -> Self {
        Self { inner: value }
    }

    /// Extract the inner value, consuming the `Secret`.  The returned `T`
    /// will NOT zero on drop (it leaves the protection envelope); use this
    /// only when handing off to a different lifetime-bound owner.
    pub fn into_inner(mut self) -> T {
        // Avoid the Drop impl zeroing the value before we hand it off.
        let mut out = T::default_for_take();
        std::mem::swap(&mut out, &mut self.inner);
        // Now `self.inner` holds the placeholder default; when self drops,
        // the default gets zeroed (no-op for a fresh default), and `out`
        // carries the original value to the caller.
        std::mem::forget(self);
        out
    }

    /// Borrow the inner value.  Identical to `Deref::deref` but spelled
    /// out for places where rustc cannot infer the deref coercion.
    pub fn expose(&self) -> &T {
        &self.inner
    }
}

// Specialized Deref impls.  A generic `Deref<Target = T>` would force
// `Option<Secret<String>>::as_deref()` to return `Option<&String>` rather
// than `Option<&str>`, breaking every caller that expects the latter
// (which is most of them, e.g. `cfg.access_key_id.as_deref().unwrap_or("")`).
// The specialized impls below make `Secret<String>` deref to `str` and
// `Secret<Vec<u8>>` deref to `[u8]` - the same shapes the bare `String` /
// `Vec<u8>` would have given before the wrap.

impl Deref for Secret<String> {
    type Target = str;

    fn deref(&self) -> &str {
        self.inner.as_str()
    }
}

impl Deref for Secret<Vec<u8>> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.inner.as_slice()
    }
}

impl<T: Zeroize + Clone> Drop for Secret<T> {
    fn drop(&mut self) {
        self.inner.zeroize();
    }
}

impl<T: Zeroize + Clone> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

// `Default` is implemented for the two common inner types so existing call
// patterns like `cfg.access_key_id.clone().unwrap_or_default()` keep
// compiling against `Option<Secret<String>>` without unwrap chains.  The
// default value is a fresh empty `String` / `Vec<u8>`, which is already at
// rest (nothing to zero), so the Drop impl is a no-op for defaults.
impl Default for Secret<String> {
    fn default() -> Self {
        Self {
            inner: String::new(),
        }
    }
}

impl Default for Secret<Vec<u8>> {
    fn default() -> Self {
        Self { inner: Vec::new() }
    }
}

// Convenience `From` impls so `Some(Secret::new(s))` can shorten to
// `Some(s.into())` at call sites that build EndpointConfig literals.
impl From<String> for Secret<String> {
    fn from(s: String) -> Self {
        Self::new(s)
    }
}

impl From<Vec<u8>> for Secret<Vec<u8>> {
    fn from(v: Vec<u8>) -> Self {
        Self::new(v)
    }
}

/// Helper trait so `Secret::into_inner` can construct a zero-cost
/// placeholder during the value-extraction swap without requiring `T:
/// Default` (which would force `String` to allocate a fresh empty heap
/// buffer rather than reusing the already-allocated capacity).
///
/// Implementations should return a value that is cheap to zero (a fresh
/// empty `String`, an empty `Vec<u8>`, etc.).  This trait is sealed to
/// `bkp-types` to avoid being misused by downstream code.
pub trait DefaultForTake: Zeroize {
    /// Returns a placeholder value used during the `into_inner` swap.
    fn default_for_take() -> Self;
}

impl DefaultForTake for String {
    fn default_for_take() -> Self {
        String::new()
    }
}

impl DefaultForTake for Vec<u8> {
    fn default_for_take() -> Self {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_redacts() {
        let s = Secret::new("hunter2".to_string());
        assert_eq!(format!("{:?}", s), "Secret(<redacted>)");
    }

    #[test]
    fn deref_exposes_for_read() {
        let s = Secret::new("hunter2".to_string());
        assert_eq!(s.len(), 7);
        assert!(s.starts_with("hunter"));
    }

    #[test]
    fn into_inner_yields_original() {
        let s = Secret::new("hunter2".to_string());
        assert_eq!(s.into_inner(), "hunter2");
    }

    #[test]
    fn serde_round_trip_is_transparent() {
        // CBOR round-trip (ciborium is a dep of bkp-types; serde_json is not).
        let s = Secret::new("hunter2".to_string());
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&s, &mut buf).unwrap();
        let back: Secret<String> = ciborium::de::from_reader(&buf[..]).unwrap();
        assert_eq!(back.expose(), "hunter2");
    }

    /// Guard: the on-disk / on-wire structs defined in bkp-types
    /// MUST NOT embed `Secret<T>`.  Because Serde for `Secret<T>` is
    /// `#[serde(transparent)]`, a `Secret<String>` field would silently
    /// serialize its inner cleartext into CBOR manifests, snapshot
    /// indexes, bootstrap records, etc - completely defeating the wrapper's
    /// purpose for at-rest / in-cloud data.
    ///
    /// This test source-greps the on-disk module files for the token
    /// `Secret<` and fails the build if it appears.  If a future field
    /// legitimately needs that type, **change the file list below** with a
    /// commit message explaining the audit trail; do not just silence the
    /// test.
    #[test]
    fn no_secret_in_on_disk_structs() {
        const ON_DISK_SOURCES: &[(&str, &str)] = &[
            ("manifest.rs", include_str!("manifest.rs")),
            ("snapshot.rs", include_str!("snapshot.rs")),
            ("machine.rs", include_str!("machine.rs")),
            ("backup_set.rs", include_str!("backup_set.rs")),
            ("endpoint.rs", include_str!("endpoint.rs")),
            ("chunk.rs", include_str!("chunk.rs")),
            ("retention.rs", include_str!("retention.rs")),
        ];
        for (name, src) in ON_DISK_SOURCES {
            assert!(
                !src.contains("Secret<"),
                "secret-leak guard violation: `Secret<` appears in `{name}` - that module \
                 defines types serialized to disk / cloud via CBOR.  Because \
                 `Secret<T>` is `#[serde(transparent)]`, embedding it leaks the \
                 wrapped cleartext into the on-wire bytes.  If this is intentional, \
                 update the audit list in tests::no_secret_in_on_disk_structs."
            );
        }
    }
}
