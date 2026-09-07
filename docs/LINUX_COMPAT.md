# Linux distro compatibility

Which Linux releases the shipped `.deb` / `.rpm` install and run on, and why.
Current as of 0.9.11.

## What the binary needs

| Requirement | Source | Floor |
|---|---|---|
| glibc | native build, host-stamped symbol versions | `GLIBC_2.34` |
| WebKitGTK 4.1 | Tauri 2 / wry (`libwebkit2gtk-4.1.so.0`) | Ubuntu 22.04, Debian 12 |
| libsoup3 | pulled in by WebKitGTK 4.1 (`libsoup-3.0.so.0`) | same |
| GTK 3 | `libgtk-3.so.0`, `libgdk-3.so.0` | any current desktop |
| libsmbclient | SMB backend, in-process SMB2/3 (`libsmbclient.so.0`) | any current distro |

Full `DT_NEEDED` list: `libwebkit2gtk-4.1.so.0`, `libjavascriptcoregtk-4.1.so.0`,
`libgtk-3.so.0`, `libgdk-3.so.0`, `libsoup-3.0.so.0`, `libgio-2.0.so.0`,
`libgobject-2.0.so.0`, `libglib-2.0.so.0`, `libgdk_pixbuf-2.0.so.0`,
`libcairo.so.2`, `libdbus-1.so.3`, `libsmbclient.so.0`, `libz.so.1`,
`libgcc_s.so.1`, `libm.so.6`, `libc.so.6`.

`libsmbclient` is what lets the SMB backend read a share with no OS mount and
no root - see "SMB without a mount" below.  Its Debian package was renamed
between the supported releases (`libsmbclient` on Ubuntu 22.04 / Debian 12,
`libsmbclient0` from Ubuntu 24.04), so the deb declares
`libsmbclient0 | libsmbclient`.

Notably absent: appindicator (the recovery app registers no tray icon) and
librsvg (dlopen'd by gdk-pixbuf for SVG icons, never linked).  Neither is a
hard dependency in either package.

WebKitGTK 4.1 - not glibc - is the real gate.  It is the reason nothing older
than Ubuntu 22.04 / Debian 12 works, and the reason the entire RHEL 9 family
does not work at any glibc version.

## How the floor is enforced

- **Build host**: release builds run on Ubuntu 22.04 (glibc 2.35).  A native
  build stamps in the BUILD host's glibc symbol versions, so building on 24.04
  produces a binary needing `GLIBC_2.39` that dies on every older distro.
- **ABI gate**: `scripts/linux/build_recover_deb_x86_64.sh` objdumps the staged
  binary and refuses to package anything above `MAX_GLIBC` (default 2.34).
- **deb**: `Depends: libc6 (>= <measured>)` plus the sonames' Debian package
  names, with `a | b` alternatives covering the 64-bit time_t rename
  (`libgtk-3-0` -> `libgtk-3-0t64` etc.).
- **rpm**: `Requires` are declared as SONAMEs, not package names - rpm
  auto-generates a Provides for every soname a package ships, so
  `libwebkit2gtk-4.1.so.0()(64bit)` resolves on Fedora, openSUSE and Mageia
  alike, which no single package-name set does.

The tables below are x86-64; see "ARM64 (aarch64)" for the arm64 matrix.

## Debian / Ubuntu family (.deb)

| Distro (version) | First released | Base | glibc | webkit2gtk-4.1 | Installs |
|---|---|---|---|---|---|
| Ubuntu 20.04 LTS | 2020-04-23 | - | 2.31 | 4.0 only | no |
| Debian 11 bullseye | 2021-08-14 | - | 2.31 | 4.0 only | no |
| Ubuntu 22.04 LTS | 2022-04-21 | - | 2.35 | yes | yes |
| Pop!_OS 22.04 | 2022-04-25 | Ubuntu 22.04 | 2.35 | yes | yes |
| Linux Mint 21 | 2022-07-31 | Ubuntu 22.04 | 2.35 | yes | yes |
| elementary OS 7 | 2023-01-31 | Ubuntu 22.04 | 2.35 | yes | yes |
| Debian 12 bookworm | 2023-06-10 | - | 2.36 | yes | yes |
| MX Linux 23 | 2023-07-31 | Debian 12 | 2.36 | yes | yes |
| Devuan 5 daedalus | 2023-08-14 | Debian 12 | 2.36 | yes | yes |
| LMDE 6 faye | 2023-09-24 | Debian 12 | 2.36 | yes | yes |
| Raspberry Pi OS (bookworm) | 2023-10-10 | Debian 12 | 2.36 | yes | arm64 deb |
| Zorin OS 17 | 2023-12-19 | Ubuntu 22.04 | 2.35 | yes | yes |
| Ubuntu 24.04 LTS | 2024-04-25 | - | 2.39 | yes | yes |
| Linux Mint 22 | 2024-07-25 | Ubuntu 24.04 | 2.39 | yes | yes |
| Ubuntu 24.10 | 2024-10-10 | - | 2.40 | yes | yes |
| elementary OS 8 | 2024-11-07 | Ubuntu 24.04 | 2.39 | yes | yes |
| Ubuntu 25.04 | 2025-04-17 | - | 2.41 | yes | yes |
| Debian 13 trixie | 2025-08-09 | - | 2.41 | yes | yes |
| Ubuntu 25.10 | ~2025-10 | - | 2.42 | yes | yes |
| Ubuntu 26.04 LTS | ~2026-04 | - | 2.42+ | yes | yes |
| KDE neon | rolling | Ubuntu LTS | 2.35+ | yes | yes |
| Kali / Parrot | rolling | Debian testing | 2.36+ | yes | yes |

## RPM family (.rpm)

| Distro (version) | First released | glibc | webkit2gtk-4.1 | Installs |
|---|---|---|---|---|
| RHEL 8 / Alma 8 / Rocky 8 | 2019-05-07 | 2.28 | 4.0 only | no |
| Fedora 35 | 2021-11-02 | 2.34 | 4.0 only | no |
| CentOS Stream 9 | 2021-12-03 | 2.34 | none (4.0 only) | no |
| RHEL 9 | 2022-05-17 | 2.34 | none (4.0 only) | no |
| AlmaLinux 9 | 2022-05-26 | 2.34 | none (4.0 only) | no (verified) |
| Rocky Linux 9 | 2022-07-14 | 2.34 | none (4.0 only) | no |
| openSUSE Leap 15.5 / 15.6 | 2023-06-07 / 2024-06-12 | 2.31 | 4.0 only | no |
| Fedora 36 | 2022-05-10 | 2.35 | yes (first 4.1) | yes |
| Mageia 9 | 2023-08-26 | 2.36 | yes | yes |
| Fedora 40 | 2024-04-23 | 2.39 | yes | yes |
| Fedora 41 | 2024-10-29 | 2.40 | yes | yes |
| Fedora 42 | 2025-04-15 | 2.41 | yes | yes |
| RHEL 10 / Alma 10 / Rocky 10 | 2025-05-20 | 2.39 | UNVERIFIED | unverified |
| openSUSE Leap 16.0 | ~2025-10 | 2.38+ | yes | yes |
| Fedora 43 / 44 | ~2025-11 / ~2026-04 | 2.42+ | yes | yes |
| openSUSE Tumbleweed | rolling | current | yes | yes |

Dates marked `~` are approximate and should be confirmed before publishing
this table externally.

### The RHEL 9 family does not work

Verified on AlmaLinux 9 (glibc 2.34) with BaseOS, AppStream, Extras, CRB and
Plus all enabled:

```
Error:
 - nothing provides libjavascriptcoregtk-4.1.so.0()(64bit)
 - nothing provides libsoup-3.0.so.0()(64bit)
 - nothing provides libwebkit2gtk-4.1.so.0()(64bit)
```

RHEL 9 ships `webkit2gtk3` (2.52.5), which provides `libwebkit2gtk-4.0.so.37`,
and `libsoup` 2.72, which provides `libsoup-2.4.so.1`.  There is no
webkit2gtk4.1 and no libsoup3 package in any RHEL 9 repository.  glibc is not
the blocker - RHEL 9 is at 2.34 and would otherwise qualify.

This is why the RPM declares soname Requires: without them the package installs
cleanly on RHEL 9 and then fails at launch with a missing-`.so` error.  With
them, `dnf` refuses up front and names what is missing.

## ARM64 (aarch64)

Same two requirements - glibc plus a WebKitGTK 4.1 / libsoup3 stack - but the
glibc ceiling is **2.35**, not 2.34: the 2.34 value exists on x86_64 only to
protect the RHEL 9 family, which cannot run this app on any architecture.  The
practical arm64 floor is Ubuntu 22.04 arm64 / Raspberry Pi OS bookworm.

| Distro (version) | Pkg | First released | glibc | webkit2gtk-4.1 | Installs | Typical hardware |
|---|---|---|---|---|---|---|
| Raspberry Pi OS 64-bit (bullseye) | deb | 2021-11-08 | 2.31 | 4.0 only | no | Pi 3/4 |
| Ubuntu 20.04 LTS arm64 | deb | 2020-04-23 | 2.31 | 4.0 only | no | servers, SBCs |
| Debian 11 arm64 | deb | 2021-08-14 | 2.31 | 4.0 only | no | servers, SBCs |
| RHEL 9 / Alma 9 / Rocky 9 aarch64 | rpm | 2022-05-17 | 2.34 | none (4.0 only) | no | Graviton, Ampere |
| Ubuntu 22.04 LTS arm64 | deb | 2022-04-21 | 2.35 | yes | yes | Graviton, Ampere, Pi |
| Debian 12 arm64 | deb | 2023-06-10 | 2.36 | yes | yes | servers, SBCs |
| Raspberry Pi OS 64-bit (bookworm) | deb | 2023-10-10 | 2.36 | yes | yes | Pi 4 / Pi 5 |
| Armbian (bookworm / jammy base) | deb | rolling | 2.35+ | yes | yes | SBCs |
| Ubuntu 24.04 LTS arm64 | deb | 2024-04-25 | 2.39 | yes | yes | Graviton, Ampere, Pi |
| Debian 13 arm64 | deb | 2025-08-09 | 2.41 | yes | yes | servers, SBCs |
| Ubuntu 26.04 LTS arm64 | deb | ~2026-04 | 2.42+ | yes | yes | Graviton, Ampere |
| Fedora 36+ aarch64 | rpm | 2022-05-10 | 2.35+ | yes | yes | Ampere, SBCs |
| Fedora Asahi Remix | rpm | 2023-08-22 | 2.37+ | yes | yes | Apple Silicon Macs |
| openSUSE Leap 16.0 aarch64 | rpm | ~2025-10 | 2.38+ | yes | yes | Ampere |
| openSUSE Tumbleweed aarch64 | rpm | rolling | current | yes | yes | Ampere, SBCs |

Two arm64-only notes:

- **WSL2 on Windows on ARM** runs the arm64 deb natively (Ubuntu 22.04/24.04
  arm64 images), with the GUI via WSLg - a useful test target on a Snapdragon
  laptop or an Apple Silicon Mac running Windows in a VM.
- **Apple Silicon** reaches this app either through Fedora Asahi Remix on bare
  metal, or through an arm64 Linux VM (UTM, Parallels, VMware Fusion), where
  Ubuntu 22.04+ arm64 behaves like any other arm64 host.

## SMB without a mount

On Linux the SMB backend opens an SMB2/3 session in-process through
libsmbclient, using the username and password from the endpoint.  Nothing has
to be mounted first, and nothing needs root.

That is deliberate, and the alternative does not work for a recovery tool: a
kernel CIFS mount needs `CAP_SYS_ADMIN`, and while `mount.cifs` is installed
setuid root on most distros it refuses unprivileged callers unless the exact
mount point is already listed in `/etc/fstab`:

```
mount.cifs: permission denied: no match for /mnt/smb found in /etc/fstab
```

So an app running as an ordinary user - the live-USB case this tool exists for
- cannot mount the share on the user's behalf, and must not have to.

`mount_path` is still honoured and takes precedence whenever it points at a
directory that exists, so pre-existing mounts and older configs keep working
(and skip a redundant second session).

Two consequences worth knowing:

- **macOS has no packaged libsmbclient**, so it still requires `mount_path`
  (`mount_smbfs //user@host/share /Volumes/share`).  The "share is not
  mounted" error, which carries a ready-to-run mount command, only fires
  there.
- **SMB calls are serialized.**  libsmbclient keeps a process-wide context
  that is not thread-safe (pavao's `SMBCTX` is a global singleton), so a mutex
  guards every call.  Restores therefore fetch packs sequentially over SMB
  rather than concurrently.

Build hosts need `libsmbclient-dev` (and `libsmbclient-dev:arm64` for the
arm64 cross-build; `scripts/dev/switch_gui_dev_arch.sh` installs it with the
rest of the per-arch dev set).

## Verifying

```bash
# Highest glibc symbol the binary needs
objdump -T staging/linux/x86_64/nyx_bkp_recover \
    | grep -oE 'GLIBC_[0-9]+\.[0-9]+' | sort -u -V | tail -1

# deb: metadata + dependency resolution (no install)
dpkg-deb -f dist/NyxBackup-Recovery-*-amd64.deb Depends Recommends
sudo apt-get install --simulate ./dist/NyxBackup-Recovery-*-amd64.deb

# SMB is linked in-process, not mounted - the binary must need libsmbclient
objdump -p staging/linux/x86_64/nyx_bkp_recover | grep 'NEEDED.*smbclient'

# rpm: declared requires + resolution on an RPM box
rpm -qpR dist/NyxBackup-Recovery-*-x86_64.rpm
dnf install --assumeno ./NyxBackup-Recovery-*-x86_64.rpm
```

## Tested for 0.9.11

- Built on Ubuntu 22.04 (glibc 2.35); binary requires at most `GLIBC_2.34`.
- deb dependency resolution simulated clean on Ubuntu 22.04 and Ubuntu 24.04
  (the latter exercises the t64 alternatives).
- rpm dependency resolution on AlmaLinux 9 - correctly refused, see above.
- SMB backend exercised end to end on Ubuntu 24.04 (WSL2, GUI via WSLg)
  against a real Samba host with no mount present: session opened from the
  endpoint's credentials, manifests and snapshot index listed, snapshot
  decoded.  This is the only runtime/GUI path smoke-tested so far.
- Restore of file content over the in-process SMB session is NOT yet tested;
  only connect / list / decode are.

## Open items

- **arm64**: no arm64 deb built for 0.9.11.  It needs the same treatment - an
  Ubuntu 22.04 arm64 host or cross-toolchain - or it will carry the build
  host's newer glibc.
- **RHEL 10 / Alma 10 / Rocky 10**: listed as unverified.  The RHEL 9 result
  shows the family's webkit packaging cannot be assumed; check with
  `dnf install --assumeno` on an Alma 10 image before claiming support.
- **`MAX_GLIBC=2.34`**: originally chosen to protect the RHEL 9 family, which
  turns out not to be supportable at all.  Every remaining RPM target is at
  glibc 2.35+.  The ceiling is kept because it is already met at no cost, but
  the rationale comment in `build_recover_deb_x86_64.sh` overstates it.
