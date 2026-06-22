#!/usr/bin/env bash
# Copyright (c) 2026 Nyx Software, LLC
# SPDX-License-Identifier: Apache-2.0
# Nyx Backup Recovery - https://nyxbackup.com
# Build the standalone Nyx Backup Recovery binary for Linux ARM64 (aarch64)
# and stage it.
#
# This cross-compiles from an x86-64 host using the aarch64-linux-gnu GCC
# toolchain.  The Tauri GUI links against WebKitGTK/GTK, so the ARM64
# (:arm64 multiarch) dev libraries must be present on the build host - this
# is the one piece that is NOT installable with the plain x86-64 dev setup.
#
# Requirements (Ubuntu/Debian x86-64 host):
#   sudo apt install gcc-aarch64-linux-gnu pkg-config
#   rustup target add aarch64-unknown-linux-gnu
#
#   # ARM64 GUI dev libs (enable the arm64 architecture first):
#   sudo dpkg --add-architecture arm64
#   sudo apt update
#   sudo apt install libwebkit2gtk-4.1-dev:arm64 libgtk-3-dev:arm64 \
#                    libayatana-appindicator3-dev:arm64 librsvg2-dev:arm64 \
#                    libsoup-3.0-dev:arm64 libjavascriptcoregtk-4.1-dev:arm64 \
#                    libssh2-1-dev:arm64 libssl-dev:arm64
#
# Cleaner alternative: build on a native ARM64 host / CI ARM64 runner, where
# the normal scripts/linux/build_linux_x86_64.sh dependency set applies and no
# cross-toolchain is needed - just swap the target triple.
#
# Output: staging/linux/arm64/   (nyx_bkp_recover + locales, ready for
#         scripts/linux/build_recover_deb_arm64.sh)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_DIR="$(cd "${SCRIPT_DIR}/../.." && pwd)"

if [[ -f "${WORKSPACE_DIR}/.env" ]]; then
    set -a; source "${WORKSPACE_DIR}/.env"; set +a
fi
: "${GOOGLE_OAUTH_CLIENT_ID:?Set GOOGLE_OAUTH_CLIENT_ID in .env or the environment}"
: "${GOOGLE_OAUTH_CLIENT_SECRET:?Set GOOGLE_OAUTH_CLIENT_SECRET in .env or the environment}"
: "${DROPBOX_APP_KEY:?Set DROPBOX_APP_KEY in .env or the environment}"
: "${DROPBOX_APP_SECRET:?Set DROPBOX_APP_SECRET in .env or the environment}"
: "${ONEDRIVE_OAUTH_CLIENT_ID:?Set ONEDRIVE_OAUTH_CLIENT_ID in .env or the environment}"
export ONEDRIVE_OAUTH_CLIENT_SECRET="${ONEDRIVE_OAUTH_CLIENT_SECRET:-}"

TARGET_BASE="aarch64-unknown-linux-gnu"
PROFILE="${PROFILE:-release}"
STAGING="${WORKSPACE_DIR}/staging/linux/arm64"
ARM64_MULTIARCH="aarch64-linux-gnu"

# -- Preflight checks --------------------------------------------------------
check_cmd() { command -v "$1" >/dev/null 2>&1 || { echo "ERROR: $1 not found. $2"; exit 1; }; }
check_cmd cargo "Install Rust: https://rustup.rs"
check_cmd node  "Install Node: sudo apt install nodejs"
check_cmd npm   "Install npm:  sudo apt install npm"

# Distinguish a native ARM64 host from an x86-64 cross-build host.  On a native
# aarch64 host the system compiler and pkg-config already target ARM64, so the
# cross-toolchain and the arch-suffixed pkg-config paths are unnecessary.
HOST_ARCH="$(uname -m)"
if [[ "$HOST_ARCH" == "aarch64" || "$HOST_ARCH" == "arm64" ]]; then
    NATIVE_ARM=1
    echo "Native ARM64 host detected - building without a cross-toolchain."
else
    NATIVE_ARM=0
    check_cmd "${ARM64_MULTIARCH}-gcc" "Install: sudo apt install gcc-aarch64-linux-gnu"
    # Point pkg-config at the ARM64 multiarch .pc files so the GUI libs resolve
    # to the arm64 variants, not the host x86-64 ones.
    export PKG_CONFIG_ALLOW_CROSS=1
    export PKG_CONFIG_LIBDIR="/usr/lib/${ARM64_MULTIARCH}/pkgconfig:/usr/share/pkgconfig"
    export PKG_CONFIG_PATH="/usr/lib/${ARM64_MULTIARCH}/pkgconfig"
    # cargo / cc-rs cross-compile wiring for the aarch64 target.
    export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER="${ARM64_MULTIARCH}-gcc"
    export CC_aarch64_unknown_linux_gnu="${ARM64_MULTIARCH}-gcc"
    export CXX_aarch64_unknown_linux_gnu="${ARM64_MULTIARCH}-g++"
    export AR_aarch64_unknown_linux_gnu="${ARM64_MULTIARCH}-ar"
fi

if ! pkg-config --exists webkit2gtk-4.1 2>/dev/null; then
    echo "ERROR: webkit2gtk-4.1 (arm64) dev packages not found - required for the GUI."
    echo "  This is the host dependency the ARM64 cross-build needs.  Enable the"
    echo "  arm64 architecture and install the :arm64 dev libs (see this script's"
    echo "  header), or build on a native ARM64 host / CI runner."
    exit 1
fi

rustup target list --installed | grep -q "$TARGET_BASE" || rustup target add "$TARGET_BASE"

# Static libssh2 so the .deb does not require libssh2-1 to be pre-installed.
export LIBSSH2_STATIC=1

# -- Frontend build (recovery Tauri UI) --------------------------------------
bash "${WORKSPACE_DIR}/scripts/set_version.sh"

echo "Building Svelte frontend (recovery)..."
cd "${WORKSPACE_DIR}/crates/bkp-recover/ui"
npm install --prefer-offline --no-audit --no-fund 2>&1 | tail -3
npm run build
cd "$WORKSPACE_DIR"

# -- Version fingerprint busting ---------------------------------------------
WORKSPACE_VER=$(tr -d '[:space:]' < "${WORKSPACE_DIR}/VERSION")
STAMP="${WORKSPACE_DIR}/target/.recover_version_stamp_linux_arm64"
if [[ ! -f "$STAMP" || "$(cat "$STAMP" 2>/dev/null)" != "$WORKSPACE_VER" ]]; then
    echo "Version changed -> clean bkp-recover to re-stamp ${WORKSPACE_VER}..."
    cargo clean -p bkp-recover --target "$TARGET_BASE" --profile "$PROFILE" 2>/dev/null || true
    mkdir -p "${WORKSPACE_DIR}/target"; echo "$WORKSPACE_VER" > "$STAMP"
fi

# -- Cargo build -------------------------------------------------------------
# Native cross-link against the host's arm64 multiarch glibc (no cargo-zigbuild
# glibc cap - see build_linux_x86_64.sh for why the cap collides with
# openssl-sys).  Trade-off: the binary requires a glibc at least as new as the
# arm64 libs on this build host; for wider compatibility build on an older
# arm64 glibc base.
CARGO_FLAGS="--target $TARGET_BASE"
[[ "$PROFILE" == "release" ]] && CARGO_FLAGS="$CARGO_FLAGS --release"

echo "Building nyx_bkp_recover for ${TARGET_BASE} (${PROFILE})..."
cargo build $CARGO_FLAGS -p bkp-recover --bin nyx_bkp_recover

# -- Stage -------------------------------------------------------------------
echo "Staging files..."
RELEASE_DIR="${WORKSPACE_DIR}/target/${TARGET_BASE}/${PROFILE}"
rm -rf "$STAGING"; mkdir -p "$STAGING/locales"

cp "$RELEASE_DIR/nyx_bkp_recover" "$STAGING/"

# English is compiled into the binary; ship the rest for non-English locales.
for f in "${WORKSPACE_DIR}/locales/"*.json; do
    [[ "$(basename "$f")" == "en.json" ]] && continue
    cp "$f" "$STAGING/locales/"
done

echo ""
echo "Staged to: $STAGING"
ls -lh "$STAGING/"
echo ""
echo "Next: scripts/linux/build_recover_deb_arm64.sh"
