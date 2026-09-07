#!/usr/bin/env bash
# Copyright (c) 2026 Nyx Software, LLC
# SPDX-License-Identifier: Apache-2.0
# Nyx Backup Recovery - https://nyxbackup.com
#
# Build the standalone Nyx Backup Recovery installer for macOS Intel.
#
# Thin wrapper around scripts/macos/build_recover_pkg_arm64.sh: that
# script honours ARCH=x86_64 to retarget the cross-compiler and the
# staging path while reusing the same packaging logic.
#
# Output: dist/NyxBackup-Recovery-VERSION-x86_64.pkg

set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ARCH=x86_64 exec bash "${SCRIPT_DIR}/build_recover_pkg_arm64.sh" "$@"
