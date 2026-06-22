# Nyx Backup Recovery

A standalone, open-source disaster-recovery tool for
[Nyx Backup](https://nyxbackup.com).  It reads, decrypts, and restores
files from any Nyx Backup endpoint **without** the main application, a
running service, or a license.

If you have your storage credentials and your 32-byte master encryption
key, this tool can get your data back - forever, from source you can
read and build yourself.  That is the point: your backups are never
hostage to one vendor's binary.

## What it does

- Connects directly to a backup endpoint (S3 / S3-compatible, Backblaze
  B2, Azure Blob, Google Cloud Storage, SFTP, SMB/CIFS, WebDAV, local
  disk, and the OAuth providers Google Drive / OneDrive / Dropbox).
- Unlocks with the master key, browses snapshots, and restores selected
  files or whole folders.
- Read-only: it never writes to, deletes from, or modifies your backup
  storage.  It only downloads.

## Cold-storage (Glacier / Archive) backups

If your backup lives in a cold/archive tier - **S3 Glacier / Glacier Deep
Archive, Azure Archive, or GCS Archive** - the objects cannot be downloaded
until they are retrieved (thawed).  This tool does **not** thaw them for you:
start the retrieval with your storage provider (its console or CLI) or the main
Nyx Backup app, wait for it to complete (minutes to many hours depending on the
tier - Deep Archive can take up to ~48 h), and note that providers usually
charge a retrieval fee.  Then run the restore normally.  If you start a restore
before the data is thawed, the tool tells you the backup is in cold storage
rather than failing with an opaque error.

## What it is NOT

This is the **restore** path only.  It does not create backups, schedule
them, or run as a service - that is the main Nyx Backup application
(separate, proprietary).  This repository is intentionally minimal: it
contains only the crates required to read and restore.

## Where to get it

- **Source code:** <https://github.com/nyxbackup/nyxbackup-recovery> - read,
  audit, and build it yourself.
- **Installers:** the download hub at <https://nyxbackup.com/recovery> links to
  the signed installers on `downloads.nyxbackup.com`.
- **Verify what you download:** every release ships `SHA256SUMS-<ver>.txt`;
  signed releases also include a detached GPG signature
  `SHA256SUMS-<ver>.txt.asc`, with the signing public key served from
  `nyxbackup.com` (HTTPS) so the key you verify with comes from a domain the
  project controls.

## Installing

Grab the installer for your OS and CPU, **verify the checksum** (and the GPG
signature, when present), then install.  Full instructions (all platforms,
uninstall, WSL notes) are in [`docs/INSTALL.md`](docs/INSTALL.md).

```bash
# Verify first (run where the installer + sums file live)
gpg --verify SHA256SUMS-<ver>.txt.asc SHA256SUMS-<ver>.txt   # authenticity (if signed)
sha256sum -c SHA256SUMS-<ver>.txt                            # integrity

# Linux DEB  - use apt so dependencies resolve (NOT dpkg -i)
sudo apt install ./NyxBackup-Recovery-<ver>-amd64.deb      # or -arm64.deb
# Linux RPM
sudo dnf install ./NyxBackup-Recovery-<ver>-x86_64.rpm     # or -aarch64.rpm
# Windows: double-click the .msi, or  msiexec /i NyxBackup-Recovery-<ver>-x86_64.msi
# macOS:   open the .pkg
```

On a HiDPI display you can scale the Linux UI with GTK's `GDK_DPI_SCALE`,
e.g. `GDK_DPI_SCALE=1.2 nyx_bkp_recover`.

## Building

Prerequisites: a recent stable Rust toolchain (see `rust-toolchain.toml`)
and Node.js 22+ for the GUI frontend.

```bash
# Library crates (the read/restore engine)
cargo build --release --workspace --exclude bkp-recover

# GUI frontend bundle
cd crates/bkp-recover/ui && npm ci && npm run build && cd -

# GUI application
cargo build --release -p bkp-recover --bin nyx_bkp_recover
```

Distributable installers are produced per platform by the scripts under
`scripts/` - Windows `.msi` and Linux `.deb` / `.rpm` in both x86-64 and
ARM64, plus a macOS universal `.pkg` (Intel + Apple Silicon).  Cross-building
the ARM64 installers from an x86-64 host has extra host setup; see
`docs/BUILD_ARM64.md`.

## License

Apache-2.0 - see [LICENSE](LICENSE).  The main Nyx Backup application is a
separate, proprietary product; this recovery tool is deliberately
released under a permissive license so anyone can audit, build, and rely
on it.
