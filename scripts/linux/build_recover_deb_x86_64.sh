#!/usr/bin/env bash
# Copyright (c) 2026 Nyx Software, LLC
# SPDX-License-Identifier: Apache-2.0
# Nyx Backup Recovery - https://nyxbackup.com
# Package the standalone Nyx Backup Recovery binary into a .deb.
#
# Assumes scripts/linux/build_linux_x86_64.sh has been run so that
# staging/linux/x86_64/nyx_bkp_recover exists.  Pass --build to chain
# a fresh compile first.
#
# Output: dist/NyxBackup-Recovery-VERSION-amd64.deb
#
# Install layout:
#   /usr/lib/nyxbackup-recovery/nyx_bkp_recover
#   /usr/bin/nyx_bkp_recover  (symlink)
#   /usr/share/applications/nyx-backup-recovery.desktop

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_DIR="$(cd "${SCRIPT_DIR}/../.." && pwd)"
STAGING="${WORKSPACE_DIR}/staging/linux/x86_64"
DIST="${WORKSPACE_DIR}/dist"
DO_BUILD=0

for arg in "$@"; do
    case "$arg" in
        --build) DO_BUILD=1 ;;
        *) echo "Unknown argument: $arg"; exit 1 ;;
    esac
done

VERSION=$(tr -d '[:space:]' < "${WORKSPACE_DIR}/VERSION")
echo "Building Recovery .deb for Nyx Backup v${VERSION}..."

command -v dpkg-deb >/dev/null 2>&1 || {
    echo "ERROR: dpkg-deb not found.  Install dpkg: sudo apt install dpkg"
    exit 1
}

if [[ $DO_BUILD -eq 1 ]]; then
    bash "${WORKSPACE_DIR}/scripts/linux/build_linux_x86_64.sh"
fi

[[ -f "$STAGING/nyx_bkp_recover" ]] || {
    echo "ERROR: $STAGING/nyx_bkp_recover not found."
    echo "  Run with --build, or run scripts/linux/build_linux_x86_64.sh first."
    exit 1
}

PKG_ROOT="${WORKSPACE_DIR}/target/deb-recovery-root"
PKG_NAME="nyxbackup-recovery"
ARCH="amd64"

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

# Strip the binary to bring the .deb down.  Skip if strip not available.
if command -v strip >/dev/null 2>&1; then
    strip "${PKG_ROOT}/usr/lib/nyxbackup-recovery/nyx_bkp_recover" || true
fi

SIZE_KB=$(du -sk "${PKG_ROOT}/usr" | cut -f1)

cat > "${PKG_ROOT}/DEBIAN/control" <<EOF
Package: ${PKG_NAME}
Version: ${VERSION}
Section: utils
Priority: optional
Architecture: ${ARCH}
Installed-Size: ${SIZE_KB}
Depends: libwebkit2gtk-4.1-0, libgtk-3-0, libayatana-appindicator3-1, librsvg2-2
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
