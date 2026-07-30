# Installing Nyx Backup Recovery

Pre-built installers are published per platform and architecture:

| Platform | x86-64 | ARM64 | File |
|----------|--------|-------|------|
| Windows  | yes    | yes   | `NyxBackup-Recovery-<ver>-{x86_64,arm64}.msi` |
| Linux    | yes    | yes   | `NyxBackup-Recovery-<ver>-{amd64,arm64}.deb` |
| Linux    | yes    | yes   | `NyxBackup-Recovery-<ver>-{x86_64,aarch64}.rpm` |
| macOS    | yes    | yes   | `NyxBackup-Recovery-<ver>-universal.pkg` (Intel + Apple Silicon) |

Pick the file matching your OS and CPU.  On Windows/Linux, `x86_64`/`amd64`
is Intel/AMD; `arm64`/`aarch64` is ARM (Windows on ARM, ARM servers/SBCs,
Apple Silicon under translation, etc.).

## 1. Verify the download first

This is a recovery tool, often downloaded onto an unfamiliar machine - verify
the checksum before running it.  Each release ships a `SHA256SUMS-<ver>.txt`.

- Linux: `sha256sum -c SHA256SUMS-<ver>.txt`
- macOS: `shasum -a 256 -c SHA256SUMS-<ver>.txt`
- Windows (PowerShell):
  `(Get-FileHash .\NyxBackup-Recovery-<ver>-x86_64.msi -Algorithm SHA256).Hash`
  and compare against the matching line in `SHA256SUMS-<ver>.txt`.

Run the command from the directory holding both the installer(s) and the sums
file.

## 2. Install

### Linux - .deb (Debian, Ubuntu, Mint, ...)

Use `apt`, not `dpkg -i` - `apt` resolves the GUI runtime dependencies
(WebKitGTK 4.1, GTK 3, librsvg, libayatana-appindicator); `dpkg -i` does not
and leaves the package unconfigured.

```bash
sudo apt install ./NyxBackup-Recovery-<ver>-amd64.deb     # or -arm64.deb
```

If you already ran `dpkg -i` and hit a dependency error, finish it with
`sudo apt-get -f install`.

### Linux - .rpm (Fedora, RHEL, openSUSE, ...)

```bash
sudo dnf install ./NyxBackup-Recovery-<ver>-x86_64.rpm     # or -aarch64.rpm
# openSUSE: sudo zypper install ./...rpm
```

The RPM declares no hard dependencies (so it installs on any RPM distro), but
the app needs a desktop WebKitGTK 4.1 / GTK 3 stack present at runtime -
already there on a normal desktop install.

### Windows - .msi

Double-click the `.msi`, or from an elevated prompt:

```bat
msiexec /i NyxBackup-Recovery-<ver>-x86_64.msi
```

Installs per-machine to `%ProgramFiles%\Nyx Backup Recovery\` with a Start
Menu shortcut.  Requires the Microsoft Edge **WebView2 Runtime** (present on
Windows 11 and most updated Windows 10; otherwise install the Evergreen
Runtime from Microsoft).

### macOS - .pkg (Intel + Apple Silicon)

The `.pkg` is a universal build - one file installs on both Intel and Apple
Silicon Macs.  Open it and follow the installer, or:

```bash
sudo installer -pkg NyxBackup-Recovery-<ver>-universal.pkg -target /
```

## 3. Run

- Linux: launch "Nyx Backup Recovery" from the app menu, or run
  `nyx_bkp_recover`.
- Windows: use the Start Menu shortcut.
- macOS: open "Nyx Backup Recovery" from Applications.

### Cold-storage (Glacier / Archive) backups

If your backup is stored in a cold/archive tier - S3 Glacier / Glacier
Deep Archive, Azure Archive, or GCS Archive - its objects must be
retrieved (thawed) before they can be downloaded.  This tool does not
thaw them for you.  Start the retrieval with your storage provider (its
console or CLI) or the main Nyx Backup app, wait for it to finish
(minutes to many hours depending on the tier; Deep Archive can take up
to ~48 h, and a retrieval fee usually applies), then run the restore.
Starting a restore before the data is thawed reports that the backup is
in cold storage rather than failing with an opaque error.

### Optional: HiDPI / display scaling (Linux/GTK)

On a HiDPI display the UI can be scaled with the GTK `GDK_DPI_SCALE`
environment variable (fractional values are fine):

```bash
GDK_DPI_SCALE=1.2 nyx_bkp_recover     # 20% larger; try 1.4, 1.5, etc.
```

### Running under WSL (WSLg)

WSLg's emulated GPU can make WebKitGTK render a blank window, with warnings
like `MESA: error: ZINK: failed to choose pdev` / `libEGL ... failed`.  The app
detects WSL and applies the fix automatically - it sets
`WEBKIT_DISABLE_DMABUF_RENDERER=1` and `LIBGL_ALWAYS_SOFTWARE=1` before the
webview starts - so a normal launch just works.

If the window is still blank, force the fuller software path yourself:

```bash
WEBKIT_DISABLE_DMABUF_RENDERER=1 WEBKIT_DISABLE_COMPOSITING_MODE=1 \
  LIBGL_ALWAYS_SOFTWARE=1 nyx_bkp_recover
```

This is a WSL quirk only; on real hardware WebKitGTK uses the GPU normally.

## 4. Uninstall

| Platform | Command |
|----------|---------|
| Linux .deb | `sudo apt remove nyxbackup-recovery` (or `sudo dpkg -r nyxbackup-recovery`) |
| Linux .rpm | `sudo dnf remove nyxbackup-recovery` (or `sudo rpm -e nyxbackup-recovery`) |
| Windows    | Settings > Apps (Add/Remove Programs), or `msiexec /x NyxBackup-Recovery-<ver>-x86_64.msi` |
| macOS      | Delete "Nyx Backup Recovery" from Applications |

The tool keeps no service, no autostart, and no OS-keyring entries; restore
checkpoints (if any) live as plain files under your per-user data directory
and can be deleted by hand.
