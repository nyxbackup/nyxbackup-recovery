# Copyright (c) 2026 Nyx Software, LLC
# SPDX-License-Identifier: Apache-2.0
# Nyx Backup Recovery - https://nyxbackup.com
# CMake toolchain file for cross-compiling to Windows ARM64 with llvm-mingw.
# Used by scripts/windows/build_windows_arm64.sh when building libssh2 from
# source.
#
# Unlike the x86-64 toolchain (which uses the system MinGW GCC), there is no
# system aarch64 Windows GCC on Linux, so the compilers come from llvm-mingw.
# The build script exports LLVM_MINGW so the absolute paths resolve to the
# user-chosen install location.

set(CMAKE_SYSTEM_NAME Windows)
set(CMAKE_SYSTEM_PROCESSOR ARM64)

# LLVM_MINGW is exported by the calling build script (default /opt/llvm-mingw).
if(NOT DEFINED ENV{LLVM_MINGW})
    message(FATAL_ERROR "LLVM_MINGW environment variable not set")
endif()
set(LLVM_MINGW_BIN "$ENV{LLVM_MINGW}/bin")

set(CMAKE_C_COMPILER   "${LLVM_MINGW_BIN}/aarch64-w64-mingw32-gcc")
set(CMAKE_CXX_COMPILER "${LLVM_MINGW_BIN}/aarch64-w64-mingw32-g++")
set(CMAKE_RC_COMPILER  "${LLVM_MINGW_BIN}/aarch64-w64-mingw32-windres")

set(CMAKE_FIND_ROOT_PATH "$ENV{LLVM_MINGW}/aarch64-w64-mingw32")
set(CMAKE_FIND_ROOT_PATH_MODE_PROGRAM NEVER)
set(CMAKE_FIND_ROOT_PATH_MODE_LIBRARY ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_INCLUDE ONLY)
