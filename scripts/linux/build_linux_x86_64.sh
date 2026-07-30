#!/usr/bin/env bash
# Copyright (c) 2026 Nyx Software, LLC
# SPDX-License-Identifier: Apache-2.0
# Nyx Backup Recovery - https://nyxbackup.com
# Build the standalone Nyx Backup Recovery binary for Linux x86-64 and stage it.
#
# Requirements (Ubuntu/Debian):
#   sudo apt install libssh2-1-dev libdbus-1-dev pkg-config libsmbclient-dev
#   sudo apt install libwebkit2gtk-4.1-dev libgtk-3-dev \
#                    libayatana-appindicator3-dev librsvg2-dev \
#                    libsoup-3.0-dev libjavascriptcoregtk-4.1-dev
#   cargo install cargo-zigbuild   # for glibc version targeting
#
# Output: staging/linux/x86_64/   (nyx_bkp_recover + locales, ready for
#         scripts/linux/build_recover_deb_x86_64.sh)
#
# GLIBC COMPATIBILITY: this is a native build, so the binary requires a glibc
# at least as new as the build host's.  Release builds must therefore run on
# the OLDEST supported host - Ubuntu 22.04 (glibc 2.35), which is also the
# oldest distro shipping libwebkit2gtk-4.1 (required by Tauri 2).  Building on
# 24.04 pulls in GLIBC_2.38/2.39 symbols and the .deb then refuses to run on
# Ubuntu 22.04 / Debian 12.  build_recover_deb_x86_64.sh enforces the ceiling.

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

# Honour CARGO_TARGET_DIR so a release build on the glibc-2.35 host can keep
# its artifacts out of the dev host's target/.  Sharing one target/ between
# hosts of different glibc vintages is what silently produces a binary with
# mixed ABI requirements: cargo fingerprints rustc flags, not the C toolchain
# that build scripts (vendored OpenSSL, libssh2-sys) compile against.
TARGET_ROOT="${CARGO_TARGET_DIR:-${WORKSPACE_DIR}/target}"

# -- Preflight checks --------------------------------------------------------
check_cmd() { command -v "$1" >/dev/null 2>&1 || { echo "ERROR: $1 not found. $2"; exit 1; }; }
check_cmd cargo "Install Rust: https://rustup.rs"
check_cmd node  "Install Node: sudo apt install nodejs"
check_cmd npm   "Install npm:  sudo apt install npm"

# The GUI -dev packages are not co-installable across architectures: a host
# that last built the arm64 .deb has had its :amd64 webkit/gtk -dev packages
# evicted by the :arm64 ones.  Flip them back rather than making the caller
# figure it out.  See scripts/dev/switch_gui_dev_arch.sh (NYX_NO_AUTO_DEPS=1
# turns the automatic install off).
if ! pkg-config --exists webkit2gtk-4.1 2>/dev/null; then
    bash "${SCRIPT_DIR}/../dev/switch_gui_dev_arch.sh" host || {
        echo "ERROR: webkit2gtk-4.1 dev packages not found - required for the GUI."
        echo "  sudo apt install libwebkit2gtk-4.1-dev libgtk-3-dev \\"
        echo "                   librsvg2-dev libsoup-3.0-dev \\"
        echo "                   libjavascriptcoregtk-4.1-dev"
        exit 1
    }
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
STAMP="${TARGET_ROOT}/.recover_version_stamp_linux"
if [[ ! -f "$STAMP" || "$(cat "$STAMP" 2>/dev/null)" != "$WORKSPACE_VER" ]]; then
    echo "Version changed -> clean bkp-recover to re-stamp ${WORKSPACE_VER}..."
    cargo clean -p bkp-recover --profile "$PROFILE" 2>/dev/null || true
    mkdir -p "$TARGET_ROOT"; echo "$WORKSPACE_VER" > "$STAMP"
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

# .cargo/config.toml links this target with mold (-fuse-ld=mold) for fast dev
# iteration.  mold is not packaged on every supported build host (notably
# Ubuntu 22.04, the glibc-2.35 release host), and gcc hard-errors with
# "collect2: fatal error: cannot find 'ld'" instead of falling back.
#
# RUSTFLAGS is the only override that actually drops the flag: it replaces all
# config-file rustflags wholesale.  CARGO_TARGET_<TRIPLE>_RUSTFLAGS does NOT -
# it is the env spelling of the same config key and cargo joins the two, so
# the mold flag survives (and an empty value is treated as unset entirely).
# The replacement value must stay non-empty for the same reason; --as-needed
# is a no-op here since gcc passes it anyway.  Linker choice does not affect
# the produced ABI.
if ! command -v mold >/dev/null 2>&1; then
    echo "mold not found - linking with default ld."
    export RUSTFLAGS="-C link-arg=-Wl,--as-needed"
fi

echo "Building nyx_bkp_recover for ${TARGET_BASE} (${PROFILE})..."
cargo build $CARGO_FLAGS -p bkp-recover --bin nyx_bkp_recover

# -- Stage -------------------------------------------------------------------
echo "Staging files..."
RELEASE_DIR="${TARGET_ROOT}/${TARGET_BASE}/${PROFILE}"
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
