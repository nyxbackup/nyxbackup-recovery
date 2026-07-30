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

# -- glibc ABI ceiling -------------------------------------------------------
# One package has to install everywhere the app can run at all.  A native
# build stamps in the BUILD host's glibc symbol versions, so building on 24.04
# silently produces a binary that dies on older distros with
# "version `GLIBC_2.39' not found".  Fail loudly here instead.
#
# The ceiling is 2.34, not 2.35, because this same staged binary is also
# repackaged as the .rpm (build_recover_rpm.sh) and the RHEL 9 family - RHEL 9,
# CentOS Stream 9, AlmaLinux 9, Rocky 9 - is pinned at exactly glibc 2.34.
# One 2.35 symbol drops that entire family while still installing fine on
# Ubuntu 22.04, so the deb-only floor would not catch it.  Build host is
# Ubuntu 22.04 (glibc 2.35); it currently links nothing above 2.34.
# Override with MAX_GLIBC=x.y when the support floor moves.
MAX_GLIBC="${MAX_GLIBC:-2.34}"
if command -v objdump >/dev/null 2>&1; then
    TOO_NEW=$(objdump -T "$STAGING/nyx_bkp_recover" \
              | grep -oE 'GLIBC_[0-9]+\.[0-9]+' | sort -u -V | tail -1)
    TOO_NEW="${TOO_NEW#GLIBC_}"
    HIGHEST=$(printf '%s\n%s\n' "$MAX_GLIBC" "$TOO_NEW" | sort -V | tail -1)
    if [[ "$HIGHEST" != "$MAX_GLIBC" ]]; then
        echo "ERROR: binary requires GLIBC_${TOO_NEW}, ceiling is GLIBC_${MAX_GLIBC}."
        echo "  Offending symbols:"
        objdump -T "$STAGING/nyx_bkp_recover" \
            | grep -E "GLIBC_(2\.(3[6-9]|[4-9][0-9])|[3-9]\.)" \
            | awk '{print "    " $NF " (" $(NF-1) ")"}' | sort -u
        echo "  Rebuild on an Ubuntu 22.04 (glibc 2.35) host - see the header of"
        echo "  scripts/linux/build_linux_x86_64.sh."
        exit 1
    fi
    echo "glibc ABI check: requires at most GLIBC_${TOO_NEW} (ceiling ${MAX_GLIBC}) - OK"
    # Pin Depends to what the binary actually needs, not to the ceiling - the
    # measured value is usually lower (the newest symbol the link happened to
    # pull), and every 0.01 of slack is another distro that can install this.
    DEB_GLIBC="$TOO_NEW"
else
    echo "WARNING: objdump not found - skipping the glibc ABI ceiling check."
    DEB_GLIBC="$MAX_GLIBC"
fi

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

# Depends is derived from the binary's actual DT_NEEDED list, not from the
# build-time dev packages.  Over-declaring blocks installs for no reason: the
# recovery app registers no tray icon (nothing links libayatana-appindicator)
# and librsvg is only dlopen'd by gdk-pixbuf for SVG icons, so both are
# Recommends at most.  The `a | b` alternatives cover Debian/Ubuntu's 64-bit
# time_t rename (libgtk-3-0 -> libgtk-3-0t64 etc.); the t64 packages declare
# Provides for the old names, so either spelling resolves on every release
# from Ubuntu 22.04 / Debian 12 onward.  libc6 is version-pinned so apt
# reports a clear unmet dependency instead of the binary dying at exec with
# "version `GLIBC_2.xx' not found".
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
