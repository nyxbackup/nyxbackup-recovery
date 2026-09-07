#!/usr/bin/env bash
# Copyright (c) 2026 Nyx Software, LLC
# SPDX-License-Identifier: Apache-2.0
# Nyx Backup Recovery - https://nyxbackup.com
#
# Authenticode-sign a Windows PE / MSI artifact (.exe / .dll / .msi) from
# Linux / WSL.  Two backends, selected by which env vars are present:
#
#   1. Azure Trusted Signing  (NYX_TS_PROFILE set)  ->  jsign  (pure Java)
#      Microsoft's managed cloud signing.  The private key never leaves
#      Microsoft's HSM; a short-lived (~72 h) cert is minted per request and
#      RFC 3161 timestamped so the signature stays valid after the leaf
#      expires.  Signs directly from Linux/WSL - no SignTool, no Windows host.
#
#   2. File-based cert  (NYX_SIGN_CERT + NYX_SIGN_KEY set)  ->  osslsigncode
#      Legacy PEM cert + key (or a PKCS#11 token via osslsigncode's own URI).
#      Idempotent: re-signing replaces any existing signature.
#
# Usage:
#   scripts/windows/sign_pe.sh <path/to/file.{exe,dll,msi}> \
#       [<cert.crt>] [<key.key>] [<rfc3161-timestamp-url>]
#
# Args 2/3 default to NYX_SIGN_CERT / NYX_SIGN_KEY (osslsigncode backend only);
# arg 4 defaults to NYX_SIGN_TIMESTAMP_URL, then a backend-appropriate TSA.
#
# -- Azure Trusted Signing env contract (jsign backend) ------------------------
#   NYX_TS_ENDPOINT   Region endpoint, bare host or URL
#                     (e.g. "weu.codesigning.azure.net").  Default region host
#                     is required - there is no sensible default.
#   NYX_TS_ACCOUNT    Trusted Signing account name.
#   NYX_TS_PROFILE    Certificate profile name.  Its PRESENCE selects this
#                     backend.  jsign alias = "<account>/<profile>".
#   Auth, one of:
#     NYX_TS_TOKEN    A pre-minted Microsoft Entra access token (scope
#                     https://codesigning.azure.net/.default).  Used verbatim.
#     AZURE_TENANT_ID + AZURE_CLIENT_ID + AZURE_CLIENT_SECRET
#                     A service principal; this script mints the token via
#                     client-credentials (curl, no Azure CLI required).
#   NYX_JSIGN_JAR     Optional path to jsign-<ver>.jar if `jsign` is not on
#                     PATH (then invoked as `java -jar <jar>`).
#
# Production note:
#   The build_windows / build_msi scripts call this automatically when either
#   backend's env vars are present.  Azure Trusted Signing certs are OV-class:
#   SmartScreen reputation accrues with download volume (not the instant
#   reputation of an EV hardware token) - budget a ramp-up window.
#
# Dev / verification usage:
#   A local self-signed test cert (~/.nyx-signing/nyx-dev-test.crt) exercises
#   the osslsigncode path only.  Self-signed certs produce real Authenticode
#   signatures but do not chain to a Microsoft-trusted CA, so end users still
#   see SmartScreen warnings.  NEVER ship a test-cert-signed binary.

set -euo pipefail

if [[ $# -lt 1 || $# -gt 4 ]]; then
    echo "Usage: $0 <path/to/file> [<cert>] [<key>] [<tsa-url>]" >&2
    exit 2
fi

INPUT="$1"

if [[ ! -f "$INPUT" ]]; then
    echo "ERROR: not a file: $INPUT" >&2
    exit 2
fi

# -- Backend selection ---------------------------------------------------------
# Trusted Signing wins when NYX_TS_PROFILE is set (it is the deliberate,
# production path); otherwise fall back to the file-based osslsigncode path.
if [[ -n "${NYX_TS_PROFILE:-}" ]]; then
    BACKEND="trustedsigning"
else
    BACKEND="osslsigncode"
fi

# ==============================================================================
# Backend 1: Azure Trusted Signing via jsign
# ==============================================================================
if [[ "$BACKEND" == "trustedsigning" ]]; then
    ENDPOINT="${NYX_TS_ENDPOINT:-}"
    ACCOUNT="${NYX_TS_ACCOUNT:-}"
    PROFILE="${NYX_TS_PROFILE:-}"
    TSA_URL="${4:-${NYX_SIGN_TIMESTAMP_URL:-http://timestamp.acs.microsoft.com}}"

    if [[ -z "$ENDPOINT" || -z "$ACCOUNT" || -z "$PROFILE" ]]; then
        echo "ERROR: Trusted Signing backend needs NYX_TS_ENDPOINT, NYX_TS_ACCOUNT," >&2
        echo "       and NYX_TS_PROFILE all set." >&2
        exit 3
    fi

    # Normalize the endpoint to a URL (jsign --keystore wants a URI).
    case "$ENDPOINT" in
        http://*|https://*) : ;;
        *) ENDPOINT="https://${ENDPOINT}" ;;
    esac

    # Locate jsign: prefer the PATH wrapper, else `java -jar $NYX_JSIGN_JAR`.
    if command -v jsign >/dev/null 2>&1; then
        JSIGN=(jsign)
    elif [[ -n "${NYX_JSIGN_JAR:-}" && -f "${NYX_JSIGN_JAR}" ]]; then
        command -v java >/dev/null 2>&1 || {
            echo "ERROR: java not found (needed to run ${NYX_JSIGN_JAR})." >&2
            echo "       sudo apt install default-jre-headless" >&2
            exit 3
        }
        JSIGN=(java -jar "${NYX_JSIGN_JAR}")
    else
        echo "ERROR: jsign not found.  Install the jsign package (puts 'jsign' on" >&2
        echo "       PATH) or set NYX_JSIGN_JAR=/path/to/jsign-<ver>.jar." >&2
        exit 3
    fi

    # Obtain the Entra access token.
    TOKEN="${NYX_TS_TOKEN:-}"
    if [[ -z "$TOKEN" ]]; then
        if [[ -z "${AZURE_TENANT_ID:-}" || -z "${AZURE_CLIENT_ID:-}" || -z "${AZURE_CLIENT_SECRET:-}" ]]; then
            echo "ERROR: no Trusted Signing token.  Set NYX_TS_TOKEN, or a service" >&2
            echo "       principal (AZURE_TENANT_ID + AZURE_CLIENT_ID + AZURE_CLIENT_SECRET)." >&2
            exit 3
        fi
        echo "Minting Entra token via service principal..."
        # Timeouts are not optional here: without them a stalled connection to
        # login.microsoftonline.com hangs the release build forever (observed:
        # a 9-minute wedge with no output), and because this runs inside the
        # build scripts there is nothing to notice it.  --retry covers the
        # transient 5xx/connection drops that make a lone attempt flaky.
        TOKEN=$(curl -sf -X POST \
            --connect-timeout 15 --max-time 60 \
            --retry 3 --retry-delay 5 --retry-connrefused \
            "https://login.microsoftonline.com/${AZURE_TENANT_ID}/oauth2/v2.0/token" \
            -d "grant_type=client_credentials" \
            --data-urlencode "client_id=${AZURE_CLIENT_ID}" \
            --data-urlencode "client_secret=${AZURE_CLIENT_SECRET}" \
            --data-urlencode "scope=https://codesigning.azure.net/.default" \
            | python3 -c 'import sys,json; print(json.load(sys.stdin)["access_token"])' 2>/dev/null || true)
        if [[ -z "$TOKEN" ]]; then
            echo "ERROR: failed to mint Entra token (check tenant/client id/secret" >&2
            echo "       and that the SP has the Trusted Signing Certificate Profile" >&2
            echo "       Signer role on the account)." >&2
            exit 4
        fi
    fi

    echo "Signing $(basename "$INPUT") via Azure Trusted Signing (${ACCOUNT}/${PROFILE})..."
    # jsign signs the file in place; re-signing replaces an existing signature.
    "${JSIGN[@]}" \
        --storetype TRUSTEDSIGNING \
        --keystore  "$ENDPOINT" \
        --storepass "$TOKEN" \
        --alias     "${ACCOUNT}/${PROFILE}" \
        --tsaurl    "$TSA_URL" \
        --tsmode    RFC3161 \
        --name      "Nyx Backup Recovery" \
        --url       "https://nyxbackup.com/download-recovery" \
        "$INPUT"

    # Best-effort confirmation if osslsigncode is available (jsign has no verb).
    if command -v osslsigncode >/dev/null 2>&1; then
        if osslsigncode verify "$INPUT" 2>&1 | grep -q "Signature verification: ok"; then
            echo "  -> signature verified (chain trusted)"
        else
            echo "  -> signature embedded (osslsigncode could not confirm chain trust here)"
        fi
    else
        echo "  -> signed (install osslsigncode to verify locally)"
    fi
    exit 0
fi

# ==============================================================================
# Backend 2: file-based cert via osslsigncode  (unchanged legacy path)
# ==============================================================================
CERT="${2:-${NYX_SIGN_CERT:-}}"
KEY="${3:-${NYX_SIGN_KEY:-}}"
TSA_URL="${4:-${NYX_SIGN_TIMESTAMP_URL:-http://timestamp.digicert.com}}"

if [[ -z "$CERT" || -z "$KEY" ]]; then
    echo "ERROR: missing cert/key.  Either pass them as args, set" >&2
    echo "       NYX_SIGN_CERT and NYX_SIGN_KEY, or use the Trusted Signing" >&2
    echo "       backend (set NYX_TS_PROFILE + NYX_TS_ACCOUNT + NYX_TS_ENDPOINT)." >&2
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
    -n       "Nyx Backup Recovery" \
    -i       "https://nyxbackup.com/download-recovery" \
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
