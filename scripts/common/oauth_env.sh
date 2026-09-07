#!/usr/bin/env bash
# Shared OAuth build-secret loader, sourced by every build_* script that
# compiles the recover binary.
#
# The recover binary embeds its OAuth app credentials at COMPILE time via
# `env!()` (not `option_env!`), so a missing variable must fail the build
# loudly rather than produce a binary whose cloud sign-in is silently inert.
# These guards are that check.
#
# Why this file exists: the block below used to be copy-pasted into six build
# scripts.  Two of them - build_recover_pkg_arm64.sh and
# build_recover_pkg_universal.sh - are 325-line near-duplicates, and that
# duplication has already caused a real defect: the macOS signing fixes were
# applied to the arm64 script only and the universal script shipped without
# them, which was reported as done before anyone checked the second file.
# One definition means a fix cannot land in only half the builds.
#
# Precedence: a real environment variable WINS over the .env file.  Only
# unset/empty names are filled in from .env.  That is what lets CI inject
# placeholder credentials, and a caller override a single secret for one
# build, without editing a gitignored file.  (The older `set -a; source .env`
# form did the opposite - .env clobbered the environment.)
#
# .env is gitignored and holds real secrets; nothing here echoes a value.
#
# Usage:
#   WORKSPACE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
#   source "${WORKSPACE_DIR}/scripts/common/oauth_env.sh"

if [[ -z "${WORKSPACE_DIR:-}" ]]; then
    echo "ERROR: oauth_env.sh requires WORKSPACE_DIR to be set before sourcing." >&2
    exit 1
fi

if [[ -f "${WORKSPACE_DIR}/.env" ]]; then
    while IFS= read -r _line; do
        case "${_line}" in
            ''|'#'*) continue ;;
            *=*)
                _k="${_line%%=*}"
                # Skip anything that is not a plain shell name (guards against
                # a stray line in .env being turned into an export).
                case "${_k}" in *[!A-Za-z0-9_]*) continue ;; esac
                if [[ -z "${!_k:-}" ]]; then
                    _v="${_line#*=}"
                    _v="${_v%\"}"; _v="${_v#\"}"
                    export "${_k}=${_v}"
                fi
                ;;
        esac
    done < "${WORKSPACE_DIR}/.env"
    unset _line _k _v
fi

# Exactly the set the recover binary embeds via env!().  Kept in sync with
# `grep -rn 'env!("' crates/` - adding an env!() without adding it here means
# the failure moves from this line to a confusing rustc error, or worse, to a
# shipped binary that cannot sign in.
#
# ONEDRIVE_OAUTH_CLIENT_SECRET is deliberately NOT here: OneDrive is a PKCE
# public client in this app and no secret is compiled in.  Verified against
# the env!() call sites, not assumed.
: "${GOOGLE_OAUTH_CLIENT_ID:?Set GOOGLE_OAUTH_CLIENT_ID in .env or the environment}"
: "${GOOGLE_OAUTH_CLIENT_SECRET:?Set GOOGLE_OAUTH_CLIENT_SECRET in .env or the environment}"
: "${DROPBOX_APP_KEY:?Set DROPBOX_APP_KEY in .env or the environment}"
: "${DROPBOX_APP_SECRET:?Set DROPBOX_APP_SECRET in .env or the environment}"
: "${ONEDRIVE_OAUTH_CLIENT_ID:?Set ONEDRIVE_OAUTH_CLIENT_ID in .env or the environment}"
