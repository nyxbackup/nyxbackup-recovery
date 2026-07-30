#!/usr/bin/env bash
# Copyright (c) 2026 Nyx Software, LLC
# SPDX-License-Identifier: Apache-2.0
# Nyx Backup Recovery - https://nyxbackup.com
# Propagate the version from VERSION to all files that must stay in sync.
#
# Usage:
#   scripts/set_version.sh          # use current VERSION file
#   scripts/set_version.sh 1.1.0    # write new version to VERSION, then propagate
#
# Files updated:
#   VERSION                              (only when a new version is supplied)
#   Cargo.toml                           (workspace version = "...")
#   crates/bkp-recover/ui/package.json   ("version": "...")
#
# Cargo.lock and package-lock.json are regenerated automatically by the next
# cargo build / npm install run, so we do not touch them here.

set -euo pipefail

WORKSPACE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [[ $# -ge 1 ]]; then
    NEW_VER="$1"
    # Basic semver sanity check
    if ! [[ "$NEW_VER" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[A-Za-z0-9._-]+)?$ ]]; then
        echo "ERROR: '$NEW_VER' does not look like a semver string (e.g. 1.2.3)"
        exit 1
    fi
    echo "$NEW_VER" > "${WORKSPACE_DIR}/VERSION"
    echo "VERSION -> ${NEW_VER}"
fi

VER=$(tr -d '[:space:]' < "${WORKSPACE_DIR}/VERSION")
echo "Propagating version ${VER}..."

# Portable in-place sed: BSD sed (macOS) requires a suffix argument after -i;
# GNU sed (Linux) accepts the same form.  Using .bak + rm keeps both happy.
sed_inplace() {
    sed -i.bak "$1" "$2"
    rm -f "$2.bak"
}

# Cargo.toml workspace version field (the [workspace.package] version = "..." line)
sed_inplace "s/^version = \"[^\"]*\"/version = \"${VER}\"/" "${WORKSPACE_DIR}/Cargo.toml"
echo "  Cargo.toml"

# Recovery GUI frontend package.json
PKGJSON="${WORKSPACE_DIR}/crates/bkp-recover/ui/package.json"
sed_inplace "s/\"version\": \"[^\"]*\"/\"version\": \"${VER}\"/" "$PKGJSON"
echo "  crates/bkp-recover/ui/package.json"

# Note: version-stamp fingerprint busting is handled inside the
# per-platform build scripts which detect version drift by inspecting the
# staged binary's embedded version string.  cargo's incremental fingerprint
# does not reliably detect a workspace-inherited version bump, so those
# scripts force-clean version-stamped crates on drift.

echo "Done."
