#!/usr/bin/env bash
# Copyright (c) 2026 Nyx Software, LLC. All rights reserved.
# Nyx Backup — https://nyxbackup.com
#
# Sign a .rpm package with a GPG key using rpmsign.  Idempotent: re-signing
# replaces the existing header signature (rpmsign --addsign is a resign).
#
# The SAME GPG key signs both .deb (debsigs, sign_deb.sh) and .rpm - one
# RSA-4096 sign-only key for all Linux packages.
#
# Requirements:
#   sudo apt install rpm        (provides rpmsign / rpm)
#   gpg with the secret key in the keyring; gpg-agent available for a
#   passphrase-protected key (modern rpm shells out to gpg2).
#
# Usage:
#   scripts/linux/sign_rpm.sh <path/to/file.rpm> <gpg-key-id>
#
# `gpg-key-id` may be the short ID, long ID, fingerprint, or email of any
# secret key in the local GPG keyring.
#
# Production usage:
#   Set NYX_SIGN_KEY_ID in the build environment and build_rpm_x86_64.sh /
#   build_rpm_arm64.sh invoke this script automatically after producing the
#   .rpm (mirrors the .deb / sign_deb.sh hook).
#
# Users verify with:
#   gpg --export -a <key-id> | sudo rpm --import /dev/stdin   # once, to trust
#   rpm --checksig file.rpm                                   # "... signatures OK"

set -euo pipefail

if [[ $# -ne 2 ]]; then
    echo "Usage: $0 <path/to/file.rpm> <gpg-key-id>" >&2
    exit 2
fi

RPM_FILE="$1"
KEY_ID="$2"

if [[ ! -f "$RPM_FILE" ]]; then
    echo "ERROR: not a file: $RPM_FILE" >&2
    exit 2
fi

if ! command -v rpmsign >/dev/null 2>&1; then
    echo "ERROR: rpmsign not installed.  Install with: sudo apt install rpm" >&2
    exit 3
fi

if ! gpg --list-secret-keys "$KEY_ID" >/dev/null 2>&1; then
    echo "ERROR: no secret key matching '$KEY_ID' in the local GPG keyring." >&2
    echo "       Available secret keys:" >&2
    gpg --list-secret-keys --keyid-format LONG | sed 's/^/         /' >&2
    exit 3
fi

# `_gpg_name` tells rpm which key to sign with; passing it via --define avoids
# mutating the operator's ~/.rpmmacros.  rpmsign uses SHA-256 header+payload
# signatures by default on modern rpm (V4).
#
# `__gpg` override: rpm (a Fedora tool) defaults its gpg binary to
# /usr/bin/gpg2, which does not exist on Debian/Ubuntu (only /usr/bin/gpg) -
# without this, rpmsign fails with "Could not exec gpg".  Point it at whatever
# gpg is actually on PATH so the script works on both Debian- and RPM-family
# build hosts.
GPG_BIN="$(command -v gpg)"
echo "Signing $(basename "$RPM_FILE") with key $KEY_ID..."
rpmsign --addsign \
    --define "_gpg_name $KEY_ID" \
    --define "__gpg $GPG_BIN" \
    "$RPM_FILE"

# Confirm the signature header is present without needing the pubkey imported
# into rpm's own keyring (that would be required for a full --checksig "OK").
# Read the same "Signature" field `rpm -qpi` shows: a header+payload RSA sign
# populates the RSAHEADER tag (not SIGPGP/SIGGPG), so query -qpi rather than a
# single tag to stay backend-agnostic.
SIG=$(rpm -qpi "$RPM_FILE" 2>/dev/null | awk -F': ' '/^Signature/{print $2; exit}')
if [[ -n "$SIG" && "$SIG" != "(none)" ]]; then
    echo "  -> embedded signature: $SIG"
else
    echo "ERROR: signature was not embedded.  Check rpmsign output above." >&2
    echo "       (A passphrase-protected key needs gpg-agent / pinentry reachable.)" >&2
    exit 4
fi
