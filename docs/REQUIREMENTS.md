# Nyx Backup Recovery - Requirements

Standalone, open-source (Apache-2.0) disaster-recovery tool for Nyx Backup.
This document is the requirements specification for THIS repository (the
recovery tool), distinct from the main Nyx Backup application's
requirements.  It is derived from the main product's REQ-003.1.3 and
REQ-021 but reframed for the spun-out, hard-forked project.

## R-1 Purpose and scope

- **R-1.1** The tool restores data from any Nyx Backup endpoint without
  the main application, a running service/daemon, an IPC connection, or
  a license.
- **R-1.2** Scope is **restore only**.  It does NOT back up, schedule,
  apply retention, run integrity audits, or manage remote state.  It is
  the smallest possible surface for the highest-anxiety user moment.
- **R-1.3** It is **free and unlicensed**.  No license check, no trial,
  no billing, no telemetry.  A customer whose trial or subscription has
  lapsed can still recover their data.  (The main app gates *creating*
  backups, where the paid value sits; recovery is unconditional.)
- **R-1.4** It is **read-only against backup storage**: it downloads,
  decrypts, and writes restored files to a local destination.  It never
  uploads to, deletes from, or mutates remote data.

## R-2 No persistent state / security floor

- **R-2.1** No `config.toml`, no SQLite state database, no OS keyring
  entries.  Session state lives in memory and dies on exit.
- **R-2.2** The master key is typed, pasted, or loaded from a file each
  session.  It is NEVER written to disk or the OS keychain.  This is a
  deliberate security floor: the recovery tool must not become a
  credential-harvesting target.
- **R-2.3** Permitted persistence (convenience only, ACL/permission
  protected, never secrets):
  - Recently-used endpoints cache (last 5): endpoint type, URL, key ID -
    NO secret.
  - Restore checkpoints for restart-safe resume (see R-5).
  - Settings (bandwidth, log level, theme, locale override).
- **R-2.4** OAuth client credentials (Google Drive / OneDrive / Dropbox)
  are compiled into the binary at build time from environment variables;
  they are public-client credentials, not user secrets.

## R-3 Supported endpoints

The tool must read from every destination the main app can write to:
S3 and S3-compatible (Backblaze B2, Wasabi, MinIO, Cloudflare R2, etc.),
Azure Blob, Google Cloud Storage, SFTP, WebDAV, local disk, and the
OAuth providers Google Drive, OneDrive, and Dropbox.

- **R-3.1** "Test connection" verifies credentials cheaply
  (`StorageBackend` reachability) with a bounded retry + wall-clock
  timeout (never an infinite spinner), and reports failures through the
  plain-language error categories of **R-4.6**.
- **R-3.2** Authentication parity with the main app: SFTP accepts a
  password OR a private-key file (with the `secret` field carrying the
  key passphrase); WebDAV accepts anonymous / Basic auth AND optional
  TLS client-certificate (mutual-TLS) auth.  The client certificate is
  supplied as a combined PEM (certificate + key) or, for Windows-SChannel
  compatibility, a PKCS#12 (`.p12`/`.pfx`) bundle.
- **R-3.3** S3-compatible endpoints accept an optional region override
  distinct from the endpoint URL (the endpoint URL occupies the region
  slot per the main-app convention); empty defaults to `us-east-1`, which
  most providers accept.

## R-4 User flow (linear)

1. **R-4.1 Connect** - endpoint type, URL, key ID, secret.  Test
   connection.
2. **R-4.2 Unlock** - paste master-key hex or load a `KEY=<hex>` file;
   the recovery-passphrase + bootstrap-record path (re-derive the master
   key via the recorded KDF params) is the alternative.  Either path
   yields a `MasterKey` held only in memory.  The imported key persists
   across endpoint switches within a session (it is one machine = one
   key), and is cleared on Disconnect.
3. **R-4.3 Snapshot picker** - list manifest objects, decrypt + parse,
   sort; group by backup set when a bucket holds several.  DB-free.
4. **R-4.4 File picker + destination** - browse the snapshot tree, select
   files/folders, choose a custom destination path.
5. **R-4.5 Progress + done** - single progress bar, current-file label,
   total bytes; on completion, "Open folder" + "Restore another", and a
   completion notification.

- **R-4.6 Plain-language errors** - failures shown to the user are
  end-user language, not raw storage/crypto/IO strings.  The engine
  classifies each low-level error into a stable category (not-connected,
  locked, bad recovery key, decrypt failure, access denied, sign-in
  failed, not found / no backups here, missing backup data, timeout,
  network unreachable, out of disk space, ...) with an actionable
  sentence, and localizes it through the `t()` table (**R-7**).  The raw
  technical detail is preserved in the log for diagnosis, never dropped.
- **R-4.7 Restore fidelity** - restored file contents are byte-for-byte
  identical to the source.  Sparse restore is on by default: all-zero
  regions are punched as filesystem holes (a restored VM disk image or
  pre-allocated database file keeps its small on-disk footprint; on
  Windows the file is marked sparse via `FSCTL_SET_SPARSE`).  A Settings
  toggle forces a fully dense write for maximum filesystem compatibility.
  POSIX mode, mtime, owner, and symlink targets are preserved.

## R-5 Restart-safe restore

- **R-5.1** Restore writes a per-restore checkpoint (flat JSON) so an
  interrupted restore (app crash, reboot, network drop) can resume.
- **R-5.2** On launch, an unfinished checkpoint surfaces an "interrupted
  restore" banner with Resume / Discard / Show details.  Resume re-uses
  the endpoint config but RE-PROMPTS for the master key (never cached -
  security floor).  Already-completed files are skipped.
- **R-5.3** The checkpoint is deleted on success or explicit discard.  No
  automatic expiry - the user owns the lifecycle.

## R-6 Cross-platform path remap

When a snapshot's recorded path cannot be expressed on the running OS
(e.g. Windows `C:\Users\...` restored on Linux), the engine remaps it
under the destination (drive letter / leading component becomes a
top-level folder) rather than failing.  Same-OS restores are identity.

## R-7 Settings (minimal)

Download-bandwidth limit (0 = unlimited, with an explicit "Unlimited"
reset), log level (default Info, log at the platform data dir,
size-rolling), theme (9 palettes), sparse restore (on by default; see
**R-4.7**), and an in-app language picker (24 languages; "Auto" follows
`navigator.language`).  Switching language applies live.  All views are
localized through t()/tf(); the recovery-specific strings are filled via
the scripts/i18n/ pipeline (`translate_fill.py` for the machine-translate
pass, `opus_review.py` for the Opus quality pass, with `--only-keys` for
cheap delta polishing when strings are added).

## R-8 Format compatibility (the inviolable requirement)

- **R-8.1** The tool MUST read every backup the shipping main app
  produces.  The on-disk/on-wire format (CBOR manifests + snapshot
  index, pack/envelope layout, key-derivation hierarchy, AES-256-GCM
  AEAD, zstd compression, FastCDC pack boundaries) is the contract.  See
  `docs/DATA_FORMAT.md`.
- **R-8.2** Because this is a hard fork of the main app's read crates,
  format drift in the main app while the fork stalls is the single
  catastrophic failure mode.  A format-conformance test fixture (a real
  pack + manifest + snapshot-index decoded and verified in CI) MUST
  guard against it.

## R-9 Licensing and distribution

- **R-9.1** Source is open under the **Apache-2.0** license (`LICENSE`).  The
  main Nyx Backup application is a separate, proprietary product; only
  this recovery tool is open.
- **R-9.2** Published in its own public repository
  (`github.com/nyxbackup/nyxbackup-recovery`), independent of the main
  monorepo, with no shared git history.  Distribution model: the website
  hub at `nyxbackup.com/recovery` links to the GitHub repository (source
  review) and to `downloads.nyxbackup.com` (the signed installers); the
  GPG public key used to sign the checksum manifest is served from the
  `nyxbackup.com` apex over HTTPS so the verification key comes from a
  domain the project controls.
- **R-9.3** Version is **independent** of the main app's (this repo
  starts at 0.0.1).  A working recovery build should keep working for
  years across many main-app releases - stability comes from the format
  spec, not version coupling.
- **R-9.4** Ships as its own installer per platform: Windows MSI
  (independent `UpgradeCode`), Linux DEB and RPM (`nyxbackup-recovery`
  package), macOS PKG.  No auto-update - it is intentionally static; users
  grab a fresh copy from the website if needed.
- **R-9.5** Provides both **x86-64 and ARM64** builds on Windows and Linux
  (macOS ships a single **universal** `.pkg` covering Intel and Apple
  Silicon).  A recovery tool may be the only thing
  that runs on a replacement machine, which is increasingly ARM (Windows on
  ARM, ARM servers/SBCs), so ARM64 is a first-class target, not an
  afterthought.  Cross-building the ARM64 installers from an x86-64 host has
  host prerequisites documented in `docs/BUILD_ARM64.md`.

## R-10 Deliberately excluded

No license display or billing; no auto-update; no multi-machine view
(one endpoint at a time); no backup capability of any kind (never links
`bkp-engine`); no OS-keyring access.
