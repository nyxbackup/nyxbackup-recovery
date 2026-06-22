#!/usr/bin/env bash
# Copyright (c) 2026 Nyx Software, LLC
# SPDX-License-Identifier: Apache-2.0
# Nyx Backup Recovery - https://nyxbackup.com
# Build the standalone Nyx Backup Recovery MSI for Windows ARM64.
#
# Assumes scripts/windows/build_windows_arm64.sh has already run so that
# staging/windows/arm64/nyx_bkp_recover.exe + WebView2Loader.dll exist.
# Pass --build to chain a fresh build first.
#
# Output: dist/NyxBackup-Recovery-VERSION-arm64.msi

set -euo pipefail

WORKSPACE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
STAGING="${WORKSPACE_DIR}/staging/windows/arm64"
WXS="${WORKSPACE_DIR}/installer/windows/NyxBackupRecovery-arm64.wxs"
DIST="${WORKSPACE_DIR}/dist"
DO_BUILD=0

for arg in "$@"; do
    case "$arg" in
        --build) DO_BUILD=1 ;;
        *) echo "Unknown argument: $arg"; exit 1 ;;
    esac
done

VERSION=$(cat "$WORKSPACE_DIR/VERSION" | tr -d '[:space:]')
echo "Building Recovery MSI (ARM64) for Nyx Backup v${VERSION}..."

command -v wixl >/dev/null 2>&1 || {
    echo "ERROR: wixl not found.  Install msitools: sudo apt install msitools"
    exit 1
}

# wixl needs arm64 arch support, which as of msitools 0.106 (the latest
# release) is NOT present: its Arch enum is only x86/ia64/x64 and it rejects
# `--arch arm64` with "arch of type 'arm64' is not supported".  arm64 requires
# a wixl built from source with the Arch enum extended (add ARM64, map the MSI
# summary Template to "Arm64", treat ARM64 as 64-bit for component attrs); see
# docs/BUILD_ARM64.md and scripts/dev/.  Probe for the missing support and fail
# with a clear message rather than a cryptic mid-build error.  (The arch is
# validated at argument-parse time, before the .wxs is read, so a throwaway
# /dev/null input triggers the check.)  The probe output is captured into a
# variable first - piping straight into `grep -q` under `set -o pipefail`
# SIGPIPE-kills wixl (exit 141) and the matched condition is masked by the
# failed pipeline.
WIXL_ARCH_PROBE="$(wixl --arch arm64 /dev/null 2>&1 || true)"
if [[ "$WIXL_ARCH_PROBE" == *"not supported"* ]]; then
    WIXL_VER=$(wixl --version 2>/dev/null | tr -d '[:space:]')
    echo "ERROR: this wixl (${WIXL_VER:-unknown}) lacks arm64 arch support."
    echo "  Stock msitools (<= 0.106) cannot build an ARM64 MSI.  Install a wixl"
    echo "  built from source with the ARM64 Arch patch (see build-host notes)."
    exit 1
fi

if [[ $DO_BUILD -eq 1 ]]; then
    bash "${WORKSPACE_DIR}/scripts/windows/build_windows_arm64.sh"
fi

for f in nyx_bkp_recover.exe WebView2Loader.dll; do
    [[ -f "$STAGING/$f" ]] || {
        echo "ERROR: missing $STAGING/$f"
        echo "  Run with --build, or run scripts/windows/build_windows_arm64.sh first."
        exit 1
    }
done

# Same wixl-vs-DOS-timezone dance the x86_64 MSI does.  See
# build_recover_msi_x86_64.sh for the rationale.
WIN_TIME=$(powershell.exe -Command "Get-Date -Format 'yyyyMMddHHmm'" 2>/dev/null | tr -d '\r\n')
if [[ -n "$WIN_TIME" ]]; then
    ( export TZ=UTC
      touch -t "$WIN_TIME" "$STAGING/nyx_bkp_recover.exe" "$STAGING/WebView2Loader.dll" )
fi

mkdir -p "$DIST"
OUTPUT="${DIST}/NyxBackup-Recovery-${VERSION}-arm64.msi"

wixl \
    --arch arm64 \
    --define "Version=${VERSION}" \
    --define "StagingDir=${STAGING}" \
    --output "$OUTPUT" \
    "$WXS"

if [[ -n "${NYX_SIGN_CERT:-}" && -n "${NYX_SIGN_KEY:-}" ]]; then
    SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
    "${SCRIPT_DIR}/sign_pe.sh" "$OUTPUT"
fi

SIZE=$(du -h "$OUTPUT" | cut -f1)
echo ""
echo "Recovery MSI created: $OUTPUT  ($SIZE)"
echo ""
echo "Install:  msiexec /i NyxBackup-Recovery-${VERSION}-arm64.msi"
