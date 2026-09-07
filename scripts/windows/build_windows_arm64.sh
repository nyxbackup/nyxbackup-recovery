#!/usr/bin/env bash
# Copyright (c) 2026 Nyx Software, LLC
# SPDX-License-Identifier: Apache-2.0
# Nyx Backup Recovery - https://nyxbackup.com
# Build the standalone Nyx Backup Recovery binary for Windows ARM64.
#
# The aarch64-pc-windows-gnullvm target is LLVM-based (clang + LLD +
# compiler-rt + the Windows UCRT), cross-compiled from Linux with llvm-mingw.
# There is no system aarch64 Windows GCC, so the C compiler, archiver, and
# linker all come from llvm-mingw.  .cargo/config.toml intentionally leaves
# the [target.aarch64-pc-windows-gnullvm] linker/AR unset because llvm-mingw
# lives at a user-chosen path; this script supplies them via the
# CARGO_TARGET_AARCH64_PC_WINDOWS_GNULLVM_LINKER and AR_aarch64_pc_windows_gnullvm
# environment variables.
#
# Unlike x86_64-pc-windows-gnu, gnullvm needs no libgcc/emutls keep-flags:
# compiler-rt supplies the builtins natively and aarch64 Windows uses native
# TLS, so the __emutls_get_address workaround does not apply here.
#
# Requirements:
#   - llvm-mingw with the aarch64 toolchain (set LLVM_MINGW, default
#     /opt/llvm-mingw).  Provides aarch64-w64-mingw32-{gcc,g++,ar,windres}.
#   - rustup target add aarch64-pc-windows-gnullvm
#   - cmake curl nodejs npm  (sudo apt install cmake curl nodejs npm)
#
# Output: staging/windows/arm64/   (nyx_bkp_recover.exe + WebView2Loader.dll
#         + locales, ready for scripts/windows/build_recover_msi_arm64.sh)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_DIR="$(cd "${SCRIPT_DIR}/../.." && pwd)"

# OAuth build secrets: load .env (real environment wins) and hard-fail on any
# variable the recover binary embeds via env!().  Shared so a fix cannot land
# in only some of the six build scripts.
source "${WORKSPACE_DIR}/scripts/common/oauth_env.sh"
# OneDrive uses the public-client OAuth flow which does NOT send a secret.

TARGET="aarch64-pc-windows-gnullvm"
PROFILE="${PROFILE:-release}"

# llvm-mingw install root (user-chosen path).  Override with LLVM_MINGW.
export LLVM_MINGW="${LLVM_MINGW:-/opt/llvm-mingw}"
MINGW_BIN="${LLVM_MINGW}/bin"

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

STAGING="${WORKSPACE_DIR}/staging/windows/arm64"
TOOLCHAIN="${WORKSPACE_DIR}/cmake/mingw-aarch64.cmake"
LIBSSH2_VERSION="1.11.0"
LIBSSH2_DIR="${WORKSPACE_DIR}/target/libssh2-winarm64"

# -- Preflight checks --------------------------------------------------------
check_cmd() { command -v "$1" >/dev/null 2>&1 || { echo "ERROR: $1 not found. $2"; exit 1; }; }
[[ -x "${MINGW_BIN}/aarch64-w64-mingw32-gcc" ]] || {
    echo "ERROR: ${MINGW_BIN}/aarch64-w64-mingw32-gcc not found."
    echo "  Install llvm-mingw with the aarch64 toolchain and set LLVM_MINGW,"
    echo "  e.g. export LLVM_MINGW=/opt/llvm-mingw"
    exit 1
}
check_cmd cmake "Install: sudo apt install cmake"
check_cmd node  "Install: sudo apt install nodejs"
check_cmd npm   "Install: sudo apt install npm"

rustup target list --installed | grep -q "$TARGET" || {
    echo "Adding Rust target $TARGET..."
    rustup target add "$TARGET"
}

# -- Build libssh2 for Windows ARM64 (cached) --------------------------------
# libssh2-sys needs a native libssh2 for the SFTP restore backend; built with
# llvm-mingw + WinCNG (Windows built-in crypto, no OpenSSL needed).
if [[ ! -f "${LIBSSH2_DIR}/lib/libssh2.a" ]]; then
    echo "Building libssh2 ${LIBSSH2_VERSION} for ${TARGET}..."
    check_cmd curl "Install: sudo apt install curl"
    LIBSSH2_SRC="${WORKSPACE_DIR}/target/libssh2-src-arm64"
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
# llvm-mingw on PATH.  The absolute-path env vars below cover the linker, AR
# and cc-rs, but tauri-winres (via tauri-build, for the .exe resource/manifest)
# shells out to `aarch64-w64-mingw32-windres` BY NAME and has no override, so
# without this it panics with
#   called `Result::unwrap()` on an `Err` value: NotAttempted("aarch64-w64-mingw32-windres")
# The x86_64 build never hits this because gcc-mingw-w64 puts its windres in
# /usr/bin; there is no distro package for the aarch64 one.
export PATH="${MINGW_BIN}:$PATH"

# Linker + AR for the gnullvm target (see header - not set in config.toml).
export CARGO_TARGET_AARCH64_PC_WINDOWS_GNULLVM_LINKER="${MINGW_BIN}/aarch64-w64-mingw32-gcc"
export AR_aarch64_pc_windows_gnullvm="${MINGW_BIN}/aarch64-w64-mingw32-ar"
# cc-rs (compiles any C in -sys crates) for the gnullvm target.
export CC_aarch64_pc_windows_gnullvm="${MINGW_BIN}/aarch64-w64-mingw32-gcc"
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
STAMP="${WORKSPACE_DIR}/target/.recover_version_stamp_winarm64"
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
if [[ ( -n "${NYX_SIGN_CERT:-}" && -n "${NYX_SIGN_KEY:-}" ) || -n "${NYX_TS_PROFILE:-}" ]]; then
    "${SCRIPT_DIR}/sign_pe.sh" "$STAGING/nyx_bkp_recover.exe"
fi

# libunwind.dll: required, and a separate question from the C runtime.
# aarch64-pc-windows-gnullvm links LLVM's unwinder DYNAMICALLY, so
# nyx_bkp_recover.exe imports libunwind.dll and does not start without it.
# x86_64-pc-windows-gnu links GCC's unwinder statically and ships nothing,
# which is why this stayed invisible until an ARM64 machine ran the MSI.
#
# Confirmed against the published 0.9.16 artifacts (2026-09-10): the ARM64
# recovery MSI contains only WebView2Loader.dll and nyx_bkp_recover.exe, and
# that binary imports libunwind.dll once; the x86_64 binary imports it zero
# times.  Same defect the main app carried until 0.9.73.
#
# Hard failure, not a warning.  A missing WebView2Loader.dll degrades to "the
# window will not open"; a missing libunwind.dll means the recovery tool does
# not run at all - on the machine whose main app already does not run.
UNWIND_DLL=""
CLANG_BIN="$(command -v aarch64-w64-mingw32-clang)"
if [[ -n "$CLANG_BIN" ]]; then
    TOOLCHAIN_ROOT="$(cd "$(dirname "$CLANG_BIN")/.." && pwd)"
else
    TOOLCHAIN_ROOT="/opt/llvm-mingw"
fi
for candidate in \
    "${TOOLCHAIN_ROOT}/aarch64-w64-mingw32/bin/libunwind.dll" \
    "${TOOLCHAIN_ROOT}/bin/libunwind.dll" \
    "/opt/llvm-mingw/aarch64-w64-mingw32/bin/libunwind.dll"; do
    if [[ -f "$candidate" ]]; then UNWIND_DLL="$candidate"; break; fi
done
if [[ -z "$UNWIND_DLL" ]]; then
    echo "ERROR: libunwind.dll not found in the llvm-mingw toolchain."
    echo "  nyx_bkp_recover.exe imports it; without it the tool does not start."
    echo "  Searched:"
    echo "    ${TOOLCHAIN_ROOT}/aarch64-w64-mingw32/bin/libunwind.dll"
    echo "    ${TOOLCHAIN_ROOT}/bin/libunwind.dll"
    echo "    /opt/llvm-mingw/aarch64-w64-mingw32/bin/libunwind.dll"
    exit 1
fi
cp "$UNWIND_DLL" "$STAGING/"
echo "Staged libunwind.dll from ${UNWIND_DLL}"

# WebView2Loader.dll is emitted by webview2-com-sys into the release dir.
# For an aarch64 target build.rs copies the arm64 loader DLL.
WEBVIEW2_DLL="${RELEASE_DIR}/WebView2Loader.dll"
[[ -f "$WEBVIEW2_DLL" ]] || WEBVIEW2_DLL="${WORKSPACE_DIR}/target/webview2/WebView2Loader.dll"
if [[ -f "$WEBVIEW2_DLL" ]]; then
    cp "$WEBVIEW2_DLL" "$STAGING/"
else
    echo "WARNING: WebView2Loader.dll not found - nyx_bkp_recover.exe will fail to"
    echo "  launch on systems without WebView2.  Expected: ${RELEASE_DIR}/WebView2Loader.dll"
fi

cp "${WORKSPACE_DIR}/locales/"*.json "$STAGING/locales/"

# Refuse to stage a binary that needs a library this package does not ship.
# The published 0.9.16 ARM64 MSI imported libunwind.dll and did not contain it,
# so the recovery tool could not start on Windows 11 ARM64 - on exactly the
# machine whose main app already could not start.  The dependency is readable
# straight out of the binary, so it is read here rather than discovered by a
# user who has already lost their data once.
python3 "${WORKSPACE_DIR}/scripts/common/check_runtime_deps.py" windows arm64 "$STAGING"

echo ""
echo "Staged to: $STAGING"
echo "$(ls -lh "$STAGING/" | tail -n +2)"
echo ""
echo "Next: scripts/windows/build_recover_msi_arm64.sh"
