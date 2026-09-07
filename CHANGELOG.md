# Changelog

All notable changes to the Nyx Backup Recovery Tool.

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Versions follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html),
where the public surface is the command-line and GUI behaviour of the tool,
not its internal crates.

The on-disk archive format is versioned separately - currently **format
version 2** - and documented in [`docs/DATA_FORMAT.md`](docs/DATA_FORMAT.md).
A major release of this tool does not imply a format change, and a format
change does not require a major release of this tool.

## [Unreleased]

## [1.0.0] - 2026-09-13

First stable release.

- Reads **format version 2** archives.
- Restores without the main application, without a running service, and
  without a licence.
- **Read-only against your storage.** It downloads; it never writes, deletes
  or modifies. OneDrive is requested with `Files.Read` and Dropbox with a
  read-only scope subset; Google Drive uses the app-private `drive.appdata`
  scope, which only exposes this app's own hidden folder.
- Reed-Solomon repair of a corrupt pack from its `.par` sibling, read-side
  only: the repaired bytes serve the restore in hand and are never written
  back to storage.
- Platforms: Windows x86-64 and ARM64, macOS universal, Linux x86-64 and
  ARM64 (deb and rpm).

Preview builds were published as 0.9.x while the format and interface
settled; they are not itemised here.

## Maintaining this file

- Add to **[Unreleased]** as changes land, not at release time.
- At release, rename `[Unreleased]` to the version with the date and open a
  fresh `[Unreleased]`.
- Write what a user would notice, and state limitations alongside fixes.
- Format changes go in `docs/DATA_FORMAT.md`'s changelog as well, and that
  file is a **mirror** - edit the authoritative copy in the application
  repository and run its `scripts/sync-recovery-spec.sh`.
