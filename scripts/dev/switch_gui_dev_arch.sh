#!/usr/bin/env bash
# Copyright (c) 2026 Nyx Software, LLC
# SPDX-License-Identifier: Apache-2.0
# Nyx Backup Recovery - https://nyxbackup.com
# Switch this Debian/Ubuntu build host's GUI dev libraries between amd64 and
# arm64.
#
# WHY THIS EXISTS: the WebKitGTK/GTK -dev packages are not co-installable
# across architectures on one host.  Installing libwebkit2gtk-4.1-dev:arm64
# EVICTS libwebkit2gtk-4.1-dev:amd64 (and vice versa) - apt removes the other
# silently, and the next build for that other architecture then dies with
# "webkit2gtk-4.1 dev packages not found".  A build host can therefore serve
# only one architecture at a time, and the scripts have to be able to flip it
# back.  build_linux_x86_64.sh and build_linux_arm64.sh call this automatically
# when their pkg-config probe comes up empty.
#
# Usage:
#   scripts/dev/switch_gui_dev_arch.sh host          # unsuffixed dev packages
#   scripts/dev/switch_gui_dev_arch.sh arm64-cross   # :arm64 dev packages
#
# "host" installs the dev set for whatever architecture this machine is - it is
# what an x86-64 host building x86-64 needs, and equally what a native ARM64
# host needs (there the packages are unsuffixed and no cross setup applies).
# "arm64-cross" is only for cross-building arm64 FROM another architecture: it
# adds the arm64 foreign architecture, the ports.ubuntu.com sources and the
# aarch64 cross toolchain, then installs the :arm64 dev set.
#
# `amd64` and `arm64` are accepted as aliases for host / arm64-cross.
#
# Idempotent: if the requested architecture's webkit2gtk-4.1.pc is already
# present, it exits 0 without touching apt.
#
# Needs root, or a sudo that does not prompt.  Set NYX_NO_AUTO_DEPS=1 to make
# this a no-op (exit 1) so a build fails with instructions instead of running
# apt - useful in CI images that are provisioned ahead of time.

set -euo pipefail

MODE="${1:-}"
case "$MODE" in
    host|amd64)       MODE="host" ;;
    arm64-cross|arm64) MODE="arm64-cross" ;;
    *) echo "Usage: $(basename "$0") {host|arm64-cross}"; exit 2 ;;
esac

# Cross-building arm64 only makes sense from a non-arm64 host; on a native
# ARM64 machine the unsuffixed packages ARE the arm64 ones.
if [[ "$MODE" == "arm64-cross" ]]; then
    case "$(uname -m)" in
        aarch64|arm64)
            echo "Native ARM64 host - the host dev set is already arm64; using host mode."
            MODE="host"
            ;;
    esac
fi

# The dev set both build scripts need.  Kept in one place so the two
# architectures cannot drift apart.
DEV_PKGS=(
    libwebkit2gtk-4.1-dev
    libgtk-3-dev
    librsvg2-dev
    libsoup-3.0-dev
    libjavascriptcoregtk-4.1-dev
    libssh2-1-dev
    libssl-dev
    libdbus-1-dev
)

# pkg-config path for the requested arch: the arm64 .pc files live under the
# aarch64 multiarch dir, which the host pkg-config does not search by default.
if [[ "$MODE" == "arm64-cross" ]]; then
    PC_ENV=(env "PKG_CONFIG_LIBDIR=/usr/lib/aarch64-linux-gnu/pkgconfig:/usr/share/pkgconfig")
else
    PC_ENV=(env)
fi

have_webkit() { "${PC_ENV[@]}" pkg-config --exists webkit2gtk-4.1 2>/dev/null; }

if have_webkit; then
    echo "GUI dev libs already present for ${MODE} ($("${PC_ENV[@]}" pkg-config --modversion webkit2gtk-4.1))."
    exit 0
fi

if [[ "${NYX_NO_AUTO_DEPS:-0}" == "1" ]]; then
    echo "ERROR: GUI dev libs missing for ${MODE} and NYX_NO_AUTO_DEPS=1."
    exit 1
fi

command -v apt-get >/dev/null 2>&1 || {
    echo "ERROR: no apt-get - cannot switch dev libs automatically on this host."
    exit 1
}

SUDO=()
if [[ "$(id -u)" != "0" ]]; then
    sudo -n true 2>/dev/null || {
        echo "ERROR: need root or a non-prompting sudo to install the ${MODE} dev libs."
        echo "  Run: sudo scripts/dev/switch_gui_dev_arch.sh ${MODE}"
        exit 1
    }
    SUDO=(sudo -n)
fi

APT=("${SUDO[@]}" env DEBIAN_FRONTEND=noninteractive apt-get)

echo "Switching GUI dev libs to ${MODE} (this evicts the other architecture's -dev packages)..."

if [[ "$MODE" == "arm64-cross" ]]; then
    # arm64 packages come from ports.ubuntu.com, not archive.ubuntu.com.  The
    # primary list must be pinned to amd64 first or apt tries ports for amd64
    # and 404s every index.
    if ! dpkg --print-foreign-architectures | grep -qx arm64; then
        "${SUDO[@]}" dpkg --add-architecture arm64
    fi
    if [[ -f /etc/apt/sources.list ]] && ! grep -q "arch=amd64" /etc/apt/sources.list; then
        "${SUDO[@]}" sed -i 's|^deb http|deb [arch=amd64] http|' /etc/apt/sources.list
    fi
    if [[ ! -f /etc/apt/sources.list.d/arm64-ports.list ]]; then
        CODENAME="$(. /etc/os-release && echo "${UBUNTU_CODENAME:-${VERSION_CODENAME}}")"
        "${SUDO[@]}" tee /etc/apt/sources.list.d/arm64-ports.list >/dev/null <<EOF
deb [arch=arm64] http://ports.ubuntu.com/ubuntu-ports ${CODENAME} main restricted universe multiverse
deb [arch=arm64] http://ports.ubuntu.com/ubuntu-ports ${CODENAME}-updates main restricted universe multiverse
deb [arch=arm64] http://ports.ubuntu.com/ubuntu-ports ${CODENAME}-security main restricted universe multiverse
EOF
    fi
    "${APT[@]}" update -qq
    # The cross toolchain is arch-independent of the above but needed for any
    # arm64 build, so install it here too rather than in a second place.
    "${APT[@]}" install -y -qq --no-install-recommends \
        gcc-aarch64-linux-gnu g++-aarch64-linux-gnu binutils-aarch64-linux-gnu
    SUFFIXED=("${DEV_PKGS[@]/%/:arm64}")
    "${APT[@]}" install -y -qq --no-install-recommends "${SUFFIXED[@]}"
else
    "${APT[@]}" install -y -qq --no-install-recommends "${DEV_PKGS[@]}"
fi

have_webkit || {
    echo "ERROR: still no webkit2gtk-4.1 for ${MODE} after installing."
    exit 1
}
echo "GUI dev libs now ${MODE} (webkit2gtk-4.1 $("${PC_ENV[@]}" pkg-config --modversion webkit2gtk-4.1))."
