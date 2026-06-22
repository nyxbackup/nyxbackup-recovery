#!/usr/bin/env bash
# Copyright (c) 2026 Nyx Software, LLC
# SPDX-License-Identifier: Apache-2.0
# Nyx Backup Recovery - https://nyxbackup.com
# Build the standalone Nyx Backup Recovery binary for Linux x86-64 and stage it.
#
# Requirements (Ubuntu/Debian):
#   sudo apt install libssh2-1-dev libdbus-1-dev pkg-config
#   sudo apt install libwebkit2gtk-4.1-dev libgtk-3-dev \
#                    libayatana-appindicator3-dev librsvg2-dev \
#                    libsoup-3.0-dev libjavascriptcoregtk-4.1-dev
#   cargo install cargo-zigbuild   # for glibc version targeting
#
# Output: staging/linux/x86_64/   (nyx_bkp_recover + locales, ready for
#         scripts/linux/build_recover_deb_x86_64.sh)

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

TARGET_BASE="x86_64-unknown-linux-gnu"
PROFILE="${PROFILE:-release}"
STAGING="${WORKSPACE_DIR}/staging/linux/x86_64"

# -- Preflight checks --------------------------------------------------------
check_cmd() { command -v "$1" >/dev/null 2>&1 || { echo "ERROR: $1 not found. $2"; exit 1; }; }
check_cmd cargo "Install Rust: https://rustup.rs"
check_cmd node  "Install Node: sudo apt install nodejs"
check_cmd npm   "Install npm:  sudo apt install npm"

if ! pkg-config --exists webkit2gtk-4.1 2>/dev/null; then
    echo "ERROR: webkit2gtk-4.1 dev packages not found - required for the GUI."
    echo "  sudo apt install libwebkit2gtk-4.1-dev libgtk-3-dev \\"
    echo "                   libayatana-appindicator3-dev librsvg2-dev"
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
STAMP="${WORKSPACE_DIR}/target/.recover_version_stamp_linux"
if [[ ! -f "$STAMP" || "$(cat "$STAMP" 2>/dev/null)" != "$WORKSPACE_VER" ]]; then
    echo "Version changed -> clean bkp-recover to re-stamp ${WORKSPACE_VER}..."
    cargo clean -p bkp-recover --profile "$PROFILE" 2>/dev/null || true
    mkdir -p "${WORKSPACE_DIR}/target"; echo "$WORKSPACE_VER" > "$STAMP"
fi

# -- Cargo build -------------------------------------------------------------
# Native build (no cargo-zigbuild glibc cap).  zigbuild's 2.35 cap collides
# with openssl-sys's vendored libcrypto referencing the host's C23
# `__isoc23_strtol` symbol, producing an undefined-symbol link error (the
# same issue the upstream monorepo documents and soft-fails for the recovery
# binary).  Building natively links against the host glibc and avoids it.
# Trade-off: the resulting binary requires a glibc at least as new as this
# build host's.  For wider distro compatibility, build on an older glibc
# host (e.g. Ubuntu 22.04) or vendor a fixed libcrypto.  The native Linux
# linker (mold) is configured in .cargo/config.toml.
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
echo "Next: scripts/linux/build_recover_deb_x86_64.sh"
