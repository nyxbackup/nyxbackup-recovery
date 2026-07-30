# Nyx Backup signing keys (Recovery Tool)

The Recovery Tool `.deb` / `.rpm` packages are signed with the **same**
production GPG key as the main Nyx Backup application - one release key for
every Nyx Software, LLC Linux package.

## `nyx-backup-production.pub.asc`

- Type: RSA 4096, sign-only.
- UID: `Nyx Backup <releases@nyxbackup.com>`.
- Fingerprint: `68473215 FAFF4871 48E2443B 5A2C55C8 614A89A3`.
- Long id (use as `NYX_SIGN_KEY_ID` for release builds): `5A2C55C8614A89A3`.
- Expires: 2040-01-01.
- Published for users at `https://nyxbackup.com/keys/release.gpg`
  (ASCII-armored: `/keys/release.asc`).

The secret half lives only on the release build machine's GPG keyring and an
offline encrypted backup. It is intentionally passphrase-less on the build box
so CI signing is unattended; the offline backup is passphrase-encrypted.

## Signing a release

```bash
export NYX_SIGN_KEY_ID=5A2C55C8614A89A3
./scripts/linux/build_recover_deb_x86_64.sh      # debsigs
./scripts/linux/build_recover_rpm.sh             # rpmsign (same key)
```

Unset `NYX_SIGN_KEY_ID` -> unsigned dev packages. The `build_recover_deb_*` /
`build_recover_rpm.sh` scripts call `sign_deb.sh` / `sign_rpm.sh` after building
when it is set.

## Users verify

```bash
# import once, confirm the fingerprint matches
curl -fsSL https://nyxbackup.com/keys/release.gpg | gpg --import

# .rpm
rpm --checksig NyxBackup-Recovery-<version>-x86_64.rpm      # "digests signatures OK"

# .deb (the _gpgorigin member signs the other members concatenated)
tmp=$(mktemp -d); ( cd "$tmp" && ar x /path/NyxBackup-Recovery-<version>-amd64.deb \
  && cat debian-binary control.tar.* data.tar.* > combined \
  && gpg --verify _gpgorigin combined ); rm -rf "$tmp"
```
