# Security Policy

## Reporting a vulnerability

If you discover a security vulnerability in Nyx Backup Recovery, please
report it privately - do NOT open a public issue.

- Email: **security@nyxbackup.com**
- Please include: a description, affected version (see Help -> About or
  the `VERSION` file), reproduction steps, and impact assessment.
- We aim to acknowledge reports within 5 business days and to provide a
  remediation timeline after triage.

Please give us a reasonable window to remediate before public
disclosure.  We will credit reporters who wish to be named once a fix is
released.

## Scope

This repository is the **recovery tool only** - it reads, decrypts, and
restores Nyx Backup data.  Issues in the main Nyx Backup application,
service/daemon, or backend infrastructure are out of scope here; report
those to the same address noting they concern the main product.

Of particular interest for this tool:

- Anything that could cause the master key or storage credentials to be
  written to disk, logged, or transmitted (the tool's design guarantee
  is that the master key lives only in memory and is never persisted).
- Decryption / integrity flaws that could yield incorrect or
  attacker-controlled restored data.
- Path-handling issues in the cross-platform restore (e.g. a malicious
  manifest path escaping the chosen destination directory).

## Verifying a build

Official release artifacts are published with SHA-256 checksums.  Because
the source is open (Apache-2.0), you can also build the tool yourself and
compare behavior.  See `README.md` for build instructions.
