// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! User-facing error mapping.
//!
//! The storage / crypto / IO layers return precise, technical error strings
//! (`OneDrive head_with_hash: not found: packs/...`, `Argon2id derivation
//! failed`, `WebDAV HEAD ...: 404 Not Found`).  Those are perfect for the
//! recovery log but read as jargon to the person actually trying to get their
//! files back.
//!
//! [`user_error`] classifies a low-level error into a *stable i18n code* plus a
//! plain-language English fallback, and packages both with the raw detail into
//! a single string:
//!
//! ```text
//! <code>\u{1}<english message>\u{1}<raw detail>
//! ```
//!
//! Tauri hands that string to the GUI as the rejected command's error.  The
//! frontend's `friendlyError()` splits on `\u{1}`, translates `<code>` through
//! the locale table (falling back to the English message when a locale lacks
//! the key), and shows the plain sentence - never the raw detail, which stays
//! in `recovery.log` for diagnosis.
//!
//! Unclassified/legacy error strings (no `\u{1}`) are shown by the GUI as-is,
//! so partial adoption is safe.

/// Field separator between code / message / detail.  U+0001 (SOH) never occurs
/// in a real error message, so splitting on it is unambiguous.
const SEP: char = '\u{1}';

/// The operation being attempted.  Lets [`user_error`] tailor the message
/// (a "not found" while listing snapshots means "no backups here"; the same
/// while restoring means "backup data is missing").
#[derive(Clone, Copy)]
pub enum Ctx {
    Connect,
    Unlock,
    ListSnapshots,
    ListFiles,
    Restore,
    ReadKeyFile,
    Oauth,
    Generic,
}

/// Marker a backend embeds in its error text to pass a copy-pasteable shell
/// command up to the GUI.  Everything after it is the command.  See
/// `bkp_storage::backends::smb`, which uses it for the "share is not mounted"
/// case: recovery cannot mount the share itself, so the user has to.
const HINT_MARKER: &str = "mount-command: ";

/// Classify `raw` under `ctx` and return the `code\u{1}message\u{1}detail`
/// string the GUI expects.  `raw` is the low-level error's `to_string()`.
///
/// When `raw` carries a [`HINT_MARKER`], a fourth `\u{1}`-separated field is
/// appended holding the command.  The GUI shows that field verbatim; unlike
/// `detail` (which is log-only) it is actionable, and unlike `message` it
/// cannot be localized because it is a literal command line.
pub fn user_error(ctx: Ctx, raw: &str) -> String {
    let (code, msg) = classify(ctx, raw);
    match raw.split_once(HINT_MARKER) {
        Some((_, hint)) if !hint.trim().is_empty() => {
            format!("{code}{SEP}{msg}{SEP}{raw}{SEP}{}", hint.trim())
        }
        _ => format!("{code}{SEP}{msg}{SEP}{raw}"),
    }
}

/// Convenience: classify an error implementing `Display`.
pub fn ue(ctx: Ctx, err: &impl std::fmt::Display) -> String {
    user_error(ctx, &err.to_string())
}

/// Map a raw error string + context to a stable i18n code and English message.
/// Order is most-specific-first; the English text is the last-resort fallback
/// shown when a locale is missing the key.
fn classify(ctx: Ctx, raw: &str) -> (&'static str, &'static str) {
    let low = raw.to_ascii_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| low.contains(n));

    // Session-state guards.
    if has(&["not connected"]) {
        return (
            "err.not_connected",
            "Not connected yet. Complete the Connect step first.",
        );
    }
    if has(&["not unlocked"]) {
        return (
            "err.not_unlocked",
            "The backup is locked. Enter your recovery key to unlock it first.",
        );
    }

    // Recovery-key problems (checked before the generic decrypt bucket).
    if has(&[
        "invalid master key",
        "odd number of hex",
        "non-hex",
        "hex char",
    ]) {
        return (
            "err.badkey",
            "The recovery key is not valid. Paste the full key exactly as it was shown when the backup was created.",
        );
    }
    if has(&[
        "decrypt",
        "aead",
        "tag mismatch",
        "argon2",
        "authentication tag",
        "hkdf",
        "aes-gcm",
    ]) {
        return (
            "err.decrypt",
            "Could not decrypt the backup. The recovery key may be incorrect, or the data may be in an unsupported format.",
        );
    }

    // Cold-storage / archive tier (S3 Glacier / Deep Archive, Azure Archive,
    // GCS Archive).  The recovery tool does NOT auto-thaw; it surfaces the
    // provider's raw error (e.g. S3 "InvalidObjectState"), so match that text.
    // Checked before permission/auth because InvalidObjectState surfaces as 403.
    if has(&[
        "invalidobjectstate",
        "object's storage class",
        "glacier",
        "deep archive",
        "deep_archive",
        "archive tier",
        "archived",
        "not restored",
        "rehydrat",
        "x-ms-archive",
        "cold storage",
    ]) {
        return (
            "err.archive",
            "This backup is in cold storage (Glacier / Archive tier). The data must be retrieved (thawed) before it can be restored - this can take several hours and may incur provider fees. Start the retrieval with your storage provider, then try again.",
        );
    }

    // Disk space (restore destination).
    if has(&["no space", "disk full", "enospc"]) {
        return (
            "err.disk",
            "There is not enough free disk space at the restore destination.",
        );
    }

    // Access denied / permissions.
    if has(&[
        "permission denied",
        "eacces",
        "access is denied",
        "forbidden",
        " 403",
        "(403)",
    ]) {
        return (
            "err.permission",
            "Access was denied. Check that your credentials or keys have permission to read this data.",
        );
    }

    // Authentication / credentials.
    if has(&[
        " 401",
        "(401)",
        "unauthor",
        "invalid credential",
        "signaturedoesnotmatch",
        "authentication failed",
        "invalid access key",
        "access denied",
        "bad credential",
    ]) {
        return (
            "err.auth",
            "Sign-in failed. Check your access key, secret, username, or password.",
        );
    }

    // SMB share not mounted (Linux/macOS).  Checked before the generic
    // not-found bucket: the text contains "not mounted", and the useful advice
    // is "mount it", not "check the path".
    if has(&["smb share is not mounted"]) {
        return (
            "err.smb_not_mounted",
            "The SMB share is not mounted. Mount it in the operating system first, then connect again.",
        );
    }

    // Not found (context-sensitive).
    if has(&[
        "not found",
        " 404",
        "(404)",
        "no such",
        "nosuchbucket",
        "nosuchkey",
        "does not exist",
    ]) {
        return match ctx {
            Ctx::Restore => (
                "err.restore_missing",
                "Some of the backup's data could not be found in storage. The backup may be incomplete, or the wrong location was selected.",
            ),
            Ctx::ListSnapshots | Ctx::ListFiles => (
                "err.no_backups",
                "No backups were found at this location. Double-check the address, bucket, or folder path.",
            ),
            _ => (
                "err.not_found",
                "The storage location or backup data could not be found. Double-check the address, bucket, or path.",
            ),
        };
    }

    // Timeouts.
    if has(&["timed out", "timeout", "deadline"]) {
        return (
            "err.timeout",
            "The storage server did not respond in time. Check your connection and try again.",
        );
    }

    // Network / TLS / DNS.
    if has(&[
        "dns",
        "resolve",
        "name resolution",
        "no such host",
        "connection refused",
        "connection reset",
        "network",
        "unreachable",
        "tls",
        "certificate",
        "connect error",
        "failed to connect",
    ]) {
        return (
            "err.network",
            "Could not reach the storage server. Check the address and your internet connection.",
        );
    }

    // Bad identifiers (snapshot/set id parse).
    if has(&["uuid", "set_id", "snapshot_id"]) {
        return (
            "err.badinput",
            "The selected backup or snapshot could not be identified. Try reloading the snapshot list.",
        );
    }

    // Context-specific generic fallbacks.
    match ctx {
        Ctx::Connect => (
            "err.connect_generic",
            "Could not connect to the storage location. Check the address and credentials.",
        ),
        Ctx::Unlock => (
            "err.unlock_generic",
            "Could not unlock the backup with this recovery key.",
        ),
        Ctx::ListSnapshots | Ctx::ListFiles => (
            "err.list_generic",
            "Could not read the backups at this location.",
        ),
        Ctx::Restore => ("err.restore_generic", "The restore could not be completed."),
        Ctx::Oauth => (
            "err.oauth_generic",
            "Sign-in did not complete. Please try again.",
        ),
        Ctx::ReadKeyFile => ("err.readfile_generic", "Could not read the selected file."),
        Ctx::Generic => (
            "err.generic",
            "Something went wrong. See the log for details.",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code_of(s: &str) -> &str {
        s.split('\u{1}').next().unwrap()
    }

    #[test]
    fn classifies_common_failures() {
        assert_eq!(
            code_of(&user_error(
                Ctx::Restore,
                "OneDrive: not found: packs/x.pack"
            )),
            "err.restore_missing"
        );
        assert_eq!(
            code_of(&user_error(
                Ctx::ListSnapshots,
                "WebDAV HEAD /x: 404 Not Found"
            )),
            "err.no_backups"
        );
        // Cold-storage: S3 returns InvalidObjectState (a 403) for a Glacier
        // object; archive must win over the permission (403) classifier.
        assert_eq!(
            code_of(&user_error(
                Ctx::Restore,
                "S3 get packs/x.pack: 403 InvalidObjectState: object is archived"
            )),
            "err.archive"
        );
        assert_eq!(
            code_of(&user_error(Ctx::Unlock, "invalid master key: odd length")),
            "err.badkey"
        );
        assert_eq!(
            code_of(&user_error(Ctx::Connect, "SignatureDoesNotMatch")),
            "err.auth"
        );
        assert_eq!(
            code_of(&user_error(
                Ctx::Connect,
                "dns error: failed to lookup host"
            )),
            "err.network"
        );
        assert_eq!(
            code_of(&user_error(Ctx::Generic, "not connected")),
            "err.not_connected"
        );
    }

    #[test]
    fn payload_roundtrips_detail() {
        let s = user_error(Ctx::Connect, "raw detail here");
        let parts: Vec<&str> = s.split('\u{1}').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[2], "raw detail here");
    }

    /// An unmounted SMB share is its own code, not the generic not-found - the
    /// useful advice is "mount it", not "check the path".
    #[test]
    fn unmounted_smb_share_is_classified_separately() {
        assert_eq!(
            code_of(&user_error(
                Ctx::Connect,
                "SMB share is not mounted at /mnt/smb/set-a; mount-command: sudo mount -t cifs //nas.example/backups /mnt/smb"
            )),
            "err.smb_not_mounted"
        );
    }

    /// The mount command is split out into a fourth field so the GUI can show
    /// it verbatim; the detail field still carries the whole raw error.
    #[test]
    fn payload_carries_the_mount_command_as_a_fourth_field() {
        let raw = "SMB share is not mounted at /mnt/smb/set-a; \
                   mount-command: sudo mount -t cifs //nas.example/backups /mnt/smb";
        let s = user_error(Ctx::Connect, raw);
        let parts: Vec<&str> = s.split('\u{1}').collect();
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[2], raw);
        assert_eq!(
            parts[3],
            "sudo mount -t cifs //nas.example/backups /mnt/smb"
        );
    }

    /// Errors without the marker keep the three-field shape, so every existing
    /// consumer is unaffected.
    #[test]
    fn payload_without_a_hint_stays_three_fields() {
        let s = user_error(Ctx::Connect, "path not found or not a directory: /x");
        assert_eq!(s.split('\u{1}').count(), 3);
    }
}
