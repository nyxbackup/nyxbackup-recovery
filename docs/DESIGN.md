# Nyx Backup Recovery - Design

How the recovery tool is built.  Read this with `REQUIREMENTS.md` (what
it must do) and `DATA_FORMAT.md` (the format it must read).

## Scope

This is an independent, open-source (Apache-2.0) reader for the Nyx
Backup on-disk format.  It restores data from a backup destination on
its own - no main product, license, or running service required - and
contains only the crates needed to read and restore.

Because the reader carries its own copy of the format-defining crates,
they must stay byte-compatible with the format they read.  The intended
guard is a format-conformance test fixture (see "Format parity" below);
it is planned but not yet committed.

## Crate layering

Lower layers never depend on higher ones:

```
bkp-types                         foundation domain types + CBOR structs
  |
  +-- bkp-crypto    decrypt / KDF / hash / verify
  +-- bkp-config    endpoint + config parsing
  |
  +-- bkp-storage   read-only StorageBackend implementations
  +-- bkp-chunker   unpack + zstd decompress
  +-- bkp-manifest  CBOR manifest / snapshot-index decode
  +-- bkp-oauth     OAuth token refresh
  +-- bkp-log       size-rolling log appender
        |
        +-- bkp-restore   browse snapshots + restore files
              |
              +-- bkp-recover   Tauri 2 GUI app (bin: nyx_bkp_recover) + Svelte UI
```

## Runtime architecture

The app runs the restore engine **in-process**.  The Tauri Rust side
(`crates/bkp-recover/src/`) is a thin set of command handlers; the Svelte
frontend (`crates/bkp-recover/ui/`) drives the five-screen flow.  A
command:

1. builds a `StorageBackend` from the user-entered endpoint config
   (`registry::build_backend`),
2. unlocks a `MasterKey` from the pasted key (or re-derived from a
   bootstrap record),
3. lists + decrypts manifest objects to present snapshots,
4. on restore, downloads packs via `bkp-storage`, decrypts via
   `bkp-crypto`, unpacks/decompresses via `bkp-chunker`, reassembles
   files via `bkp-restore`, and writes them to the chosen destination,
5. checkpoints progress so an interrupted restore can resume.

Bandwidth limiting wraps the backend in `RateLimitedBackend`, exactly as
the main daemon does.

## Read-only discipline

`bkp-storage` exposes only the read methods: `get`, `get_range`, `list`,
`exists`, `head`, `get_critical`, and the archive pre-warm probes.
Wrappers (`RetryBackend` etc.) forward only these.

## Security model

- The master key exists only in process memory; `MasterKey` / `SubKey`
  zeroize on drop.  It is never persisted (R-2.2).
- No OS-keyring access at all - the recovery tool is not a credential
  store and must not be a harvesting target.
- Credentials entered in the UI are held in memory for the session.  The
  recently-used cache may also persist the storage secret (access key /
  password / OAuth refresh token) in a permission-restricted (ACL'd) file
  for one-click reconnect - but it never persists the master key, which is
  always re-entered.
- Decryption is standard AES-256-GCM with HKDF-SHA256 subkey derivation
  over the loaded master key, matching the format spec.
- **OAuth wind-down escape hatch.**  The Google Drive / OneDrive / Dropbox
  backends authenticate with an OAuth app whose client id/secret is baked in at
  build time, but that is only the default.  The effective credential resolves
  in this order: the per-endpoint config field, then the runtime environment
  variable (`GOOGLE_OAUTH_CLIENT_ID` / `_SECRET`, `DROPBOX_APP_KEY` / `_SECRET`,
  `ONEDRIVE_OAUTH_CLIENT_ID`), then the compiled-in value.  So if the project's
  OAuth apps ever stop working, a user (or delegate) can register their own app
  and point the tool at it - covering both the interactive re-authorization
  (`bkp-recover`) and the token refresh at download time (`bkp-storage`) - with
  no rebuild.  The data is in the user's own cloud account, so a fresh app with
  read scope reaches the same backups.  The non-OAuth backends (S3, B2, Azure,
  GCS, SFTP, SMB, WebDAV, local) use credentials the user holds directly and
  have no such dependency.

## Format parity (mandatory guard)

The format-defining crates - `bkp-types`, `bkp-crypto`, `bkp-manifest`,
`bkp-chunker`, and the storage object-layout in `bkp-storage` - must stay
byte-compatible with the main app's output.  The planned guard against
drift is a conformance fixture (a real pack + manifest + snapshot-index
committed as test data, decoded and verified); it is a TODO, tracked
alongside the reference test vectors (`DATA_FORMAT.md` Section 11) and not
yet committed.  When stripping dead code from
these crates, remove unused functions / methods / backends / write paths
only - never serialized struct fields or CBOR enum variants, because CBOR
deserialization must match the full schema even for fields the read path
ignores.

## Crypto, TLS, and state

- `bkp-crypto` decrypts and verifies through the RustCrypto crates
  (aes-gcm, sha2, hmac, hkdf, pbkdf2, argon2).  Five known-answer tests
  guard conformance with the format's published algorithms, which are
  byte-identical across any conformant implementation.
- HTTP backends use native-tls: SChannel on Windows, Secure Transport on
  macOS, OpenSSL on Linux.  The object-store and Azure backends use a
  ring-backed rustls.
- The SFTP backend (libssh2 via `ssh2`) takes its crypto from the platform:
  WinCNG on Windows, vendored OpenSSL on Unix.  Windows deliberately uses
  WinCNG (libssh2-sys's default) and links no OpenSSL - OpenSSL has no
  `aarch64-pc-windows-gnullvm` build target, so vendoring it would break the
  Windows ARM64 cross-build.  The OpenSSL features are gated to non-Windows
  in `bkp-storage`'s `Cargo.toml`.
- Restore checkpoints are flat JSON files under the per-user data
  directory.

## Build and packaging

`scripts/set_version.sh` propagates `VERSION` to `Cargo.toml` and the
recover UI `package.json`.  Per-platform compile+stage scripts
(`scripts/windows/build_windows_{x86_64,arm64}.sh`,
`scripts/linux/build_linux_{x86_64,arm64}.sh`) build only `nyx_bkp_recover`
and its UI, then the installer scripts (`build_recover_msi_*.sh` /
`build_recover_deb_*.sh` / `build_recover_rpm.sh` / `build_recover_pkg_*.sh`)
wrap the staged binary.  Windows and Linux ship in both **x86-64 and
ARM64**, and macOS ships a single **universal** `.pkg`; the `.rpm` is
repackaged from the `.deb` with fpm.  The Windows x86-64 link requires the
llvm-mingw toolchain + the `__emutls_get_address` keep-flag configured in
`.cargo/config.toml`; Windows ARM64 uses the llvm-mingw aarch64 tools (no
emutls flag - gnullvm uses compiler-rt + native TLS).  Cross-building the
ARM64 installers from an x86-64 host - including the arm64 multiarch dev
libraries and the wixl ARM64 patch - is covered by
`scripts/dev/setup_arm64_buildhost.sh` and documented in
`docs/BUILD_ARM64.md`.

OAuth client credentials are injected at build time from `.env` (see
`.env.example`); `.env` is never committed.

## Distribution and trust chain

The tool is distributed so a user under duress can obtain it and prove it is
genuine, using three roles played by three surfaces:

- **Source** lives on GitHub at `github.com/nyxbackup/nyxbackup-recovery`
  (Apache-2.0, no shared history with the proprietary app) for public audit and
  self-build.
- **Installers** live on `downloads.nyxbackup.com`.  The web hub at
  `nyxbackup.com/recovery` - the one URL the app's About screen opens - links to
  both the source repo and the downloads.
- **Releases** publish a GitHub Release for the current `VERSION` with every
  installer plus `SHA256SUMS-<ver>.txt`.  One **detached GPG signature** over
  that manifest (`.asc`) is the format-agnostic trust anchor - it covers every
  installer at once, sidestepping the fact that `dpkg`/`apt` do not verify
  per-file `.deb` signatures.  RPMs may additionally be `rpm --addsign`ed
  (before the checksums are computed, since signing rewrites the file) for
  native `dnf` / `rpm -K` verification.  The signing **public key** is served
  from the `nyxbackup.com` apex over HTTPS (and attached to the release), so the
  key a downloader verifies with comes from a domain the project controls, not
  only from the same host as the artifacts.  Verification:

  ```bash
  gpg --verify SHA256SUMS-<ver>.txt.asc SHA256SUMS-<ver>.txt
  sha256sum -c SHA256SUMS-<ver>.txt
  ```

  `downloads.nyxbackup.com` may host the files directly or redirect to the
  GitHub Release assets; the checksum + signature validate either way.  Windows
  MSIs and the macOS PKG carry their platform-native signatures (Authenticode /
  Apple notarization) separately - GPG is the cross-platform layer over the
  checksum manifest.
