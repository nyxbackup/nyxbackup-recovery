#!/usr/bin/env python3
# Copyright (c) 2026 Nyx Software, LLC. All rights reserved.
# Nyx Backup - https://nyxbackup.com
"""Fail the build when a shipped binary needs a library the package does not
provide.

Why this exists
---------------
0.9.72 shipped a Windows ARM64 MSI in which all five executables imported
libunwind.dll, and the MSI did not contain that file.  Nothing runs: the
service fails to start, the CLI exits silently, and only the GUI reaches a
dialog ("The code execution cannot proceed because libunwind.dll was not
found").  It survived several signed, published releases because the x86_64
target links its unwinder statically and never needed the file, so every
check that passed, passed on the architecture that could not catch it.

The dependency was visible the whole time, in the binaries themselves.  This
script reads it: PE import tables on Windows, ELF DT_NEEDED on Linux, compared
against a pinned manifest of what each platform is expected to need.

Pinned, not inferred
--------------------
The manifest lists the exact dependency set.  Anything outside it fails the
build, including a NEW dependency that is perfectly satisfiable - because a new
entry appearing is precisely the event worth a human look.  Updating the pin is
one line; discovering the omission on a customer's machine is not.

Usage
-----
    check_runtime_deps.py windows x86_64 staging/windows/x86_64
    check_runtime_deps.py linux   arm64  staging/linux/arm64

Exit codes: 0 all dependencies accounted for   1 usage error   2 unmet
dependency (the build must stop).
"""

import json
import os
import struct
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
MANIFEST = os.path.join(HERE, "runtime_deps.json")


# ---------------------------------------------------------------- PE (Windows)

def pe_imports(path):
    """Return the DLL names in a PE file's import directory.

    Returns None when the file is not a PE image, so callers can skip
    non-binaries without special-casing them by extension.
    """
    try:
        with open(path, "rb") as fh:
            d = fh.read()
    except OSError:
        return None
    if len(d) < 0x40 or d[:2] != b"MZ":
        return None
    pe = struct.unpack_from("<I", d, 0x3C)[0]
    if pe + 24 > len(d) or d[pe:pe + 4] != b"PE\0\0":
        return None

    nsec = struct.unpack_from("<H", d, pe + 6)[0]
    optsz = struct.unpack_from("<H", d, pe + 20)[0]
    opt = pe + 24
    magic = struct.unpack_from("<H", d, opt)[0]
    # Data directories start after the optional header's fixed part: 112 bytes
    # for PE32+ (x64/ARM64), 96 for PE32.  Entry 1 is the import directory.
    ddoff = opt + (112 if magic == 0x20B else 96)
    imp_rva = struct.unpack_from("<I", d, ddoff + 8)[0]
    if imp_rva == 0:
        return []

    secs = []
    sect = opt + optsz
    for i in range(nsec):
        s = sect + i * 40
        vsize = struct.unpack_from("<I", d, s + 8)[0]
        vaddr = struct.unpack_from("<I", d, s + 12)[0]
        rawsz = struct.unpack_from("<I", d, s + 16)[0]
        rawptr = struct.unpack_from("<I", d, s + 20)[0]
        secs.append((vaddr, vsize, rawsz, rawptr))

    def to_offset(rva):
        for vaddr, vsize, rawsz, rawptr in secs:
            if vaddr <= rva < vaddr + max(vsize, rawsz):
                return rawptr + (rva - vaddr)
        return None

    out = []
    p = to_offset(imp_rva)
    if p is None:
        return []
    while p + 20 <= len(d):
        desc = d[p:p + 20]
        if desc == b"\0" * 20:
            break
        name_rva = struct.unpack_from("<I", desc, 12)[0]
        if name_rva == 0:
            break
        off = to_offset(name_rva)
        if off is None:
            break
        end = d.index(b"\0", off)
        out.append(d[off:end].decode("ascii", "replace"))
        p += 20
    return out


# ------------------------------------------------------------------ ELF (Linux)

def elf_needed(path):
    """Return the DT_NEEDED entries of a 64-bit ELF, or None if not an ELF.

    A package tree contains symlinks into the installed layout (/usr/bin/... ->
    /usr/lib/nyxbackup/...) which dangle when the tree is only staged, so an
    unreadable path is skipped rather than fatal.
    """
    try:
        with open(path, "rb") as fh:
            d = fh.read()
    except OSError:
        return None
    if len(d) < 64 or d[:4] != b"\x7fELF" or d[4] != 2:
        return None

    e_shoff = struct.unpack_from("<Q", d, 0x28)[0]
    e_shentsize = struct.unpack_from("<H", d, 0x3A)[0]
    e_shnum = struct.unpack_from("<H", d, 0x3C)[0]

    secs = []
    dynamic = None
    for i in range(e_shnum):
        s = e_shoff + i * e_shentsize
        if s + 0x40 > len(d):
            return []
        sh_type = struct.unpack_from("<I", d, s + 4)[0]
        sh_offset = struct.unpack_from("<Q", d, s + 0x18)[0]
        sh_size = struct.unpack_from("<Q", d, s + 0x20)[0]
        sh_link = struct.unpack_from("<I", d, s + 0x28)[0]
        secs.append((sh_type, sh_offset, sh_size, sh_link))
        if sh_type == 6:  # SHT_DYNAMIC
            dynamic = (sh_offset, sh_size, sh_link)

    if dynamic is None:
        return []
    off, size, link = dynamic
    if link >= len(secs):
        return []
    stroff = secs[link][1]

    out = []
    for p in range(off, min(off + size, len(d) - 15), 16):
        tag, val = struct.unpack_from("<QQ", d, p)
        if tag == 0:  # DT_NULL
            break
        if tag == 1:  # DT_NEEDED
            end = d.index(b"\0", stroff + val)
            out.append(d[stroff + val:end].decode("ascii", "replace"))
    return out


# ------------------------------------------------------------------------ checks

def check_windows(stage, cfg, arch_cfg):
    """Every imported DLL must be a Windows system DLL or present in staging."""
    system = {s.lower() for s in cfg["system_dlls"]}
    prefixes = tuple(p.lower() for p in cfg.get("system_dll_prefixes", []))
    staged = {f.lower() for f in os.listdir(stage)}
    problems, checked = [], 0

    for name in sorted(os.listdir(stage)):
        path = os.path.join(stage, name)
        if not os.path.isfile(path):
            continue
        imports = pe_imports(path)
        if imports is None:
            continue
        checked += 1
        for dll in imports:
            low = dll.lower()
            if low in system or low.startswith(prefixes) or low in staged:
                continue
            problems.append(
                "  %-24s imports %s - not a Windows system DLL and not in staging"
                % (name, dll))

    # A file we promise to bundle but did not stage is the same failure seen
    # from the other side, and it is silent until a machine runs the package.
    for dll in arch_cfg.get("bundled", []):
        if dll.lower() not in staged:
            problems.append("  MISSING from staging: %s (manifest says it must ship)" % dll)

    return checked, problems


def check_linux(stage, cfg, arch_cfg):
    """Every DT_NEEDED entry must be in the pinned allowed set."""
    allowed = {s for s in cfg["base_libs"]} | {s for s in arch_cfg.get("extra_libs", [])}
    problems, checked = [], 0

    for root, _dirs, files in os.walk(stage):
        for name in sorted(files):
            path = os.path.join(root, name)
            needed = elf_needed(path)
            if needed is None:
                continue
            checked += 1
            for lib in needed:
                if lib in allowed:
                    continue
                problems.append(
                    "  %-24s needs %s - not in the pinned dependency set"
                    % (name, lib))

    return checked, problems


def main(argv):
    if len(argv) != 4:
        sys.stderr.write(__doc__)
        return 1
    platform, arch, stage = argv[1], argv[2], argv[3]

    if not os.path.isdir(stage):
        sys.stderr.write("ERROR: staging directory not found: %s\n" % stage)
        return 1
    with open(MANIFEST) as fh:
        manifest = json.load(fh)
    if platform not in manifest or arch not in manifest[platform]:
        sys.stderr.write(
            "ERROR: no manifest entry for %s/%s in %s\n" % (platform, arch, MANIFEST))
        return 1

    cfg = manifest[platform]
    arch_cfg = cfg[arch]
    if platform == "windows":
        checked, problems = check_windows(stage, cfg, arch_cfg)
    elif platform == "linux":
        checked, problems = check_linux(stage, cfg, arch_cfg)
    else:
        sys.stderr.write("ERROR: unknown platform '%s'\n" % platform)
        return 1

    if problems:
        sys.stderr.write(
            "\nRUNTIME DEPENDENCY CHECK FAILED (%s/%s, %d binaries)\n\n"
            % (platform, arch, checked))
        for p in problems:
            sys.stderr.write(p + "\n")
        sys.stderr.write(
            "\nEither stage the missing file alongside the binaries and add it to the\n"
            "installer, or - if the dependency is genuinely provided by the target\n"
            "system - add it to %s.\n"
            "Do not silence this by deleting the check: a missing runtime library is\n"
            "not detectable until the package runs on a real machine.\n\n" % MANIFEST)
        return 2

    print("runtime deps OK: %d binaries checked (%s/%s)" % (checked, platform, arch))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
