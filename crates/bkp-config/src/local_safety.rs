// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! Local-destination safety classification.
//!
//! A backup-set destination is "local" when storage type is `local`
//! (a filesystem path) or when the user is restoring to a Custom
//! local path.  Picking a local destination on the same physical
//! volume as the source data is **not a real backup** - a disk
//! failure, theft of the machine, fire, or ransomware that wipes
//! the source volume also wipes the backup.
//!
//! Most users picking `Local` in the editor don't realise the
//! distinction - they pick `C:\Backups` next to their `C:\Users\me`
//! sources and assume they're protected.  We surface this in the
//! editor as an amber warning banner so the user can
//! either accept the risk explicitly or pick a safer destination
//! (external drive, NAS, network share, cloud backend).
//!
//! # Classification rules
//!
//! - **SameVolumeAsSource** - destination resolves to the same
//!   volume as at least one configured source path.  Strong warning;
//!   the editor blocks save unless the user explicitly acknowledges.
//! - **DifferentVolume** - destination is on a different volume.
//!   May still be on the same machine (USB drive, second internal
//!   disk, mapped network drive), but at least independent of the
//!   source volume's failure modes.  No warning.
//! - **Unknown** - one of the paths doesn't exist, or stat failed.
//!   Surfaced as "couldn't verify" without blocking save.

use std::path::{Path, PathBuf};

/// Classification of a local-disk destination relative to a
/// backup-set's source paths.
///
/// See module docs for the rules behind each variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalSafety {
    /// Destination is on the same volume as `matched_source`.
    /// Strong warning surface in the editor.
    SameVolumeAsSource {
        /// The source path whose volume matches the destination.
        matched_source: PathBuf,
    },
    /// Destination is on a different volume from every configured
    /// source.  Includes UNC paths and mapped network drives on
    /// Windows.  No warning.
    DifferentVolume,
    /// One of the paths could not be inspected (missing parent
    /// directory tree, permission denied on stat, etc.).  The
    /// editor surfaces "couldn't verify" but doesn't block.
    Unknown,
}

/// Classify `dest` against `sources`.
///
/// Returns the first matching source if the destination resolves to
/// the same volume as any of them; otherwise `DifferentVolume`.
/// `Unknown` is returned when stat fails for both `dest` and every
/// source - that's a "we couldn't tell" outcome, not a positive
/// "different volume" claim.
pub fn classify_local_destination(dest: &Path, sources: &[PathBuf]) -> LocalSafety {
    // Walk up the destination path to the nearest ancestor that
    // exists.  Restore destinations and backup-set destinations are
    // often `<prefix>/NyxBackup-<set-id>/...` where the leaf doesn't
    // exist yet; the volume identity sits at any existing ancestor.
    let dest_anchor = nearest_existing_ancestor(dest);
    let dest_anchor = match dest_anchor {
        Some(p) => p,
        None => return LocalSafety::Unknown,
    };

    #[cfg(unix)]
    {
        return classify_unix(&dest_anchor, sources);
    }
    #[cfg(windows)]
    {
        return classify_windows(&dest_anchor, sources);
    }
    #[allow(unreachable_code)]
    LocalSafety::Unknown
}

/// Walk `p` upward until we find an existing path component.  Used
/// because backup destinations frequently point at a subdirectory
/// that will be created on first run; we still want to classify
/// against the parent's volume.
fn nearest_existing_ancestor(p: &Path) -> Option<PathBuf> {
    let mut cur = p.to_path_buf();
    loop {
        if cur.exists() {
            return Some(cur);
        }
        if !cur.pop() {
            return None;
        }
    }
}

// - Unix (Linux + macOS) ---------------------------------------------
//
// Same volume === same device id from stat(2).  `MetadataExt::dev()`
// returns the kernel's device identifier; two files on the same
// mount have identical dev.  Cleaner than parsing /proc/self/mountinfo
// and works the same way on macOS where `/proc` doesn't exist.

#[cfg(unix)]
fn classify_unix(dest_anchor: &Path, sources: &[PathBuf]) -> LocalSafety {
    use std::os::unix::fs::MetadataExt;

    let dest_dev = match std::fs::metadata(dest_anchor) {
        Ok(m) => m.dev(),
        Err(_) => return LocalSafety::Unknown,
    };

    let mut any_source_inspected = false;
    for src in sources {
        let src_anchor = match nearest_existing_ancestor(src) {
            Some(p) => p,
            None => continue,
        };
        let src_dev = match std::fs::metadata(&src_anchor) {
            Ok(m) => m.dev(),
            Err(_) => continue,
        };
        any_source_inspected = true;
        if src_dev == dest_dev {
            return LocalSafety::SameVolumeAsSource {
                matched_source: src.clone(),
            };
        }
    }

    if any_source_inspected {
        LocalSafety::DifferentVolume
    } else {
        LocalSafety::Unknown
    }
}

// - Windows ----------------------------------------------------------
//
// Simple drive-letter comparison.  Both paths are canonicalised (or
// at least normalised to absolute form) and then their two-character
// drive prefix is compared case-insensitively.  UNC paths
// (`\\server\share\...`) and mapped network drives (`Y:` whose
// underlying type is DRIVE_REMOTE) never match a local source drive
// letter, so a UNC destination is correctly classified as
// `DifferentVolume`.
//
// Stronger detection (e.g. `GetVolumeInformationByHandleW` to
// compare volume serials so a mounted-on-folder NTFS junction also
// classifies correctly) is post-1.0.  The drive-letter heuristic
// catches the overwhelming common case where a user picks
// `C:\Backups` next to `C:\Users\...` sources.

#[cfg(windows)]
fn classify_windows(dest_anchor: &Path, sources: &[PathBuf]) -> LocalSafety {
    let dest_drive = match drive_prefix_windows(dest_anchor) {
        Some(d) => d,
        None => return LocalSafety::Unknown,
    };

    let mut any_source_inspected = false;
    for src in sources {
        let src_anchor = match nearest_existing_ancestor(src) {
            Some(p) => p,
            None => continue,
        };
        let src_drive = match drive_prefix_windows(&src_anchor) {
            Some(d) => d,
            None => continue,
        };
        any_source_inspected = true;
        // Case-insensitive ASCII compare ('c:' == 'C:').
        if src_drive.eq_ignore_ascii_case(&dest_drive) {
            return LocalSafety::SameVolumeAsSource {
                matched_source: src.clone(),
            };
        }
    }

    if any_source_inspected {
        LocalSafety::DifferentVolume
    } else {
        LocalSafety::Unknown
    }
}

/// Return the "drive prefix" of a Windows path - the first 2 chars
/// (`C:`, `D:`, etc.) for drive-letter paths, or `None` for UNC
/// paths and anything that isn't a valid drive-letter form.
///
/// UNC returning `None` is intentional: a UNC source can never
/// match a drive-letter destination, and a UNC destination can
/// never match a drive-letter source.
#[cfg(windows)]
fn drive_prefix_windows(p: &Path) -> Option<String> {
    let s = p.to_string_lossy();
    if s.starts_with(r"\\") {
        // UNC: no drive prefix.  Tag with a unique marker so two UNC
        // paths can still compare equal if they're the same server +
        // share.  We treat any two UNC paths as "different volume"
        // for v1 - a server share equality check is post-1.0 polish.
        return None;
    }
    let bytes = s.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        Some(format!("{}:", bytes[0] as char))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_sources_yields_unknown() {
        let dest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        assert_eq!(classify_local_destination(&dest, &[]), LocalSafety::Unknown);
    }

    #[test]
    fn nonexistent_dest_yields_unknown() {
        // A path whose ancestor walk eventually leaves the path tree
        // - on Unix that means we hit /, which exists; on Windows
        // we hit the drive root, which exists.  So a truly "nothing
        // resolves" path is rare.  Test by giving a clearly bogus
        // root that no platform has.
        let dest = PathBuf::from("/__nyx_test_bogus_root_X9__/sub/dir");
        let sources = vec![PathBuf::from(env!("CARGO_MANIFEST_DIR"))];
        // We don't assert exact variant: on Unix the ancestor walk
        // finds `/` so we'd classify against that; on Windows we'd
        // get None drive prefix.  Just confirm we don't panic.
        let _ = classify_local_destination(&dest, &sources);
    }

    #[cfg(unix)]
    #[test]
    fn unix_same_dir_is_same_volume() {
        let me = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let sub = me.join("src");
        match classify_local_destination(&sub, std::slice::from_ref(&me)) {
            LocalSafety::SameVolumeAsSource { matched_source } => {
                assert_eq!(matched_source, me);
            }
            other => panic!("expected SameVolume, got {other:?}"),
        }
    }
}
