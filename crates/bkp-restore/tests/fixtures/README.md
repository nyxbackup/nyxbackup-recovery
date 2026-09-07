# Format-conformance fixture

`format-v2/` is a complete, miniature Nyx Backup **produced by the Nyx Backup
application's real writer**, committed here as opaque bytes and decoded by
`tests/format_conformance.rs`.

```
format-v2/
  packs/<pack-uuid>.pack                     chunk data + CBOR footer index
  manifests/<set-uuid>/<snapshot-uuid>.manifest   encrypted CBOR manifest
  indexes/<set-uuid>/snapshot-index          encrypted CBOR snapshot index
```

## Why bytes and not a generator

This tool exists to read data written by a different program, and its write
paths were deliberately stripped.  There is therefore nothing here to
round-trip against, and a test that re-encoded the format with this repo's own
code would only prove the code agrees with itself - it would keep passing while
real backups became unreadable.

These bytes were written by the application that writes real backups, so
decoding them is the only honest evidence that recovery still works.

## The master key is public on purpose

```
000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f
```

It protects nothing: every byte of content in the fixture is synthetic test
data.  Never regenerate this fixture from real backup data - the master key
would have to be published alongside it.

## What it covers

- key derivation for all subkey labels used by the read path
  (ChunkEncryption, ChunkIdentity, ManifestEncryption, SnapshotIndex)
- envelope + CBOR schema for the manifest and snapshot index
- pack framing: header, chunk offsets, CBOR footer, 8-byte trailer offset
- chunk id derivation (HMAC-SHA256 over plaintext) and its use as AEAD AAD
- zstd decompression
- multi-chunk reassembly with non-zero `plaintext_offset` (`big.txt`)
- a symlink, an empty file, and a nested directory

## Regenerating

Only when the format changes deliberately.  From the **main** Nyx Backup repo:

```bash
cargo run -p bkp-engine --example gen_recovery_fixture -- \
  <path-to>/nyxbackup-recovery/crates/bkp-restore/tests/fixtures/format-v2
```

AES-GCM nonces are random per encryption, so a regenerated fixture differs
byte-for-byte from the old one while meaning the same thing.  A regeneration
that is *not* accompanied by an intentional format change is a red flag: it
hides exactly the drift this fixture exists to catch.

If the test starts failing and nobody changed this repo, the format moved.
Stop and find out how before shipping a release.
