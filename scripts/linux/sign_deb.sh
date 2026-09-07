#!/usr/bin/env bash
# Copyright (c) 2026 Nyx Software, LLC. All rights reserved.
# Nyx Backup — https://nyxbackup.com
#
# Sign a .deb package with a GPG key using debsigs.  Idempotent: a .deb
# can be re-signed (the old signature is replaced).
#
# Requirements:
#   sudo apt install debsigs
#
# Usage:
#   scripts/linux/sign_deb.sh <path/to/file.deb> <gpg-key-id>
#
# `gpg-key-id` may be the short ID, long ID, fingerprint, or email of any
# secret key in the local GPG keyring.
#
# Production usage:
#   The release machine holds the production secret key (id from
#   the Nyx Backup PGP rotation log).  Set NYX_SIGN_KEY_ID in the build
#   environment and `build_deb_x86_64.sh` invokes this script
#   automatically after producing the .deb.
#
# Dev / verification usage:
#   A local test key (8F6CBC59730BA86264347A042401849480519258) was
#   generated 2026-06-06 for pipeline testing only.  NEVER ship a
#   .deb signed with the test key to users.

set -euo pipefail

if [[ $# -ne 2 ]]; then
    echo "Usage: $0 <path/to/file.deb> <gpg-key-id>" >&2
    exit 2
fi

DEB_FILE="$1"
KEY_ID="$2"

if [[ ! -f "$DEB_FILE" ]]; then
    echo "ERROR: not a file: $DEB_FILE" >&2
    exit 2
fi

if ! command -v debsigs >/dev/null 2>&1; then
    echo "ERROR: debsigs not installed.  Install with: sudo apt install debsigs" >&2
    exit 3
fi

if ! gpg --list-secret-keys "$KEY_ID" >/dev/null 2>&1; then
    echo "ERROR: no secret key matching '$KEY_ID' in the local GPG keyring." >&2
    echo "       Available secret keys:" >&2
    gpg --list-secret-keys --keyid-format LONG | sed 's/^/         /' >&2
    exit 3
fi

# `origin` signature type is the canonical "publisher signed this package"
# verifier.  Alternative types (maint, archive) exist for repo / repository
# operator signatures; we only do origin signing for now.
echo "Signing $(basename "$DEB_FILE") with key $KEY_ID..."
debsigs --sign=origin --default-key="$KEY_ID" "$DEB_FILE"

# Confirm the signature landed.  `debsigs --check` would do remote-trust
# verification (requires a debsig-verify policy file), which is overkill for
# a local sign-and-verify; `ar` listing shows the _gpgorigin member.
if ar t "$DEB_FILE" | grep -q '^_gpgorigin$'; then
    echo "  -> embedded signature: _gpgorigin ($(ar p "$DEB_FILE" _gpgorigin | wc -c) bytes)"
else
    echo "ERROR: signature was not embedded.  Check debsigs output above." >&2
    exit 4
fi
