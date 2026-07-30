#!/usr/bin/env bash
# Copyright (c) 2026 Nyx Software, LLC
# SPDX-License-Identifier: Apache-2.0
# Nyx Backup Recovery - https://nyxbackup.com
# Build the standalone Nyx Backup Recovery binary for Windows x86-64.
#
# Requirements (Ubuntu/Debian):
#   sudo apt install gcc-mingw-w64-x86-64 cmake curl nodejs npm
#   rustup target add x86_64-pc-windows-gnu
#
# Output: staging/windows/x86_64/   (nyx_bkp_recover.exe + WebView2Loader.dll
#         + locales, ready for scripts/windows/build_recover_msi_x86_64.sh)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_DIR="$(cd "${SCRIPT_DIR}/../.." && pwd)"

if [[ -f "${WORKSPACE_DIR}/.env" ]]; then
    set -a; source "${WORKSPACE_DIR}/.env"; set +a
fi
# OAuth client credentials are baked into the binary at compile time via
# env!() in bkp-recover (Google Drive / OneDrive / Dropbox restore).
: "${GOOGLE_OAUTH_CLIENT_ID:?Set GOOGLE_OAUTH_CLIENT_ID in .env or the environment}"
: "${GOOGLE_OAUTH_CLIENT_SECRET:?Set GOOGLE_OAUTH_CLIENT_SECRET in .env or the environment}"
: "${DROPBOX_APP_KEY:?Set DROPBOX_APP_KEY in .env or the environment}"
: "${DROPBOX_APP_SECRET:?Set DROPBOX_APP_SECRET in .env or the environment}"
: "${ONEDRIVE_OAUTH_CLIENT_ID:?Set ONEDRIVE_OAUTH_CLIENT_ID in .env or the environment}"
# OneDrive uses the public-client OAuth flow which does NOT send a secret.
export ONEDRIVE_OAUTH_CLIENT_SECRET="${ONEDRIVE_OAUTH_CLIENT_SECRET:-}"

TARGET="x86_64-pc-windows-gnu"
PROFILE="${PROFILE:-release}"

# --fast: release-fast profile for dev iteration only. DO NOT ship.
FAST=0
for arg in "$@"; do
    case "$arg" in
        --fast) FAST=1; PROFILE="release-fast" ;;
        *) echo "Unknown argument: $arg"; exit 1 ;;
    esac
done
if [[ "$FAST" == "1" ]]; then
    echo "--fast: using release-fast profile (DEV ONLY - do not ship)."
fi

STAGING="${WORKSPACE_DIR}/staging/windows/x86_64"
TOOLCHAIN="${WORKSPACE_DIR}/cmake/mingw-x86_64.cmake"
LIBSSH2_VERSION="1.11.0"
LIBSSH2_DIR="${WORKSPACE_DIR}/target/libssh2-win64"

# -- Preflight checks --------------------------------------------------------
check_cmd() { command -v "$1" >/dev/null 2>&1 || { echo "ERROR: $1 not found. $2"; exit 1; }; }
check_cmd x86_64-w64-mingw32-gcc "Install: sudo apt install gcc-mingw-w64-x86-64"
check_cmd cmake "Install: sudo apt install cmake"
check_cmd node  "Install: sudo apt install nodejs"
check_cmd npm   "Install: sudo apt install npm"

rustup target list --installed | grep -q "$TARGET" || {
    echo "Adding Rust target $TARGET..."
    rustup target add "$TARGET"
}

# -- Build libssh2 for Windows (cached) --------------------------------------
# libssh2-sys needs a native libssh2 for the SFTP restore backend; built with
# MinGW + WinCNG (Windows built-in crypto, no OpenSSL needed for this part).
if [[ ! -f "${LIBSSH2_DIR}/lib/libssh2.a" ]]; then
    echo "Building libssh2 ${LIBSSH2_VERSION} for ${TARGET}..."
    check_cmd curl "Install: sudo apt install curl"
    LIBSSH2_SRC="${WORKSPACE_DIR}/target/libssh2-src"
    TARBALL="${WORKSPACE_DIR}/target/libssh2-${LIBSSH2_VERSION}.tar.gz"
    mkdir -p "${WORKSPACE_DIR}/target"
    if [[ ! -f "$TARBALL" ]]; then
        echo "  Downloading libssh2 ${LIBSSH2_VERSION}..."
        curl -fL "https://www.libssh2.org/download/libssh2-${LIBSSH2_VERSION}.tar.gz" -o "$TARBALL"
    fi
    rm -rf "$LIBSSH2_SRC"; mkdir -p "$LIBSSH2_SRC"
    tar -xzf "$TARBALL" -C "$LIBSSH2_SRC" --strip-components=1
    mkdir -p "${LIBSSH2_DIR}/build"
    cmake -S "$LIBSSH2_SRC" -B "${LIBSSH2_DIR}/build" \
          -DCMAKE_TOOLCHAIN_FILE="$TOOLCHAIN" \
          -DCMAKE_INSTALL_PREFIX="$LIBSSH2_DIR" \
          -DCMAKE_BUILD_TYPE=Release -DBUILD_SHARED_LIBS=OFF \
          -DCRYPTO_BACKEND=WinCNG -DENABLE_ZLIB_COMPRESSION=OFF \
          -DBUILD_EXAMPLES=OFF -DBUILD_TESTING=OFF -Wno-dev \
          -DCMAKE_C_FLAGS="-D_WIN32_WINNT=0x0601"
    cmake --build "${LIBSSH2_DIR}/build" --config Release
    cmake --install "${LIBSSH2_DIR}/build"
    echo "  libssh2 built: ${LIBSSH2_DIR}/lib/libssh2.a"
else
    echo "libssh2 already built (${LIBSSH2_DIR}/lib/libssh2.a)."
fi

# -- Environment for cargo build ---------------------------------------------
export CC_x86_64_pc_windows_gnu="/usr/bin/x86_64-w64-mingw32-gcc"
export AR_x86_64_pc_windows_gnu="/usr/bin/x86_64-w64-mingw32-ar"
export LIBSSH2_STATIC=1
export LIBSSH2_INCLUDE_DIR="${LIBSSH2_DIR}/include"
export LIBSSH2_LIB_DIR="${LIBSSH2_DIR}/lib"
export PKG_CONFIG_ALLOW_CROSS=1

# -- Frontend build (recovery Tauri UI) --------------------------------------
bash "${WORKSPACE_DIR}/scripts/set_version.sh"

# Plain `cargo build` does not honour Tauri's beforeBuildCommand (that only
# runs under `cargo tauri build`), so the recovery UI must be built explicitly
# or the embedded dist stays stale.
echo "Building Svelte frontend (recovery)..."
cd "${WORKSPACE_DIR}/crates/bkp-recover/ui"
npm install --prefer-offline --no-audit --no-fund 2>&1 | tail -3
npm run build
cd "$WORKSPACE_DIR"

# -- Version fingerprint busting ---------------------------------------------
# Cargo's incremental fingerprint does not reliably detect a workspace-
# inherited version bump (version.workspace = true), so the binary can stamp
# a stale env!("CARGO_PKG_VERSION"). Force-clean bkp-recover when VERSION
# changed since the last build, tracked by a stamp file.
WORKSPACE_VER=$(tr -d '[:space:]' < "${WORKSPACE_DIR}/VERSION")
STAMP="${WORKSPACE_DIR}/target/.recover_version_stamp"
if [[ "$FAST" != "1" ]]; then
    if [[ ! -f "$STAMP" || "$(cat "$STAMP" 2>/dev/null)" != "$WORKSPACE_VER" ]]; then
        echo "Version changed -> clean bkp-recover to re-stamp ${WORKSPACE_VER}..."
        cargo clean -p bkp-recover --target "$TARGET" --profile "$PROFILE" 2>/dev/null || true
        mkdir -p "${WORKSPACE_DIR}/target"; echo "$WORKSPACE_VER" > "$STAMP"
    fi
fi

# -- Cargo build -------------------------------------------------------------
echo "Building nyx_bkp_recover for ${TARGET} (${PROFILE})..."
CARGO_FLAGS="--target $TARGET"
if [[ "$PROFILE" == "release" ]]; then
    CARGO_FLAGS="$CARGO_FLAGS --release"
elif [[ "$PROFILE" == "release-fast" ]]; then
    CARGO_FLAGS="$CARGO_FLAGS --profile release-fast"
fi
cargo build $CARGO_FLAGS -p bkp-recover --bin nyx_bkp_recover

# -- Stage -------------------------------------------------------------------
echo "Staging files..."
RELEASE_DIR="${WORKSPACE_DIR}/target/${TARGET}/${PROFILE}"
rm -rf "$STAGING"; mkdir -p "$STAGING/locales"

cp "$RELEASE_DIR/nyx_bkp_recover.exe" "$STAGING/"

# Optional Authenticode signing.
if [[ -n "${NYX_SIGN_CERT:-}" && -n "${NYX_SIGN_KEY:-}" ]]; then
    "${SCRIPT_DIR}/sign_pe.sh" "$STAGING/nyx_bkp_recover.exe"
fi

# WebView2Loader.dll is emitted by webview2-com-sys into the release dir.
WEBVIEW2_DLL="${RELEASE_DIR}/WebView2Loader.dll"
[[ -f "$WEBVIEW2_DLL" ]] || WEBVIEW2_DLL="${WORKSPACE_DIR}/target/webview2/WebView2Loader.dll"
if [[ -f "$WEBVIEW2_DLL" ]]; then
    cp "$WEBVIEW2_DLL" "$STAGING/"
else
    echo "WARNING: WebView2Loader.dll not found - nyx_bkp_recover.exe will fail to"
    echo "  launch on systems without WebView2.  Expected: ${RELEASE_DIR}/WebView2Loader.dll"
fi

cp "${WORKSPACE_DIR}/locales/"*.json "$STAGING/locales/"

echo ""
echo "Staged to: $STAGING"
echo "$(ls -lh "$STAGING/" | tail -n +2)"
echo ""
echo "Next: scripts/windows/build_recover_msi_x86_64.sh"
