#!/usr/bin/env bash
# Copyright (c) 2026 Nyx Software, LLC
# SPDX-License-Identifier: Apache-2.0
# Nyx Backup Recovery - https://nyxbackup.com
#
# One-time setup of an x86-64 Ubuntu 24.04 (noble) host to cross-build the
# ARM64 recovery installers.  Resolves the two host blockers that stock noble
# does not satisfy out of the box:
#
#   1. Linux ARM64 .deb/.rpm need the arm64 WebKitGTK/GTK -dev stack, which is
#      not installable until arm64 multiarch is enabled and pointed at
#      ports.ubuntu.com (archive.ubuntu.com carries no arm64).  Several
#      gobject-introspection .gir files collide between the amd64 and arm64
#      -dev packages (shared path, arch-specific content), so dpkg needs
#      --force-overwrite; arm64 maintainer scripts need qemu-user-static to run.
#
#   2. The Windows ARM64 .msi needs a wixl that understands arm64.  As of
#      msitools 0.106 (the latest release) wixl's Arch enum is only x86/ia64/
#      x64 and it rejects `--arch arm64`.  This builds wixl from the 0.106
#      source with a small patch (add ARM64 to the Arch enum, emit the MSI
#      summary Template "Arm64", treat ARM64 as 64-bit for component attrs) and
#      installs it to /usr/local (ahead of the distro wixl on PATH).
#
# Safe to re-run: each stage is skipped if already satisfied.  It does change
# system apt configuration (adds an arm64 ports source, pins the stock sources
# to amd64) - review before running on a machine you care about.  On a native
# ARM64 host or CI runner none of this is needed; just install the normal
# build_linux_x86_64.sh dependency set against the native arch.
#
# Usage:  sudo-capable user runs:  scripts/dev/setup_arm64_buildhost.sh

set -euo pipefail

MSITOOLS_VERSION="0.106"

echo "== 1/4 arm64 multiarch sources =="
if dpkg --print-foreign-architectures | grep -qx arm64; then
    echo "  arm64 multiarch already enabled."
else
    sudo dpkg --add-architecture arm64
fi
# Pin the stock Ubuntu sources to amd64 so apt does not look for arm64 on
# archive.ubuntu.com / security.ubuntu.com (which carry none).
if ! grep -q '^Architectures: amd64' /etc/apt/sources.list.d/ubuntu.sources; then
    sudo cp -a /etc/apt/sources.list.d/ubuntu.sources \
               /etc/apt/sources.list.d/ubuntu.sources.bak.nyx
    sudo sed -i '/^URIs:/a Architectures: amd64' /etc/apt/sources.list.d/ubuntu.sources
    echo "  pinned ubuntu.sources to amd64 (backup: ubuntu.sources.bak.nyx)."
fi
# arm64 packages live on ports.ubuntu.com.
if [[ ! -f /etc/apt/sources.list.d/ubuntu-ports-arm64.sources ]]; then
    sudo tee /etc/apt/sources.list.d/ubuntu-ports-arm64.sources >/dev/null <<'EOF'
## Nyx Backup Recovery: arm64 cross-build dev libraries.
## arm64 binaries are served from ports.ubuntu.com, not archive.ubuntu.com.
Types: deb
URIs: http://ports.ubuntu.com/ubuntu-ports/
Suites: noble noble-updates noble-backports noble-security
Components: main universe restricted multiverse
Architectures: arm64
Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg
EOF
    echo "  added ubuntu-ports-arm64.sources."
fi
sudo apt-get update

echo "== 2/4 arm64 GUI dev stack + cross toolchain + qemu =="
sudo apt-get install -y gcc-aarch64-linux-gnu qemu-user-static binfmt-support
# --force-overwrite resolves the shared gobject-introspection .gir file clash
# between the amd64 and arm64 -dev packages (benign for cross-compilation).
sudo apt-get -o Dpkg::Options::="--force-overwrite" install -y \
    libwebkit2gtk-4.1-dev:arm64 libgtk-3-dev:arm64 \
    libayatana-appindicator3-dev:arm64 librsvg2-dev:arm64 \
    libsoup-3.0-dev:arm64 libjavascriptcoregtk-4.1-dev:arm64 \
    libssl-dev:arm64

echo "== 3/4 rust + tooling =="
rustup target add aarch64-unknown-linux-gnu aarch64-pc-windows-gnullvm || true
command -v fpm  >/dev/null || echo "  NOTE: fpm not found - needed for .rpm (sudo gem install fpm)."
command -v zstd >/dev/null || sudo apt-get install -y zstd

echo "== 4/4 wixl with arm64 support =="
if wixl --arch arm64 /dev/null 2>&1 | grep -q "not supported"; then
    echo "  building patched wixl ${MSITOOLS_VERSION}..."
    sudo apt-get install -y \
        meson ninja-build valac bison flex gettext pkg-config \
        libgcab-dev libgsf-1-dev libglib2.0-dev uuid-dev libbz2-dev \
        libgirepository1.0-dev gobject-introspection
    WORK="$(mktemp -d)"
    curl -fsSL "https://download.gnome.org/sources/msitools/${MSITOOLS_VERSION}/msitools-${MSITOOLS_VERSION}.tar.xz" \
        -o "${WORK}/msitools.tar.xz"
    tar -xf "${WORK}/msitools.tar.xz" -C "$WORK"
    SRC="${WORK}/msitools-${MSITOOLS_VERSION}"
    # --- arm64 Arch patch (see header) ---
    sed -i 's/^        X64;/        X64,\n        ARM64;/' "${SRC}/tools/wixl/builder.vala"
    sed -i 's/                case X64: return "x64";/                case X64: return "x64";\n                case ARM64: return "arm64";/' "${SRC}/tools/wixl/builder.vala"
    sed -i 's/(arch == Arch.X64 || arch == Arch.IA64)/(arch == Arch.X64 || arch == Arch.IA64 || arch == Arch.ARM64)/g' "${SRC}/tools/wixl/builder.vala"
    sed -i 's/                return "Intel";/                return "Intel";\n            else if (arch == Arch.ARM64)\n                return "Arm64";/' "${SRC}/tools/wixl/msi.vala"
    # --- build + install to /usr/local (DESTDIR stage avoids needing meson on
    #     root PATH; the distro wixl in /usr/bin is shadowed, not replaced) ---
    meson setup "${SRC}/_build" --prefix=/usr/local --buildtype=release >/dev/null
    meson compile -C "${SRC}/_build"
    rm -rf "${WORK}/stage"
    DESTDIR="${WORK}/stage" meson install -C "${SRC}/_build" >/dev/null
    sudo cp -a "${WORK}/stage/usr/local/." /usr/local/
    sudo ldconfig
    hash -r
    rm -rf "$WORK"
fi
if wixl --arch arm64 /dev/null 2>&1 | grep -q "not supported"; then
    echo "  ERROR: wixl still lacks arm64 support after build." >&2
    exit 1
fi
echo "  wixl $(wixl --version) at $(command -v wixl) supports arm64."

echo ""
echo "Done.  ARM64 cross-build host is ready.  Build with:"
echo "  scripts/linux/build_recover_deb_arm64.sh --build"
echo "  scripts/linux/build_recover_rpm.sh --arch arm64"
echo "  scripts/windows/build_recover_msi_arm64.sh --build"
