#!/usr/bin/env bash
# Copyright (c) 2026 Nyx Software, LLC
# SPDX-License-Identifier: Apache-2.0
# Nyx Backup Recovery - https://nyxbackup.com
# Build the standalone Nyx Backup Recovery MSI.
#
# Assumes scripts/windows/build_windows_x86_64.sh has already run so that
# staging/windows/x86_64/nyx_bkp_recover.exe + WebView2Loader.dll exist.
# Pass --build to chain a fresh build first.
#
# Output: dist/NyxBackup-Recovery-VERSION-x86_64.msi  (~33 MB - much
# smaller than the main MSI; the recovery binary plus WebView2Loader is
# all it ships).

set -euo pipefail

WORKSPACE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
STAGING="${WORKSPACE_DIR}/staging/windows/x86_64"
WXS="${WORKSPACE_DIR}/installer/windows/NyxBackupRecovery-x86_64.wxs"
DIST="${WORKSPACE_DIR}/dist"
DO_BUILD=0

for arg in "$@"; do
    case "$arg" in
        --build) DO_BUILD=1 ;;
        *) echo "Unknown argument: $arg"; exit 1 ;;
    esac
done

VERSION=$(cat "$WORKSPACE_DIR/VERSION" | tr -d '[:space:]')
echo "Building Recovery MSI for Nyx Backup v${VERSION}..."

command -v wixl >/dev/null 2>&1 || {
    echo "ERROR: wixl not found.  Install msitools: sudo apt install msitools"
    exit 1
}

if [[ $DO_BUILD -eq 1 ]]; then
    bash "${WORKSPACE_DIR}/scripts/windows/build_windows_x86_64.sh"
fi

for f in nyx_bkp_recover.exe WebView2Loader.dll; do
    [[ -f "$STAGING/$f" ]] || {
        echo "ERROR: missing $STAGING/$f"
        echo "  Run with --build, or run scripts/windows/build_windows_x86_64.sh first."
        exit 1
    }
done

# Same wixl-vs-DOS-timezone dance the main MSI does.  See build_msi_x86_64.sh
# for the rationale comment.
WIN_TIME=$(powershell.exe -Command "Get-Date -Format 'yyyyMMddHHmm'" 2>/dev/null | tr -d '\r\n')
if [[ -n "$WIN_TIME" ]]; then
    ( export TZ=UTC
      touch -t "$WIN_TIME" "$STAGING/nyx_bkp_recover.exe" "$STAGING/WebView2Loader.dll" )
fi

mkdir -p "$DIST"
OUTPUT="${DIST}/NyxBackup-Recovery-${VERSION}-x86_64.msi"

wixl \
    --arch x64 \
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
echo "Install:  msiexec /i NyxBackup-Recovery-${VERSION}-x86_64.msi"
