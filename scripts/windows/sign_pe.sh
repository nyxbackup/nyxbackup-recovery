#!/usr/bin/env bash
# Copyright (c) 2026 Nyx Software, LLC
# SPDX-License-Identifier: Apache-2.0
# Nyx Backup Recovery - https://nyxbackup.com
#
# Authenticode-sign a Windows PE binary (.exe / .dll / .msi) from Linux/WSL
# using osslsigncode.  Idempotent: a file can be re-signed (existing
# signature is replaced; osslsigncode handles the PE container surgery).
#
# Requirements:
#   sudo apt install osslsigncode
#
# Usage:
#   scripts/windows/sign_pe.sh <path/to/file.{exe,dll,msi}> [<cert.crt>] [<key.key>] [<rfc3161-timestamp-url>]
#
# Argument 2/3 default to NYX_SIGN_CERT and NYX_SIGN_KEY env vars; argument 4
# defaults to NYX_SIGN_TIMESTAMP_URL or http://timestamp.digicert.com .
#
# Production usage:
#   The release build sets NYX_SIGN_CERT / NYX_SIGN_KEY to the production
#   code-signing cert (Sectigo / DigiCert / Azure Trusted Signing).  The
#   build_windows scripts call this script automatically when those env
#   vars are present.
#
# Dev / verification usage:
#   A local self-signed test cert (~/.nyx-signing/nyx-dev-test.crt) was
#   generated 2026-06-06 for pipeline testing only.  Self-signed certs
#   produce real Authenticode signatures but do not chain to a
#   Microsoft-trusted CA, so end users would still see SmartScreen
#   warnings.  Acceptable for internal validation; NEVER ship a binary
#   signed with the test cert to end users.

set -euo pipefail

if [[ $# -lt 1 || $# -gt 4 ]]; then
    echo "Usage: $0 <path/to/file> [<cert>] [<key>] [<tsa-url>]" >&2
    exit 2
fi

INPUT="$1"
CERT="${2:-${NYX_SIGN_CERT:-}}"
KEY="${3:-${NYX_SIGN_KEY:-}}"
TSA_URL="${4:-${NYX_SIGN_TIMESTAMP_URL:-http://timestamp.digicert.com}}"

if [[ ! -f "$INPUT" ]]; then
    echo "ERROR: not a file: $INPUT" >&2
    exit 2
fi

if [[ -z "$CERT" || -z "$KEY" ]]; then
    echo "ERROR: missing cert/key.  Either pass them as args or set" >&2
    echo "       NYX_SIGN_CERT and NYX_SIGN_KEY env vars." >&2
    exit 3
fi

if [[ ! -f "$CERT" || ! -f "$KEY" ]]; then
    echo "ERROR: cert or key file missing: $CERT / $KEY" >&2
    exit 3
fi

if ! command -v osslsigncode >/dev/null 2>&1; then
    echo "ERROR: osslsigncode not installed.  Install with: sudo apt install osslsigncode" >&2
    exit 3
fi

# osslsigncode takes the file via -in and writes to -out; for in-place
# replacement we sign to a temp file then atomic-rename.
TMP="${INPUT}.signed.tmp"
trap 'rm -f "$TMP"' EXIT

# `-h sha256` is the Authenticode digest algorithm.
# `-t` uses the legacy Authenticode timestamp endpoint; `-ts` uses RFC 3161.
# DigiCert's URL works for both, but RFC 3161 is preferred by SmartScreen.
echo "Signing $(basename "$INPUT")..."
osslsigncode sign \
    -certs   "$CERT" \
    -key     "$KEY" \
    -h       sha256 \
    -n       "Nyx Backup" \
    -i       "https://nyxbackup.com" \
    -ts      "$TSA_URL" \
    -in      "$INPUT" \
    -out     "$TMP"

mv "$TMP" "$INPUT"
trap - EXIT

# Confirm signature is present.
SIG_INFO=$(osslsigncode verify "$INPUT" 2>&1 || true)
if echo "$SIG_INFO" | grep -q "Signature verification: ok"; then
    echo "  -> signature verified (cert chain may be self-signed in dev)"
elif echo "$SIG_INFO" | grep -q "Certificate not trusted"; then
    echo "  -> signature embedded; chain not trusted (expected for self-signed dev cert)"
else
    echo "  -> signature embedded (verify output below)"
    echo "$SIG_INFO" | head -10 | sed 's/^/     /'
fi
