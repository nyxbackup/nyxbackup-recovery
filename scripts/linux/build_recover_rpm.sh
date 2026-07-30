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
#   ...) that do NOT exist on RPM distros, and no single RPM package-name set
#   is correct across Fedora/RHEL (webkit2gtk4.1), openSUSE
#   (libwebkit2gtk-4_1-0) and Mageia.  So --no-auto-depends drops the Debian
#   names, and the Requires are declared as SONAMEs instead: rpm auto-generates
#   a Provides for every soname a package ships, so `libwebkit2gtk-4.1.so.0()
#   (64bit)` resolves on all of them regardless of what the package is called.
#   That is also exactly what rpmbuild's own ELF dependency generator would
#   emit if this were built from a spec rather than converted from a .deb.
#
#   Getting a soname string wrong makes the RPM uninstallable EVERYWHERE, which
#   is worse than declaring nothing - so this list is derived from the staged
#   binary's DT_NEEDED entries, and RPM_REQUIRES can be overridden (or set
#   empty) without editing the script.  Verify after changing it with:
#     rpm -qpR dist/NyxBackup-Recovery-*.rpm
#     dnf install --assumeno ./dist/NyxBackup-Recovery-*.rpm   # on an RPM box
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

# SONAME Requires (see header).  The (64bit) marker is what rpm appends to
# ELF64 Provides on both x86_64 and aarch64, so the same list serves both -
# including the GLIBC_2.34 floor, which exists as a symbol version on both
# architectures.  Note the arm64 .deb is gated at 2.35 rather than 2.34
# (see build_recover_deb_arm64.sh); the floor below is the looser of the two
# and can be raised via RPM_REQUIRES if an arm64 RPM ever needs to match.
# The libc entry carries the glibc floor - the .deb's libc6 (>= 2.34) has no
# meaning to rpm - so `dnf install` on a too-old release (RHEL 8, Leap 15)
# fails with a clear unmet dependency instead of the binary dying at exec.
# librsvg is deliberately absent: it is dlopen'd by gdk-pixbuf for SVG icons,
# not linked, and hard-requiring it blocks installs for a cosmetic loader.
DEFAULT_REQUIRES="
libc.so.6(GLIBC_2.34)(64bit)
libwebkit2gtk-4.1.so.0()(64bit)
libjavascriptcoregtk-4.1.so.0()(64bit)
libgtk-3.so.0()(64bit)
libgdk-3.so.0()(64bit)
libsoup-3.0.so.0()(64bit)
libgio-2.0.so.0()(64bit)
libgobject-2.0.so.0()(64bit)
libglib-2.0.so.0()(64bit)
libgdk_pixbuf-2.0.so.0()(64bit)
libcairo.so.2()(64bit)
libdbus-1.so.3()(64bit)
libz.so.1()(64bit)
"
RPM_REQUIRES="${RPM_REQUIRES-$DEFAULT_REQUIRES}"

DEP_FLAGS=()
DEP_COUNT=0
for dep in $RPM_REQUIRES; do
    DEP_FLAGS+=(-d "$dep")
    DEP_COUNT=$((DEP_COUNT + 1))
done
echo "Declaring ${DEP_COUNT} soname Requires (set RPM_REQUIRES= to disable)."

# --no-auto-depends drops the deb's Debian-named Requires (see header); -f
# overwrites a stale rpm from a previous run.
fpm -s deb -t rpm \
    --no-auto-depends \
    "${DEP_FLAGS[@]}" \
    -f \
    -p "$OUTPUT" \
    "$DEB"

SIZE=$(du -h "$OUTPUT" | cut -f1)
echo ""
echo "Recovery .rpm created: $OUTPUT  ($SIZE)"
echo ""
echo "Install:  sudo dnf install ./NyxBackup-Recovery-${VERSION}-${RPM_ARCH}.rpm"
echo "  (or: sudo rpm -i ...).  Needs a desktop WebKitGTK 4.1 / GTK 3 stack."
