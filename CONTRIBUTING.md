# Contributing to Nyx Backup Recovery

Thank you for looking. This repository exists to be read as much as to be run:
it is the tool that reads a Nyx Backup archive when the application that wrote
it is gone, so its value depends on being independently auditable.

Bug reports and format-correctness fixes are especially welcome.

## What this project is

A standalone, Apache-2.0 restore tool. It reads the published on-disk format
directly and does not talk to the Nyx Backup service, the licence server, or
anything else. It has no telemetry and requires no account.

It is deliberately **not** a backup client: there is no scheduling, no upload
path, no retention. Restore only. Changes that add write-side features are out
of scope, and will be declined however well written - the smaller this codebase
stays, the easier it is to trust.

## Reporting a problem

The most valuable report is one where a real archive cannot be restored. Please
include:

- the version of the Recovery Tool (Help > About, or `--version`),
- the version of Nyx Backup that wrote the archive, if known,
- the storage backend (S3, B2, R2, SFTP, local disk, ...),
- what you expected and what happened, with the exact error text.

**Never attach credentials, an access key, or a recovery passphrase**, and note
that a manifest or snapshot index is encrypted but its object *names* can reveal
machine and set identifiers. Redact anything you would not publish.

If you believe you have found a vulnerability, do not open a public issue - see
[SECURITY.md](SECURITY.md) if present, or email security@nyxbackup.com.

## Building

Prerequisites: the Rust toolchain (pinned in `rust-toolchain.toml`; `rustup`
installs it automatically), Node 22+ for the frontend, and on Linux the
WebKitGTK development packages:

```bash
sudo apt install libwebkit2gtk-4.1-dev libgtk-3-dev \
                 libayatana-appindicator3-dev librsvg2-dev

cd crates/bkp-recover/ui && npm ci && npm run build && cd -
cargo build --release
```

`README.md` covers packaging and the platform installers.

## Before opening a pull request

CI runs these, so running them first saves a round trip:

```bash
cargo fmt --all --check
cargo clippy --workspace --exclude bkp-recover --all-targets -- -D warnings
cargo test  --workspace --exclude bkp-recover

# and, if you touched the Tauri app (needs the system libraries above):
cargo clippy -p bkp-recover --all-targets -- -D warnings
cargo test   -p bkp-recover
```

Warnings are denied. That is deliberate: a warning nobody fixes becomes a
warning nobody reads.

## Conventions

- **ASCII only** in source, comments and commit messages. No smart quotes or
  em dashes.
- Spaces, never tabs; `cargo fmt` settles the rest.
- Doc comments (`///`, `//!`) on public items.
- Commit messages: a `type(scope): summary` first line, then *why* the change is
  right, not a restatement of the diff. If a change fixes something subtle,
  the commit message is where the next person finds out how it was found.

## Changing anything the format depends on

`bkp-types`, `bkp-crypto`, `bkp-manifest` and `bkp-chunker` implement the
published on-disk format. A change there can render existing archives
unreadable, which is the one failure this tool exists to prevent.

- The format specification is `docs/DATA_FORMAT.md` in the main Nyx Backup
  repository, published at <https://nyxbackup.com/format>. It is the authority;
  this code should agree with it, and a disagreement is a bug in one of them.
- Format versions are append-only: a newer reader must keep reading older
  archives. Breaking that requires a major version bump and a specification
  change, not a patch here.
- Be more permissive than the writer where it is safe. The tool already accepts
  several historic pack layouts, because a recovery tool that rejects an old
  archive on a technicality has failed at its only job.

If you are unsure whether a change is format-affecting, open an issue and ask
before writing it.

## Licence

By contributing you agree that your contribution is licensed under the Apache
License 2.0, as in [LICENSE](LICENSE).
