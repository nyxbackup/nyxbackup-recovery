# Building the ARM64 installers

Nyx Backup Recovery ships ARM64 installers alongside x86-64:

| Platform | Installer | Build script |
|----------|-----------|--------------|
| Windows ARM64 | `.msi` | `scripts/windows/build_recover_msi_arm64.sh --build` |
| Linux ARM64   | `.deb` | `scripts/linux/build_recover_deb_arm64.sh --build` |
| Linux ARM64   | `.rpm` | `scripts/linux/build_recover_rpm.sh --arch arm64` |
| macOS (universal) | `.pkg` | `scripts/macos/build_recover_pkg_universal.sh` |

The macOS universal `.pkg` is built natively on a Mac (it `lipo`s the arm64 and
x86-64 slices into one binary); it is listed here for completeness but is not
part of the x86-64-host cross-build this document covers.

On a native ARM64 host (or an ARM64 CI runner) the Linux build needs only the
normal `build_linux_x86_64.sh` dependency set against the native arch - no
cross toolchain, no multiarch.  This document covers the harder case:
**cross-building the ARM64 installers from an x86-64 Ubuntu 24.04 (noble)
host**, which has two blockers that a stock host does not satisfy.

A single script provisions everything below:

```
scripts/dev/setup_arm64_buildhost.sh
```

It is idempotent and safe to re-run.  The rest of this document explains what
it does and why, so the setup can be reproduced or audited by hand.

## Blocker 1 - Linux ARM64 GUI dev libraries

The Tauri GUI links against WebKitGTK / GTK, so the ARM64 `-dev` libraries must
be present on the build host.  Stock noble cannot install them because:

- ARM64 packages are served from `ports.ubuntu.com`, not
  `archive.ubuntu.com`.  Enabling `arm64` multiarch and pointing an apt source
  at the ports archive is required; the stock sources must be pinned to
  `amd64` so apt does not look for ARM64 on `archive.ubuntu.com` (404s).
- Several gobject-introspection `.gir` files (e.g.
  `/usr/share/gir-1.0/Pango-1.0.gir`) ship in an arch-independent path but have
  arch-specific contents, so the `amd64` and `arm64` `-dev` packages collide.
  `dpkg --force-overwrite` resolves the clash (benign for cross-compilation).
- ARM64 maintainer scripts (e.g. gdk-pixbuf's loader cache) try to execute
  ARM64 binaries on the x86-64 host.  `qemu-user-static` + `binfmt-support`
  let them run.

The cross compiler is `gcc-aarch64-linux-gnu`; `build_linux_arm64.sh` points
`pkg-config` at `/usr/lib/aarch64-linux-gnu/pkgconfig` and sets the
`aarch64-unknown-linux-gnu` cargo/cc cross variables.  OpenSSL is **vendored**
(built from source): `bkp-storage` enables `ssh2`'s `vendored-openssl` and
`openssl`'s `vendored` features on Unix, so `openssl-sys` compiles OpenSSL for
the whole graph - libssh2's SFTP crypto and reqwest's native-tls alike - and
the binary links no system OpenSSL.  libssh2 itself is bundled and built from
source by `libssh2-sys` (no system ARM64 libssh2 needed).  The setup script
still installs `libssl-dev:arm64`, but with OpenSSL vendored that is
belt-and-suspenders, not a hard requirement.

## Blocker 2 - wixl cannot target ARM64

The Windows `.msi` is built with `wixl` from msitools.  **As of msitools 0.106
(the latest release), wixl cannot build an ARM64 MSI**: its `Arch` enum is only
`x86` / `ia64` / `x64`, and it rejects `--arch arm64` with:

```
arch of type 'arm64' is not supported
```

`build_recover_msi_arm64.sh` probes for this and fails early with a clear
message rather than a cryptic mid-build error.

### The patch

`scripts/dev/msitools-0.106-wixl-arm64.patch` adds ARM64 support to wixl.  It
is small and self-contained:

- **`Arch` enum** (`tools/wixl/builder.vala`): add an `ARM64` member.
  `wixl --arch arm64` then parses, because `enum_from_string<Arch>` derives the
  accepted token from the lowercased GEnum member nick (the same mechanism that
  maps `"x64"` to `X64`).
- **MSI summary Template** (`tools/wixl/msi.vala`, `get_arch_template`): map
  `ARM64` to the platform string `"Arm64"`.  Windows Installer reads this from
  the `_SummaryInformation` Template field; the emitted value is `Arm64;1033`.
- **64-bit defaults** (`tools/wixl/builder.vala`): include `ARM64` alongside
  `X64`/`IA64` so components and registry searches default to the 64-bit
  attribute (`ComponentAttribute.64BIT` / `RegistryType.64BIT`) when the `.wxs`
  does not set `Win64` explicitly.

Build and install (what `setup_arm64_buildhost.sh` automates):

```
curl -fsSL https://download.gnome.org/sources/msitools/0.106/msitools-0.106.tar.xz | tar xJ
cd msitools-0.106
patch -p1 < /path/to/scripts/dev/msitools-0.106-wixl-arm64.patch
meson setup _build --prefix=/usr/local --buildtype=release
meson compile -C _build
DESTDIR=/tmp/msi-stage meson install -C _build   # stage as the build user
sudo cp -a /tmp/msi-stage/usr/local/. /usr/local/  # /usr/local/bin/wixl shadows the distro wixl
sudo ldconfig
```

Build deps: `meson ninja-build valac bison flex gettext pkg-config
libgcab-dev libgsf-1-dev libglib2.0-dev uuid-dev libbz2-dev
libgirepository1.0-dev gobject-introspection`.

### Verifying

```
wixl --version            # 0.106
wixl --arch arm64 ...      # no "not supported" error
msiinfo suminfo out.msi | grep Template   # Template: Arm64;1033
```

> Note: the `Platform='arm64'` attribute on `<Package>` in the `.wxs` is not a
> recognized wixl property and emits a harmless `GLib-GObject-CRITICAL`
> warning; the MSI's target architecture comes from the `--arch arm64` CLI flag
> (this mirrors how the x86-64 `.wxs` relies on `--arch x64`).  The warning is
> non-fatal and the resulting MSI is correct.

## Upstreaming

The patch is a candidate to send upstream to
[GNOME/msitools](https://gitlab.gnome.org/GNOME/msitools).  If a future
msitools release adds ARM64 support, drop the patch and require that version
instead - the build script's probe already keys off the runtime capability,
not the version number.
