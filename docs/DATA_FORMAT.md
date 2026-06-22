<!--
SPDX-License-Identifier: Apache-2.0
Copyright (c) 2026 Nyx Software, LLC
Source of truth for the Nyx Backup on-disk format.
Published as docs/DATA_FORMAT.md and rendered at https://nyxbackup.com/format.
-->

# Nyx Backup On-Disk Format Specification

**Version:** 2 (matches `bkp_manifest::CURRENT_FORMAT_VERSION`)
**Status:** Stable.  Backwards compatible.  See "Versioning rules" below.

This document specifies the on-disk format of a Nyx Backup archive in
enough detail that an independent implementer can interpret an
archive's bytes and reconstruct the original files from a chosen
snapshot.

**Scope**: this spec describes the **data, formats, algorithms, and
restore process**.  It does NOT describe how to obtain the bytes
from a remote backend (S3, Backblaze B2, Azure Blob, an SFTP server,
a local directory tree, etc.).  Use whatever tool you prefer to
mirror the bucket contents to a local directory; from that point on
this spec is self-contained.

**Inputs assumed**:

1. Read access to every object that exists in the destination's
   namespace (see Section 2 for the namespace).
2. The 32-byte master key, OR the passphrase plus the bootstrap
   record (Section 6).
3. This document.

A reference reader is shipping open-source under Apache-2.0 at
<https://github.com/nyxbackup/nyxbackup-recovery>.  Use it as a
sanity-check oracle when implementing your own reader.  If the
reference reader diverges from this document, the document is the
authoritative source and the reader is the bug.

---

## 1. Conventions

- All multi-byte integers are stated explicitly as "big-endian" or
  "little-endian" at each appearance.  There is no global endianness.
- "UUID bytes" means the 16 raw bytes of a UUID in network byte order
  (RFC 4122, the same byte order `uuid::Uuid::as_bytes` returns).
- **`[u8; N]` is Rust array notation** meaning "a fixed-length
  contiguous array of N unsigned 8-bit bytes."  It is NOT a single
  byte.  For example, `snapshot_id: [u8; 16]` means a 16-byte field
  holding a 128-bit UUID (~3.4x10^38 possible values), not an 8-bit
  integer capped at 256.
- "Unix nanoseconds" means nanoseconds since 1970-01-01 00:00:00 UTC.
- "Unix seconds" means seconds since the same epoch.
- "CBOR" means RFC 8949 (Concise Binary Object Representation) as
  emitted by the `ciborium` crate.  All struct field names below are
  encoded as text-string keys (CBOR major type 3); ordering follows
  the declaration order in this document.
- "HMAC-SHA256" tags are 32 bytes.  HMAC is per RFC 2104.
- "HKDF-SHA256" is per RFC 5869.  We use HKDF with no salt and an
  explicit non-empty `info` field.
- "AES-256-GCM" is per NIST SP 800-38D with a 12-byte nonce and a
  16-byte authentication tag.

## 2. Object layout in the backup destination

A backup destination ("the bucket") contains a flat namespace of
objects.  Every object has a path that starts with one of the
prefixes below.  The reader must be able to list and read these
paths; how they're stored physically (S3 bucket, B2 bucket, local
directory tree, etc.) is out of scope.

```
machines/<machine_id>/bootstrap                     # plaintext CBOR
machines/<machine_id>/bootstrap.bak                 # plaintext CBOR (mirror)

indexes/<backup_set_id>/snapshot-index              # encrypted envelope
indexes/<backup_set_id>/snapshot-index.bak          # encrypted envelope (mirror)

manifests/<backup_set_id>/<snapshot_id>.manifest    # encrypted envelope
manifests/<backup_set_id>/<snapshot_id>.manifest.bak # encrypted envelope (mirror)

packs/<pack_id>.pack                                # binary pack format
```

Where:

- `<machine_id>`, `<backup_set_id>`, `<snapshot_id>`, `<pack_id>` are
  lowercase UUIDs in the canonical 8-4-4-4-12 hyphenated form.

### 2.1 `.bak` mirrors

Every critical small object (everything except `packs/*.pack`) is
written to both the primary path and a `<path>.bak` mirror with
byte-for-byte identical content.  A reader SHOULD try the primary
path first; on any failure (object missing, decryption fails, CBOR
parse fails) it MAY fall back to the `.bak` mirror.  Pack files
themselves are not mirrored.

### 2.2 Sets vs snapshots

A backup destination may contain backup sets from multiple machines
(though this is unusual).  A single machine typically owns the
destination and writes one or more backup sets.  Each backup set
collects many snapshots over time.

To enumerate all available snapshots for restore:

1. List `machines/` to find the `<machine_id>`s in the destination.
2. For each machine, read its `bootstrap` record (plaintext, no key
   needed) to get the list of `backup_set_id`s the machine owns.
3. For each backup set, read its `snapshot-index` (encrypted) to get
   the list of `snapshot_id`s and the metadata for each.
4. For each snapshot, read its `manifest` (encrypted) to get the
   file tree and chunk references.

## 3. Master key and key derivation

### 3.1 Master key

The master key is **32 raw bytes** (256 bits).  It is the
root-of-trust for the entire archive.  Loss = total data loss; theft
= total data exposure.

In Nyx Backup's first-run UI the master key is generated by a
cryptographically secure random byte generator and shown to the user
as 64 lowercase hexadecimal characters for transcription.  An
independent reader receives the master key as input from the user
the same way - typically the 64-character hex string pasted into a
form, which the reader decodes to the 32 raw bytes.

A second way to obtain the master key is to re-derive it from the
user's passphrase via the parameters stored in the bootstrap
record; see Section 6.

### 3.2 Subkey derivation (HKDF-SHA256)

The master key is never used directly to encrypt or hash anything.
Instead, **purpose-bound subkeys** are derived via HKDF-SHA256, with
the master key as input keying material (IKM) and a structured
`info` field that includes both the purpose label and the backup set
ID:

```
info = label_bytes || 0x00 || backup_set_uuid_bytes_16
```

- `label_bytes`: the ASCII label string for the purpose (table below).
- `0x00`: a single zero byte as a separator.
- `backup_set_uuid_bytes_16`: the 16 raw UUID bytes of the backup set.

HKDF is called with `salt = None` (an empty salt) and output length
`32 bytes`.  The resulting 32-byte OKM is the subkey.

Subkey table:

| Purpose | Label string | Used for |
|---|---|---|
| Chunk encryption | `chunk-encryption-v1` | AES-256-GCM key encrypting chunk plaintext bytes inside pack files |
| Manifest encryption | `manifest-encryption-v1` | AES-256-GCM key encrypting `manifest` and `machine.record` envelopes |
| Manifest HMAC | `manifest-hmac-v1` | HMAC-SHA256 key for the `manifest_hmac` field in each snapshot index entry |
| Pack index encryption | `pack-index-encryption-v1` | Reserved; not used in version 2 (pack indices are plaintext CBOR inside pack files - see Section 7) |
| Snapshot index | `snapshot-index-v1` | AES-256-GCM key encrypting the `snapshot-index` envelope |
| Chunk identity | `chunk-identity-v1` | HMAC-SHA256 key for computing chunk IDs (content addressing) |

Two calls to HKDF with identical inputs produce identical subkeys.
Two calls with any input differing - different label, different
backup set ID - produce subkeys that are independent.  A reader MUST
re-derive the appropriate subkey from scratch when decrypting any
object; no subkeys are stored anywhere.

## 4. The encryption envelope (BKEV)

Manifests, snapshot indexes, and machine records are wrapped in a
common **envelope format** (magic `BKEV`).  The envelope provides
AES-256-GCM authenticated encryption with associated data (AAD).

### 4.1 Layout

```
+- Header (40 bytes - used verbatim as AAD) -------------------+
|  Offset  Width  Field                                        |
|       0      4  magic: ASCII "BKEV" (0x42 0x4B 0x45 0x56)    |
|       4      2  format_version: u16 big-endian = 2           |
|       6      1  key_label_id: u8                             |
|       7      1  cipher_id: u8 = 0 (AES-256-GCM only)         |
|       8     24  nonce field (first 12 bytes fed to AES-GCM)  |
|      32      4  plaintext_length: u32 big-endian             |
|      36      4  ciphertext_length: u32 big-endian            |
+- Body -------------------------------------------------------+
|      40      *  ciphertext: AES-256-GCM output               |
|       *     16  AES-GCM authentication tag                   |
+--------------------------------------------------------------+
```

### 4.2 Decoding procedure

To decode an envelope given the appropriate `SubKey`:

1. Verify the envelope is at least 56 bytes (40 header + 16 tag).
2. Verify bytes `0..4` equal ASCII `"BKEV"`.
3. Read `format_version` from bytes `4..6` (big-endian u16).  If it
   is not `2`, abort with a clear error (a newer value implies the
   reader needs to upgrade).
4. Read `key_label_id` from byte `6`.  This identifies which subkey
   should decrypt this envelope:

   | id | label | subkey to use |
   |---|---|---|
   | 0 | `chunk-encryption-v1` | ChunkEncryption (but chunks are not in BKEV envelopes; see Section 7) |
   | 1 | `manifest-encryption-v1` | ManifestEncryption |
   | 2 | `manifest-hmac-v1` | not encrypted; HMAC only |
   | 3 | `pack-index-encryption-v1` | reserved; not used in v2 |
   | 4 | `snapshot-index-v1` | SnapshotIndex |
   | 5 | `chunk-identity-v1` | not encrypted; HMAC only |

5. Read `cipher_id` from byte `7`.  If not `0`, abort.
6. Read `nonce24` from bytes `8..32`.  Take bytes `0..12` as the
   12-byte AES-GCM nonce; bytes `12..24` are reserved and unused.
7. Read `plaintext_length` from bytes `32..36` (big-endian u32).
8. Verify `data.len() - 40 == plaintext_length + 16` (envelope body
   is exactly ciphertext plus 16-byte tag).
9. AES-256-GCM decrypt:
   - **Key**: the SubKey identified by `key_label_id`.
   - **Nonce**: the 12 bytes from step 6.
   - **AAD**: the entire 40-byte header from step 1.
   - **Ciphertext + tag**: bytes from offset 40 to end of envelope.
   - The 16-byte tag is the last 16 bytes of the ciphertext-and-tag
     concatenation (standard AES-GCM tag convention).
10. If the GCM tag verification fails, abort.  The envelope is
    either corrupt or was not encrypted with this key.
11. The decryption output is the plaintext - usually CBOR; what
    that CBOR represents depends on `key_label_id`.

Encoding is the inverse, with a fresh 12-byte random nonce in the
`nonce24` field's first 12 bytes (the remaining 12 are zero or
arbitrary; they are reserved for forward compatibility and do not
contribute to security).

## 5. CBOR schemas

CBOR encoding is per RFC 8949.  Field names are text strings; all
nested structures follow the same convention.  The `ciborium` crate
emits these structures in declaration order with no canonicalisation
beyond CBOR's basic rules.

### 5.1 `BootstrapRecord` (plaintext)

Stored at `machines/<machine_id>/bootstrap` and its `.bak` mirror.
The only **plaintext** CBOR object in the format.  Contains no
secrets - the Argon2id parameters (including the salt) are
intentionally non-secret; only the passphrase is.

```
BootstrapRecord {
    format_version: u32,                  // = 1 for the bootstrap record family
    machine_id: [u8; 16],                 // raw UUID bytes
    hostname: text,                       // informational; may be empty
    created_at: u64,                      // Unix seconds
    kdf_params: Argon2Params,             // see below
    backup_set_ids: [[u8; 16], ...],      // list of backup-set UUIDs on this machine
}

Argon2Params {
    algorithm: u8,                        // 0 = Argon2id, 1 = PBKDF2-HMAC-SHA256
    m_cost: u32,                          // memory cost in KiB (Argon2 only; default 131072)
    t_cost: u32,                          // iteration count (Argon2 default 3, PBKDF2 ~1,000,000)
    p_cost: u32,                          // parallelism (Argon2 only; default 4)
    output_len: usize,                    // CBOR encodes as u64; output is always 32 bytes here
    salt: [u8; 32],                       // 32 raw bytes
}
```

### 5.2 `SnapshotIndex` (encrypted with `snapshot-index-v1`)

Stored at `indexes/<backup_set_id>/snapshot-index`.  The encrypted
envelope wraps this CBOR.

```
SnapshotIndex {
    format_version: u32,                  // = 2 (current stable format version)
    backup_set_id: [u8; 16],              // raw UUID bytes
    machine_id: [u8; 16],                 // raw UUID bytes
    entries: [SnapshotEntry, ...],        // oldest first
}

SnapshotEntry {
    snapshot_id: [u8; 16],                // raw UUID bytes
    created_at: u64,                      // Unix nanoseconds
    manifest_path: text,                  // "manifests/<set_id>/<snapshot_id>.manifest"
    manifest_size: u64,                   // byte size of the encrypted manifest object
    manifest_hmac: [u8; 32],              // HMAC-SHA256 (see Section 5.3)
    files_total: u64,
    bytes_total: u64,
    packs_referenced: [[u8; 16], ...],    // UUIDs of every pack this snapshot references
}
```

### 5.3 `manifest_hmac` semantics

The 32-byte tag in `SnapshotEntry::manifest_hmac` is:

```
HMAC-SHA256(
    key = ManifestHmac subkey (label "manifest-hmac-v1", set_id),
    msg = snapshot_id_bytes_16 || manifest_envelope_bytes
)
```

where `manifest_envelope_bytes` is the **complete encrypted manifest
envelope** as read from `manifests/<set_id>/<snapshot_id>.manifest`
(including the 40-byte BKEV header and the 16-byte tag).  A reader
SHOULD verify this HMAC before decrypting the manifest, as cheap
detection of tampered or substituted manifests.  Mismatch -> abort
or try the `.bak` mirror.

### 5.4 `Manifest` (encrypted with `manifest-encryption-v1`)

Stored at `manifests/<backup_set_id>/<snapshot_id>.manifest`.  One
per snapshot.

```
Manifest {
    format_version: u32,                  // = 2
    snapshot_id: [u8; 16],
    backup_set_id: [u8; 16],
    machine_id: [u8; 16],
    created_at_ns: u64,                   // Unix nanoseconds
    hostname: text,
    set_name: text,                       // user-visible label (e.g. "Documents"); may be empty
    files_total: u64,
    dirs_total: u64,
    bytes_total: u64,                     // total plaintext bytes across all files
    chunks_total: u64,
    file_tree: FileTree,                  // see Section 5.5
}
```

### 5.5 File tree (`FileTree`, `TreeNode`, `FileEntry`, `ChunkRef`)

```
FileTree {
    root: TreeNode,                       // single root directory
}

TreeNode {
    node_type: u8,                        // 0 = Directory, 1 = File, 2 = Symlink
    name: text,                           // single path component only, NOT a full path
    children: [TreeNode, ...],            // populated only when node_type == Directory
    file_entry: FileEntry | null,         // Some(.) when node_type == File or Symlink
    dir_mtime_ns: u64,                    // mtime for Directory nodes (0 if not recorded)
}

FileEntry {
    size: u64,                            // file size in plaintext bytes
    mtime_ns: u64,                        // modification time, Unix nanoseconds
    ctime_ns: u64,                        // inode change time, Unix nanoseconds
    mode: u32,                            // POSIX permission bits (0 on Windows)
    owner_uid: u32,                       // 0 on Windows
    owner_gid: u32,                       // 0 on Windows
    windows_attrs: u32,                   // Windows file-attribute flags (0 on non-Windows)
    xattrs: { text -> [u8] },             // CBOR map of extended attribute name -> raw bytes
    symlink_target: text | null,          // Some(.) only when node_type == Symlink
    chunks: [ChunkRef, ...],              // empty for Symlink and Directory
}

ChunkRef {
    chunk_hash: [u8; 32],                 // 32-byte HMAC-SHA256 chunk ID
    plaintext_offset: u64,                // byte offset within the reconstructed file
    plaintext_size: u64,                  // plaintext (uncompressed) length of this chunk
}
```

The full path of a file is built by concatenating the `name`
components from the root down to the file, joined by `/` (forward
slash).  A Windows-source backup will record paths like
`C:\Users\steve\Documents\foo.txt` as a chain of directory names
`C:`, `Users`, `steve`, `Documents` with a file `foo.txt` at the
leaf.  See Section 9.4 for cross-platform restore guidance.

**Encoded node types**: only `Directory`, `File`, and `Symlink`
nodes appear in a manifest.  Unix special files (FIFOs / named
pipes, Unix domain sockets, character devices, block devices) are
explicitly skipped at scan time and never enter the format - they
have no meaningful "file contents" to back up (a FIFO blocks
forever, a socket can't be opened for read, devices return
hardware state).  Hard links are NOT preserved: a file with N
hard links is enumerated under each of its N paths; content
deduplication stores the bytes exactly once but the post-restore
filesystem has N independent inodes.

### 5.6 `MachineRecord` (encrypted with `manifest-encryption-v1`)

Stored at `machines/<machine_id>/machine.record` and its `.bak`
mirror.  Informational only - the bootstrap record carries the
recovery-critical fields (Argon2 params, backup set IDs).  A reader
does not need this object for restore.

```
MachineRecord {
    format_version: u32,                  // = 2
    machine_id: [u8; 16],
    hostname: text,
    os_name: text,                        // "Linux", "macOS", "Windows"
    os_version: text,
    app_version: text,                    // Nyx Backup version that wrote this record
    created_at_ns: u64,
    last_seen_at_ns: u64,
    backup_set_ids: [[u8; 16], ...],
}
```

## 6. Re-deriving the master key from a passphrase

If the user has lost their hex-encoded master key but retained the
**passphrase** they chose at install time, the master key can be
re-derived from the passphrase plus the parameters in
`BootstrapRecord.kdf_params`.

### 6.1 Argon2id (`algorithm == 0`)

```
master_key = Argon2id(
    password    = passphrase_utf8_bytes,
    salt        = kdf_params.salt (32 bytes),
    m_cost_kib  = kdf_params.m_cost,
    t_cost      = kdf_params.t_cost,
    parallelism = kdf_params.p_cost,
    output_len  = 32,
)
```

Reference: RFC 9106.  Any conformant Argon2id implementation
produces the same 32 bytes given identical inputs.

### 6.2 PBKDF2-HMAC-SHA256 (`algorithm == 1`)

```
master_key = PBKDF2(
    PRF        = HMAC-SHA256,
    password   = passphrase_utf8_bytes,
    salt       = kdf_params.salt (32 bytes),
    iterations = kdf_params.t_cost,
    output_len = 32,
)
```

Reference: RFC 8018.  `t_cost` will be at least 1,000,000 for any
modern Nyx Backup install.  `m_cost` and `p_cost` are ignored for
this algorithm.

## 7. The pack format (BKPK)

Pack files at `packs/<pack_id>.pack` carry the actual encrypted
chunk bytes that reconstruct file contents.  Pack files are NOT
wrapped in a BKEV envelope - they have their own format.

### 7.1 Binary layout

```
+- Header (22 bytes) ------------------------------------------+
|  0.. 4  magic: ASCII "BKPK" (0x42 0x4B 0x50 0x4B)            |
|  4.. 6  pack_version: u16 big-endian = 1                     |
|  6..22  pack_id: 16 raw UUID bytes                           |
+- Chunk entries (repeated, variable count) -------------------+
|  0.. 4  encrypted_size: u32 LITTLE-endian                    |
|  4.. N  encrypted_bytes: 12-byte nonce || ciphertext || tag  |
+- Footer index -----------------------------------------------+
|         CBOR-encoded array of PackIndexEntry records         |
+- Trailer (8 bytes) ------------------------------------------+
|  0.. 8  footer_offset: u64 LITTLE-endian                     |
+--------------------------------------------------------------+
```

Note the endianness mix - the header is big-endian (matching
network byte order and the BKEV envelope convention) while the
per-chunk size prefix and trailer are little-endian.  This is
historical and stable.

### 7.2 `PackIndexEntry` CBOR schema

```
PackIndexEntry {
    chunk_id: [u8; 32],                   // HMAC-SHA256 chunk ID (matches Manifest's ChunkRef.chunk_hash)
    offset: u64,                          // byte offset within the pack file of the size-prefixed entry
    size: u64,                            // byte length of encrypted_bytes (NOT including the 4-byte size prefix)
}
```

The footer is a CBOR array (major type 4) of these entries.

### 7.3 Reading the pack index

Given the complete pack bytes:

1. Verify bytes `0..4` equal `"BKPK"`.
2. Verify bytes `4..6` (big-endian u16) equal `1`.
3. Read the last 8 bytes of the file as a little-endian u64; this
   is the `footer_offset`.
4. CBOR-decode the slice `pack_bytes[footer_offset..pack_bytes.len() - 8]`
   as an array of `PackIndexEntry` (Section 7.2).

The result is a list mapping each chunk's 32-byte HMAC chunk ID to
its byte offset and size within this pack.  Individual chunks then
live at `pack_bytes[entry.offset + 4 .. entry.offset + 4 + entry.size]`
- the `+ 4` skips the per-entry size prefix.

### 7.4 Decrypting a chunk

Each chunk in a pack is a 12-byte AES-GCM nonce followed by the
ciphertext-with-appended-tag:

```
encrypted_chunk = nonce (12 bytes) || ciphertext_with_tag
```

Where `ciphertext_with_tag` is the standard AES-GCM output (the last
16 bytes are the authentication tag).

Decrypt as:

```
plaintext_compressed = AES-256-GCM-Decrypt(
    key        = ChunkEncryption subkey (label "chunk-encryption-v1", backup_set_id),
    nonce      = encrypted_chunk[0..12],
    AAD        = chunk_id (32 bytes, from PackIndexEntry.chunk_id or Manifest's ChunkRef.chunk_hash),
    ciphertext = encrypted_chunk[12..],
)
```

The `chunk_id` is bound as AAD - swapping two chunks within a pack
or across packs would force an AAD mismatch on decryption.

### 7.5 Decompressing a chunk

After successful AES-GCM decryption, `plaintext_compressed` is a
**zstd-compressed** blob.  Decompress it to obtain the chunk's raw
plaintext bytes:

```
plaintext = zstd_decompress(plaintext_compressed)
```

zstd is per RFC 8478.  No special framing - the input is a single
zstd frame.  The encoder uses the level configured per backup set
(default level 3); the level does not need to be known by the
decoder because zstd is self-describing.

The decompressed length MUST equal the `ChunkRef.plaintext_size`
field in the manifest entry that referenced this chunk.

### 7.6 Verifying chunk identity (optional but recommended)

To verify that the chunk you just decrypted is the chunk the
manifest claimed it would be:

```
expected_chunk_id = HMAC-SHA256(
    key = ChunkIdentity subkey (label "chunk-identity-v1", backup_set_id),
    msg = plaintext,
)

assert(expected_chunk_id == chunk_id_from_manifest)
```

This is the same per-set-keyed HMAC the encoder used to compute the
chunk ID in the first place.  Match -> integrity verified.  Mismatch
-> stop; the chunk or the manifest has been tampered with.

## 8. Chunking parameters (FastCDC)

Encoder-side detail only.  A reader does not need to re-chunk
anything; the chunk boundaries are recorded directly in the
manifest's `ChunkRef.plaintext_size` and `plaintext_offset` fields.

For completeness: chunks are produced by FastCDC (Wen Xia et al.,
ATC '16) as implemented by the `fastcdc` crate's v2020 variant.
Default parameters: minimum 512 KiB, average 4 MiB, maximum 16 MiB.
A backup set may override these; the values are not persisted in
the archive because they are not needed for restore.

## 9. The complete restore algorithm

Given:

- `dest_objects`: a read-only view of every object in the backup
  destination.
- `master_key`: 32 raw bytes.

To restore one snapshot identified by `(backup_set_id, snapshot_id)`:

### 9.1 Subkey derivation

Derive the five subkeys needed (per Section 3.2):

```
chunk_enc_key   = HKDF(master_key, "chunk-encryption-v1"   || 0x00 || backup_set_id)
chunk_id_key    = HKDF(master_key, "chunk-identity-v1"     || 0x00 || backup_set_id)
manifest_enc_key= HKDF(master_key, "manifest-encryption-v1"|| 0x00 || backup_set_id)
manifest_hmac_key= HKDF(master_key, "manifest-hmac-v1"     || 0x00 || backup_set_id)
snapindex_key   = HKDF(master_key, "snapshot-index-v1"     || 0x00 || backup_set_id)
```

### 9.2 Load the snapshot index

```
index_envelope = dest_objects["indexes/<set_id>/snapshot-index"]
                 (fall back to .bak on failure)
index_plaintext = BKEV_Decode(snapindex_key, index_envelope)
index = CBOR_Decode<SnapshotIndex>(index_plaintext)
assert(index.format_version == 2)
```

### 9.3 Locate and verify the manifest

```
entry = first entry in index.entries where entry.snapshot_id == snapshot_id
manifest_envelope = dest_objects[entry.manifest_path]
                    (fall back to .bak on failure)

expected_hmac = HMAC-SHA256(manifest_hmac_key,
                            snapshot_id_bytes_16 || manifest_envelope)
assert(expected_hmac == entry.manifest_hmac)

manifest_plaintext = BKEV_Decode(manifest_enc_key, manifest_envelope)
manifest = CBOR_Decode<Manifest>(manifest_plaintext)
assert(manifest.format_version == 2)
```

### 9.4 Read pack indices for every referenced pack

```
pack_index: map[chunk_id -> (pack_id, offset_in_pack, encrypted_size)] = empty

for pack_id in entry.packs_referenced:
    pack_bytes = dest_objects["packs/<pack_id>.pack"]
    assert(pack_bytes[0..4] == "BKPK")
    assert(big_endian_u16(pack_bytes[4..6]) == 1)
    footer_offset = little_endian_u64(pack_bytes[-8..])
    footer_cbor = pack_bytes[footer_offset .. -8]
    entries = CBOR_Decode<Vec<PackIndexEntry>>(footer_cbor)
    for e in entries:
        pack_index[e.chunk_id] = (pack_id, e.offset, e.size)
```

### 9.5 Walk the file tree and reconstruct each file

Walk `manifest.file_tree.root` recursively.  For each `TreeNode`:

- `node_type == 0` (Directory): create directory at the joined-path
  location.  Set `dir_mtime_ns` if non-zero.
- `node_type == 2` (Symlink): create symlink pointing at
  `file_entry.symlink_target`.  On Windows, the reader may instead
  record this as a regular file containing the target text - the
  Recovery Tool's default behaviour.
- `node_type == 1` (File): see below.

For a file:

```
target_path = join_path_components(ancestor_names, this_node.name)
out = open(target_path for writing)

for chunk_ref in this_node.file_entry.chunks:
    # Locate the encrypted chunk in its pack.
    (pack_id, offset, enc_size) = pack_index[chunk_ref.chunk_hash]
    pack_bytes = dest_objects["packs/<pack_id>.pack"]
    encrypted_chunk = pack_bytes[offset + 4 .. offset + 4 + enc_size]

    # Decrypt: nonce || ciphertext+tag
    nonce = encrypted_chunk[0..12]
    ctt = encrypted_chunk[12..]
    plaintext_compressed = AES-256-GCM-Decrypt(
        key   = chunk_enc_key,
        nonce = nonce,
        AAD   = chunk_ref.chunk_hash (32 bytes),
        input = ctt,
    )

    # Decompress.
    plaintext = zstd_decompress(plaintext_compressed)
    assert(plaintext.len() == chunk_ref.plaintext_size)

    # Optional: verify the chunk identity HMAC.
    expected = HMAC-SHA256(chunk_id_key, plaintext)
    assert(expected == chunk_ref.chunk_hash)

    # Write at the correct offset in the output file.
    out.seek(chunk_ref.plaintext_offset)
    out.write(plaintext)

# Restore metadata.
out.set_mtime(file_entry.mtime_ns)
out.set_mode(file_entry.mode)        # POSIX hosts only
out.set_ownership(uid, gid)          # POSIX hosts only; usually requires root
for (name, bytes) in file_entry.xattrs:
    out.set_xattr(name, bytes)
```

That is the complete restore.  No other state is consulted.

### 9.6 Cross-platform path handling

A manifest produced on Windows may include drive letters as
top-level path components (e.g., a `C:` directory containing
`Users`).  A reader running on a non-Windows host SHOULD:

- Replace `:` in path components with a safe substitute (the
  Recovery Tool's default: strip the trailing colon and treat
  `C` as a directory name).
- Convert backslashes if any appear in `name` (they shouldn't -
  the encoder splits on the OS path separator before recording -
  but defensive readers may want to convert).

Symmetric concerns apply to Unix-source archives restored on
Windows (`/` in filenames isn't expressible on NTFS; rare in
practice because directory names with `/` are illegal POSIX).

## 10. Versioning rules

- `format_version: 2` is the only version this spec reads; any other
  value must be rejected with a clear error pointing at the version
  field.
- A version `> 2` indicates a newer format.  The reader MUST
  refuse with a clear "this archive was produced by a newer
  Nyx Backup version" message; the v2 reader cannot safely
  interpret v3 fields.
- Backwards compatibility commitment: a v3 reader, if and when
  one ships, MUST be able to read v2 archives.  A version bump
  on the format implies Nyx Backup itself bumps a major version.

## 11. Reference test vectors

Test vectors that exercise the entire restore pipeline (a small
backup set with known plaintext, master key, encrypted pack,
manifest, and snapshot index) will be published as a downloadable
tarball alongside this spec at <https://nyxbackup.com/format/>.

The tarball contains:

- `master_key.hex` - the 64-character hex master key for this
  archive.
- `dest/` - the full bucket contents as a directory tree
  (`machines/...`, `indexes/...`, `manifests/...`, `packs/...`).
- `expected/` - the plaintext files that a correct restore should
  produce, byte-for-byte.
- `intermediate/` - derived subkeys, decoded CBOR JSON, and
  per-chunk decryption outputs at every step, for debugging an
  in-progress reader implementation.
- `README.md` - walkthrough of one example file restore from raw
  bytes to plaintext, with every intermediate value shown.

A reader implementation is considered conformant when it can
restore the contents of `dest/` to match `expected/` byte-for-byte
using only the published master key.

**Status**: test vectors are TODO and will land before the 1.0
launch.

## 12. Implementer notes

- HKDF, AES-GCM, and SHA-256 are FIPS-approved primitives (NIST
  SP 800-56C, SP 800-38D, FIPS 180-4).  The Nyx Backup writer
  routes these through the host OS's vendor-validated cryptographic
  module (BCryptPrimitives on Windows, CoreCrypto on macOS, AWS-LC
  FIPS Cert #4759 on Linux) for its FIPS positioning.  This is a
  writer-side detail only: independent readers may use any
  conformant implementation - the algorithms are widely supported
  and produce identical outputs across implementations.  The
  reference reader (this project) uses the pure-Rust RustCrypto
  crates and makes no FIPS claim of its own.
- Argon2id is documented by RFC 9106.  PBKDF2-HMAC-SHA256 is
  documented by RFC 8018.  Both have multiple conformant
  implementations in every modern language.
- zstd is documented by RFC 8478.  Use any conformant zstd
  decoder.
- CBOR is documented by RFC 8949.  Use any conformant CBOR
  decoder that supports text-string keys and standard integer /
  byte-string / array / map types.
- All UUIDs in this spec are RFC 4122 (variant 1, version 4) and
  carried as 16 raw bytes in CBOR.

## 13. Open-source reference reader

The Nyx Backup Recovery Tool is the reference implementation of
this specification, shipping open-source under Apache-2.0 at
<https://github.com/nyxbackup/nyxbackup-recovery>.  It is **not** a
required dependency for restore - anyone implementing their own
reader from this spec alone will produce correct results.

The reference reader exists as a sanity-check oracle: if your
implementation produces different bytes than the reference reader
for the same input archive, one of you has a bug, and the
discrepancy is worth investigating.  If the reference reader and
this document disagree, file an issue - this document is the
authoritative source and the reader will be corrected.

---

*Format version 2.  Stable.  Maintained alongside the Nyx Backup
Recovery reference reader.*
