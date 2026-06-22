#!/usr/bin/env bash
# Copyright (c) 2026 Nyx Software, LLC
# SPDX-License-Identifier: Apache-2.0
# Nyx Backup Recovery - https://nyxbackup.com
# Repackage the standalone Nyx Backup Recovery .deb into an .rpm.
#
# This converts an already-built .deb (from build_recover_deb_x86_64.sh or
# build_recover_deb_arm64.sh) straight into an .rpm with fpm, so the RPM
# carries the exact same staged binary, desktop file, and layout as the .deb -
# one source of truth, no second packaging path to drift.
#
# Dependency naming note:
#   The .deb declares Debian library names (libwebkit2gtk-4.1-0, libgtk-3-0,
#   ...) that do NOT exist on RPM distros, and there is no single RPM name set
#   that is correct across Fedora/RHEL, openSUSE, and Mageia.  Rather than bake
#   in wrong Requires that would block `dnf install`, this builds a
#   dependency-free RPM (like `alien` does) that installs on any RPM distro.
#   The recovery binary statically links libssh2; the only runtime requirement
#   is a desktop WebKitGTK 4.1 / GTK 3 stack, which RPM desktops already carry:
#     Fedora/RHEL:  webkit2gtk4.1 gtk3 librsvg2 libappindicator-gtk3
#     openSUSE:     libwebkit2gtk-4_1-0 gtk3 librsvg-2-2 libayatana-appindicator3-1
#   To declare hard Requires for a specific distro, add `-d name` flags to the
#   fpm invocation below (and drop --no-auto-depends).
#
# Requirements (Ubuntu/Debian host):
#   sudo gem install fpm          # or: sudo apt install ruby-dev && gem install fpm
#   sudo apt install rpm zstd     # rpmbuild backend + zstd to read the .deb
#
# Output: dist/NyxBackup-Recovery-VERSION-{x86_64,aarch64}.rpm
#
# Usage:
#   scripts/linux/build_recover_rpm.sh                 # x86_64 from the amd64 deb
#   scripts/linux/build_recover_rpm.sh --arch arm64    # aarch64 from the arm64 deb
#   scripts/linux/build_recover_rpm.sh --arch arm64 --build   # build the deb first

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_DIR="$(cd "${SCRIPT_DIR}/../.." && pwd)"
DIST="${WORKSPACE_DIR}/dist"

DEB_ARCH="amd64"
DO_BUILD=0
while [[ $# -gt 0 ]]; do
    case "$1" in
        --arch) DEB_ARCH="${2:?--arch needs a value (amd64|arm64)}"; shift 2 ;;
        --build) DO_BUILD=1; shift ;;
        *) echo "Unknown argument: $1"; exit 1 ;;
    esac
done

# Map the Debian architecture to its RPM equivalent (used for the output name;
# fpm itself sets the rpm's internal arch tag from the deb).
case "$DEB_ARCH" in
    amd64) RPM_ARCH="x86_64"; DEB_SCRIPT="build_recover_deb_x86_64.sh" ;;
    arm64) RPM_ARCH="aarch64"; DEB_SCRIPT="build_recover_deb_arm64.sh" ;;
    *) echo "ERROR: unsupported --arch '$DEB_ARCH' (expected amd64 or arm64)"; exit 1 ;;
esac

VERSION=$(tr -d '[:space:]' < "${WORKSPACE_DIR}/VERSION")
echo "Building Recovery .rpm (${RPM_ARCH}) for Nyx Backup v${VERSION}..."

# -- Preflight checks --------------------------------------------------------
check_cmd() { command -v "$1" >/dev/null 2>&1 || { echo "ERROR: $1 not found. $2"; exit 1; }; }
check_cmd fpm      "Install: sudo gem install fpm"
check_cmd rpmbuild "Install: sudo apt install rpm"
# fpm reads the .deb's zstd-compressed control/data members (dpkg-deb defaults
# to zstd on this host), so zstd must be available to unpack them.
check_cmd zstd     "Install: sudo apt install zstd"

DEB="${DIST}/NyxBackup-Recovery-${VERSION}-${DEB_ARCH}.deb"

if [[ $DO_BUILD -eq 1 ]]; then
    bash "${SCRIPT_DIR}/${DEB_SCRIPT}" --build
fi

[[ -f "$DEB" ]] || {
    echo "ERROR: $DEB not found."
    echo "  Build it first: scripts/linux/${DEB_SCRIPT} --build"
    echo "  (or pass --build to chain it)."
    exit 1
}

mkdir -p "$DIST"
OUTPUT="${DIST}/NyxBackup-Recovery-${VERSION}-${RPM_ARCH}.rpm"

# --no-auto-depends drops the deb's Debian-named Requires (see header); -f
# overwrites a stale rpm from a previous run.
fpm -s deb -t rpm \
    --no-auto-depends \
    -f \
    -p "$OUTPUT" \
    "$DEB"

SIZE=$(du -h "$OUTPUT" | cut -f1)
echo ""
echo "Recovery .rpm created: $OUTPUT  ($SIZE)"
echo ""
echo "Install:  sudo dnf install ./NyxBackup-Recovery-${VERSION}-${RPM_ARCH}.rpm"
echo "  (or: sudo rpm -i ...).  Needs a desktop WebKitGTK 4.1 / GTK 3 stack."
