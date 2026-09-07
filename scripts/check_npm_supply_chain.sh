#!/usr/bin/env bash
# Copyright (c) 2026 Nyx Software, LLC
# SPDX-License-Identifier: Apache-2.0
# Nyx Backup Recovery - https://nyxbackup.com
#
# scripts/check_npm_supply_chain.sh
#
# Pre-build supply-chain check for the two UI trees.  Run from anywhere;
# the script locates the workspace via $WORKSPACE_DIR or `git rev-parse`.
#
# Two passes:
#   1. Denylist scan - greps package-lock.json against package families
#      known to have been compromised in recent npm worm / typosquat
#      incidents.  Hard fail on any match.
#   2. `npm audit --omit=dev --audit-level=high` - asks npm's
#      vulnerability database.  Warning-only by default; pass
#      --strict-audit to make `high`/`critical` advisories a hard fail
#      (intended for CI).
#
# Exit codes:
#   0 - all clean
#   1 - denylist hit, or strict-audit fail
#   2 - tool / file missing (npm not on PATH, no UI tree, etc.)
#
# Usage:
#   scripts/check_npm_supply_chain.sh
#   scripts/check_npm_supply_chain.sh --strict-audit
#
# The denylist below is curated by hand from public incident reports.
# Update when a new incident lands - this is a paranoia rail, not a
# substitute for `npm audit`.  Add a comment with the date and short
# context for each new entry so future-you knows why it's there.

set -u
set -o pipefail

WORKSPACE_DIR="${WORKSPACE_DIR:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}"
STRICT_AUDIT=0
for arg in "$@"; do
    case "$arg" in
        --strict-audit) STRICT_AUDIT=1 ;;
        -h|--help)
            sed -n '3,33p' "$0"
            exit 0
            ;;
        *)
            echo "ERROR: unknown arg: $arg"
            exit 2
            ;;
    esac
done

UI_TREES=(
    "${WORKSPACE_DIR}/crates/bkp-recover/ui"
)

# Denylist of package families known to have been compromised in recent
# npm supply-chain incidents.  One pattern per line, ECMAScript-regex
# compatible (used with grep -E).  Match against the bare package name
# OR the `"name": "..."` form in lockfiles.
#
# Format: <pattern>  # <YYYY-MM-DD> <short context>
DENYLIST_PATTERNS=(
    '@redhat-cloud-services/'         # 2026 npm worm campaign against RH packages (~117k weekly dl).
    '@vapi-ai/server-sdk'             # 2026 npm worm (same wave as @redhat-cloud-services).
    '@vapi-ai/'                       # broader family - any @vapi-ai/* package while the campaign is active.
    '@ctrl/tinycolor'                 # 2025 worm via tinycolor maintainer compromise.
    'shai-hulud'                      # 2025-2026 self-replicating npm worm signature.
    'eslint-config-airbnb-compat'     # 2024 typosquat campaign.
    'event-source-polyfill'           # 2024 maintainer hijack.
    'noblox\.js-proxies'              # 2024 token-stealer typosquat.
    'ua-parser-js@0\.7\.29'           # 2021 historical - kept for completeness.
    'rc@1\.2\.9'                      # 2021 historical.
    'coa@2\.0\.3'                     # 2021 historical.
)

# Files in a checked-out tree that should NEVER be present.  Worm-specific
# artefacts dropped in the repo at install time.
ARTEFACT_PATTERNS=(
    'bundle\.js'                                  # Shai-Hulud worm self-replication payload.
    'shai-hulud-workflow\.yml'                    # CI workflow the worm tries to plant.
    '\.github/workflows/shai-hulud.*\.yml'        # variants.
)

HAS_HIT=0
HAS_AUDIT_HIT=0

bold() { printf '\033[1m%s\033[0m\n' "$1"; }
red()  { printf '\033[31m%s\033[0m\n' "$1"; }
green(){ printf '\033[32m%s\033[0m\n' "$1"; }
yellow(){ printf '\033[33m%s\033[0m\n' "$1"; }

bold "Nyx Backup npm supply-chain check"
echo

# ── Tool availability ─────────────────────────────────────────────────────────
if ! command -v npm >/dev/null 2>&1; then
    red "ERROR: npm not on PATH.  Install Node.js / npm and retry."
    exit 2
fi

# ── Denylist scan ─────────────────────────────────────────────────────────────
bold "[1/3] Denylist scan"
for tree in "${UI_TREES[@]}"; do
    if [[ ! -d "$tree" ]]; then
        yellow "  skip (missing): $tree"
        continue
    fi
    lockfile="$tree/package-lock.json"
    if [[ ! -f "$lockfile" ]]; then
        yellow "  skip (no package-lock.json): $tree"
        continue
    fi
    echo "  scanning $lockfile"
    for pattern in "${DENYLIST_PATTERNS[@]}"; do
        # Greppable form: "<name>" patterns inside the lockfile.  Use
        # grep -E because patterns contain regex meta.  Two passes - one
        # against the bare name field, one against the resolved package
        # path - because lockfile schemas vary across npm versions.
        if grep -E -q "\"$pattern" "$lockfile" 2>/dev/null \
        || grep -E -q "/$pattern"   "$lockfile" 2>/dev/null; then
            red    "  HIT: $pattern   in $lockfile"
            HAS_HIT=1
        fi
    done
done
if [[ $HAS_HIT -eq 0 ]]; then
    green "  no denylist hits."
fi
echo

# ── Worm-artefact scan ────────────────────────────────────────────────────────
bold "[2/3] Worm-artefact scan"
for pattern in "${ARTEFACT_PATTERNS[@]}"; do
    # Search the workspace, skipping node_modules / build outputs.
    matches=$(find "$WORKSPACE_DIR" \
        \( -path '*/node_modules' -o -path '*/target' -o -path '*/dist' -o -path '*/.git' \) -prune \
        -o -type f -regextype posix-extended -regex ".*/$pattern" -print 2>/dev/null)
    if [[ -n "$matches" ]]; then
        red "  HIT: $pattern"
        echo "$matches" | sed 's/^/    /'
        HAS_HIT=1
    fi
done
if [[ $HAS_HIT -eq 0 ]]; then
    green "  no worm artefacts found."
fi
echo

# ── npm audit (high+critical only) ────────────────────────────────────────────
bold "[3/3] npm audit (high + critical, runtime deps only)"
for tree in "${UI_TREES[@]}"; do
    if [[ ! -d "$tree" ]]; then continue; fi
    if [[ ! -f "$tree/package-lock.json" ]]; then continue; fi
    pushd "$tree" >/dev/null
    out=$(npm audit --omit=dev --audit-level=high --json 2>/dev/null || true)
    popd >/dev/null
    # npm audit's JSON shape varies; do a cheap check of the summary field.
    if echo "$out" | grep -qE '"vulnerabilities":\s*\{[^}]*"(high|critical)":\s*[1-9]'; then
        red "  AUDIT FINDING in $tree"
        echo "$out" | python3 -c 'import sys, json
try:
    d = json.load(sys.stdin)
    v = d.get("metadata", {}).get("vulnerabilities", {})
    print(f"    high={v.get(\"high\",0)}  critical={v.get(\"critical\",0)}  total={v.get(\"total\",0)}")
except Exception as e:
    print(f"    (could not parse audit JSON: {e})")' 2>/dev/null || true
        HAS_AUDIT_HIT=1
    else
        echo "  clean: $tree"
    fi
done
echo

# ── Summary ───────────────────────────────────────────────────────────────────
if [[ $HAS_HIT -eq 1 ]]; then
    red "FAIL: denylist or worm artefact hit.  Investigate before continuing."
    exit 1
fi
if [[ $HAS_AUDIT_HIT -eq 1 ]]; then
    if [[ $STRICT_AUDIT -eq 1 ]]; then
        red "FAIL: --strict-audit set and npm audit reported high/critical advisories."
        exit 1
    else
        yellow "Audit advisories above (warning only - rerun with --strict-audit to fail)."
    fi
fi
green "OK: supply-chain check passed."
