#!/usr/bin/env bash
# Copyright (c) 2026 Nyx Software, LLC
# SPDX-License-Identifier: Apache-2.0
# Nyx Backup Recovery - https://nyxbackup.com
# Package the standalone Nyx Backup Recovery ARM64 binary into a .deb.
#
# Assumes scripts/linux/build_linux_arm64.sh has been run so that
# staging/linux/arm64/nyx_bkp_recover exists.  Pass --build to chain
# a fresh compile first.
#
# Output: dist/NyxBackup-Recovery-VERSION-arm64.deb
#
# Install layout:
#   /usr/lib/nyxbackup-recovery/nyx_bkp_recover
#   /usr/bin/nyx_bkp_recover  (symlink)
#   /usr/share/applications/nyx-backup-recovery.desktop

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_DIR="$(cd "${SCRIPT_DIR}/../.." && pwd)"
STAGING="${WORKSPACE_DIR}/staging/linux/arm64"
DIST="${WORKSPACE_DIR}/dist"
DO_BUILD=0

for arg in "$@"; do
    case "$arg" in
        --build) DO_BUILD=1 ;;
        *) echo "Unknown argument: $arg"; exit 1 ;;
    esac
done

VERSION=$(tr -d '[:space:]' < "${WORKSPACE_DIR}/VERSION")
echo "Building Recovery .deb (arm64) for Nyx Backup v${VERSION}..."

command -v dpkg-deb >/dev/null 2>&1 || {
    echo "ERROR: dpkg-deb not found.  Install dpkg: sudo apt install dpkg"
    exit 1
}

if [[ $DO_BUILD -eq 1 ]]; then
    bash "${WORKSPACE_DIR}/scripts/linux/build_linux_arm64.sh"
fi

[[ -f "$STAGING/nyx_bkp_recover" ]] || {
    echo "ERROR: $STAGING/nyx_bkp_recover not found."
    echo "  Run with --build, or run scripts/linux/build_linux_arm64.sh first."
    exit 1
}

# -- glibc ABI ceiling -------------------------------------------------------
# A native (or cross-native) build stamps in the glibc symbol versions of the
# libs it linked against, so building against 24.04 arm64 libs produces a
# binary that dies on older distros with "version `GLIBC_2.39' not found".
#
# The ceiling is 2.35 here, not the 2.34 used for x86_64: 2.34 exists on
# x86_64 only to protect the RHEL 9 family, which cannot run this app at all
# (no webkit2gtk-4.1 - see docs/LINUX_COMPAT.md).  The real arm64 floor is
# Ubuntu 22.04 arm64 / glibc 2.35, the oldest arm64 base shipping WebKitGTK
# 4.1.  Override with MAX_GLIBC=x.y when the support floor moves.
MAX_GLIBC="${MAX_GLIBC:-2.35}"

# The host objdump on an x86-64 build host is usually single-target and cannot
# read an aarch64 ELF, so prefer the cross binutils when they are installed.
OBJDUMP=""
for candidate in aarch64-linux-gnu-objdump objdump; do
    command -v "$candidate" >/dev/null 2>&1 || continue
    if "$candidate" -T "$STAGING/nyx_bkp_recover" >/dev/null 2>&1; then
        OBJDUMP="$candidate"
        break
    fi
done

if [[ -n "$OBJDUMP" ]]; then
    TOO_NEW=$("$OBJDUMP" -T "$STAGING/nyx_bkp_recover" \
              | grep -oE 'GLIBC_[0-9]+\.[0-9]+' | sort -u -V | tail -1)
    TOO_NEW="${TOO_NEW#GLIBC_}"
    HIGHEST=$(printf '%s\n%s\n' "$MAX_GLIBC" "$TOO_NEW" | sort -V | tail -1)
    if [[ "$HIGHEST" != "$MAX_GLIBC" ]]; then
        echo "ERROR: binary requires GLIBC_${TOO_NEW}, ceiling is GLIBC_${MAX_GLIBC}."
        echo "  Offending symbols:"
        "$OBJDUMP" -T "$STAGING/nyx_bkp_recover" \
            | grep -E "GLIBC_(2\.(3[6-9]|[4-9][0-9])|[3-9]\.)" \
            | awk '{print "    " $NF " (" $(NF-1) ")"}' | sort -u
        echo "  Rebuild against Ubuntu 22.04 arm64 libs - see the header of"
        echo "  scripts/linux/build_linux_arm64.sh."
        exit 1
    fi
    echo "glibc ABI check: requires at most GLIBC_${TOO_NEW} (ceiling ${MAX_GLIBC}) - OK"
    DEB_GLIBC="$TOO_NEW"
else
    echo "WARNING: no objdump able to read an aarch64 ELF - skipping the glibc"
    echo "  ABI ceiling check.  Install binutils-aarch64-linux-gnu to enable it."
    DEB_GLIBC="$MAX_GLIBC"
fi

PKG_ROOT="${WORKSPACE_DIR}/target/deb-recovery-root-arm64"
PKG_NAME="nyxbackup-recovery"
ARCH="arm64"

rm -rf "$PKG_ROOT"
mkdir -p "${PKG_ROOT}/DEBIAN"
mkdir -p "${PKG_ROOT}/usr/lib/nyxbackup-recovery"
mkdir -p "${PKG_ROOT}/usr/bin"
mkdir -p "${PKG_ROOT}/usr/share/applications"

install -m 0755 "$STAGING/nyx_bkp_recover" "${PKG_ROOT}/usr/lib/nyxbackup-recovery/"
ln -s /usr/lib/nyxbackup-recovery/nyx_bkp_recover "${PKG_ROOT}/usr/bin/nyx_bkp_recover"

cat > "${PKG_ROOT}/usr/share/applications/nyx-backup-recovery.desktop" <<EOF
[Desktop Entry]
Name=Nyx Backup Recovery
Comment=Restore from a Nyx Backup snapshot
Exec=/usr/bin/nyx_bkp_recover
Terminal=false
Type=Application
Categories=Utility;Archiving;
StartupNotify=true
EOF

# Strip the binary to bring the .deb down.  Use the aarch64 cross strip when
# present (host `strip` cannot process arm64 objects on an x86-64 build host);
# fall back to native strip on an arm64 host, and skip if neither is available.
RECOVER_BIN="${PKG_ROOT}/usr/lib/nyxbackup-recovery/nyx_bkp_recover"
if command -v aarch64-linux-gnu-strip >/dev/null 2>&1; then
    aarch64-linux-gnu-strip "$RECOVER_BIN" || true
elif [[ "$(uname -m)" == "aarch64" || "$(uname -m)" == "arm64" ]] && command -v strip >/dev/null 2>&1; then
    strip "$RECOVER_BIN" || true
fi

SIZE_KB=$(du -sk "${PKG_ROOT}/usr" | cut -f1)

# Depends mirrors build_recover_deb_x86_64.sh - derived from the binary's
# DT_NEEDED list, not the build-time dev packages.  Nothing links
# libayatana-appindicator (no tray icon) or librsvg (dlopen'd by gdk-pixbuf for
# SVG icons), so neither is a hard dependency.  The `a | b` alternatives cover
# the 64-bit time_t package rename, and libc6 is version-pinned so apt reports
# an unmet dependency instead of the binary dying at exec.
cat > "${PKG_ROOT}/DEBIAN/control" <<EOF
Package: ${PKG_NAME}
Version: ${VERSION}
Section: utils
Priority: optional
Architecture: ${ARCH}
Installed-Size: ${SIZE_KB}
Depends: libc6 (>= ${DEB_GLIBC}), libgcc-s1, zlib1g,
 libwebkit2gtk-4.1-0, libjavascriptcoregtk-4.1-0,
 libgtk-3-0 | libgtk-3-0t64, libglib2.0-0 | libglib2.0-0t64,
 libgdk-pixbuf-2.0-0 | libgdk-pixbuf-2.0-0t64, libcairo2,
 libsoup-3.0-0 | libsoup-3.0-0t64, libdbus-1-3
Recommends: librsvg2-2, librsvg2-common
Maintainer: Nyx Backup <support@nyxbackup.com>
Homepage: https://nyxbackup.com
Description: Nyx Backup Recovery - standalone disaster recovery tool
 Connects directly to a Nyx Backup remote (S3, Azure, B2, GCS, SFTP,
 SMB, Google Drive, OneDrive, Dropbox) and restores snapshots without
 needing the main Nyx Backup service installed.  Useful for "the
 backup machine is gone" scenarios.
EOF

mkdir -p "$DIST"
OUTPUT="${DIST}/NyxBackup-Recovery-${VERSION}-${ARCH}.deb"
dpkg-deb --build --root-owner-group "$PKG_ROOT" "$OUTPUT"
rm -rf "$PKG_ROOT"

SIZE=$(du -h "$OUTPUT" | cut -f1)
echo ""
echo "Recovery .deb created: $OUTPUT  ($SIZE)"
echo ""
echo "Install:  sudo apt install ./NyxBackup-Recovery-${VERSION}-${ARCH}.deb"
