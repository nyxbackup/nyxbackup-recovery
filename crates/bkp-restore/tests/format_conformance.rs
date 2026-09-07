// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com
//
//! Format-conformance test: decode a real backup produced by the Nyx Backup
//! app and restore it byte-for-byte.
//!
//! WHY THIS EXISTS
//! ---------------
//! This tool's entire purpose is reading data written by another program.  Its
//! own unit tests cannot prove that: the encrypt / pack-build paths were
//! stripped, so there is nothing here to round-trip against, and a test that
//! re-implemented the reader would only prove the reimplementation agrees with
//! itself.  So the fixture under `tests/fixtures/format-v2/` is *written by the
//! main app's real writer* (`cargo run -p bkp-engine --example
//! gen_recovery_fixture` in the main repo) and committed here as opaque bytes.
//!
//! If the main app's format drifts - key derivation, envelope layout, CBOR
//! schema, pack framing, chunk-id derivation - this test fails, and we learn
//! that recovery is broken from a test run instead of from a user who has
//! already lost their machine.
//!
//! The test deliberately drives the PUBLIC read path (`RestoreEngine`,
//! `build_pack_map_from_storage`, `LocalBackend`) rather than poking at
//! internals, so it also covers pack discovery and reassembly, not just
//! decryption.
//!
//! The fixture's master key is synthetic and committed on purpose; it protects
//! nothing.  Regenerate the fixture only when the format changes deliberately.

use std::path::PathBuf;
use std::sync::Arc;

use bkp_crypto::keys::{KeyLabel, MasterKey};
use bkp_crypto::subkey::derive_subkey;
use bkp_restore::{
    OverwriteMode, RestoreEngine, RestoreOwner, RestoreTarget, build_pack_map_from_storage,
};
use bkp_storage::backend::StorageBackend;
use bkp_storage::backends::local::LocalBackend;
use bkp_types::backup_set::BackupSetId;
use bkp_types::snapshot::SnapshotId;
use uuid::Uuid;

/// Must match `gen_recovery_fixture.rs` in the main repo.
const FIXTURE_MASTER_KEY: [u8; 32] = [
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
    0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f,
];
const SET_UUID: &str = "11111111-1111-4111-8111-111111111111";
const SNAPSHOT_UUID: &str = "22222222-2222-4222-8222-222222222222";

/// The exact plaintext the fixture encodes.  Written out here, not derived, so
/// a change in the reader cannot quietly redefine what "correct" means.
const HELLO: &str = "hello recovery\n";
const NOTES: &str = "# nested\n\nThis file lives one directory down.\n";
const COMPRESSIBLE: &str = "nyx backup format conformance fixture. ";

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/format-v2")
}

fn backend() -> Arc<dyn StorageBackend> {
    Arc::new(LocalBackend::new(fixture_root()).expect("open fixture as local backend"))
}

fn ids() -> (BackupSetId, SnapshotId) {
    (
        BackupSetId::from_uuid(Uuid::parse_str(SET_UUID).unwrap()),
        SnapshotId::from_uuid(Uuid::parse_str(SNAPSHOT_UUID).unwrap()),
    )
}

/// The snapshot index decodes, and its summary matches what the writer stored.
#[tokio::test]
async fn snapshot_index_decodes() {
    let (set_id, snapshot_id) = ids();
    let master = MasterKey::from_bytes(FIXTURE_MASTER_KEY);
    let index_key = derive_subkey(&master, KeyLabel::SnapshotIndex, &set_id).unwrap();

    let engine = RestoreEngine::new(backend());
    let snaps = engine
        .list_snapshots(&set_id, &index_key)
        .await
        .expect("decode snapshot index");

    assert_eq!(snaps.len(), 1, "fixture has exactly one snapshot");
    let s = &snaps[0];
    assert_eq!(s.snapshot_id, snapshot_id);
    assert_eq!(s.files_total, 5, "3 files + big.txt + symlink");
    // created_at is stored in SECONDS by the engine despite the field name.
    assert_eq!(s.created_at, 1_767_225_600);
}

/// The manifest decodes and the file tree round-trips: names, nesting, the
/// symlink target, and the empty file all survive.
#[tokio::test]
async fn manifest_and_file_tree_decode() {
    let (set_id, snapshot_id) = ids();
    let master = MasterKey::from_bytes(FIXTURE_MASTER_KEY);
    let manifest_key = derive_subkey(&master, KeyLabel::ManifestEncryption, &set_id).unwrap();

    let engine = RestoreEngine::new(backend());
    let files = engine
        .list_snapshot_files(&snapshot_id, &set_id, &manifest_key)
        .await
        .expect("decode manifest");

    let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
    for want in [
        "big.txt",
        "empty.txt",
        "hello.txt",
        "link-to-hello",
        "nested",
        "nested/notes.md",
    ] {
        assert!(paths.contains(&want), "missing {want} in {paths:?}");
    }

    let nested = files.iter().find(|f| f.path == "nested").unwrap();
    assert!(nested.is_dir, "nested must decode as a directory");

    let link = files.iter().find(|f| f.path == "link-to-hello").unwrap();
    assert!(link.is_symlink, "symlink flag must survive decode");

    let empty = files.iter().find(|f| f.path == "empty.txt").unwrap();
    assert_eq!(empty.size, 0);

    let hello = files.iter().find(|f| f.path == "hello.txt").unwrap();
    assert_eq!(hello.size, HELLO.len() as u64);
}

/// The end-to-end proof: discover packs, resolve chunks, decrypt, decompress,
/// verify chunk ids, and reassemble every file byte-for-byte.
#[tokio::test]
async fn restores_files_byte_for_byte() {
    let (set_id, snapshot_id) = ids();
    let master = MasterKey::from_bytes(FIXTURE_MASTER_KEY);
    let chunk_key = derive_subkey(&master, KeyLabel::ChunkEncryption, &set_id).unwrap();
    let chunk_id_key = derive_subkey(&master, KeyLabel::ChunkIdentity, &set_id).unwrap();
    let manifest_key = derive_subkey(&master, KeyLabel::ManifestEncryption, &set_id).unwrap();

    let storage = backend();
    // Pack discovery: the fixture uses the flat `packs/<uuid>.pack` layout, and
    // the map is built by reading each pack's footer - no database, no
    // knowledge of which pack holds which chunk beforehand.
    let pack_map = build_pack_map_from_storage(storage.as_ref())
        .await
        .expect("build pack map");
    assert!(
        !pack_map.is_empty(),
        "no chunks discovered in fixture packs"
    );

    let dest = tempfile::tempdir().expect("tempdir");
    let engine = RestoreEngine::new_with_pack_cache(storage, pack_map);

    let big_expected = format!("{}{}", COMPRESSIBLE.repeat(64), COMPRESSIBLE.repeat(48));
    let cases: [(&str, &str); 3] = [
        ("hello.txt", HELLO),
        ("nested/notes.md", NOTES),
        // Two chunks with a non-zero plaintext_offset on the second - proves
        // reassembly ordering, not just single-chunk decryption.
        ("big.txt", big_expected.as_str()),
    ];

    for (path, expected) in cases {
        engine
            .restore_file(
                &snapshot_id,
                &set_id,
                path,
                RestoreTarget::Custom(dest.path().to_path_buf()),
                OverwriteMode::Replace,
                &chunk_key,
                &chunk_id_key,
                &manifest_key,
                RestoreOwner::default(),
            )
            .await
            .unwrap_or_else(|e| panic!("restore {path}: {e}"));

        let written = dest.path().join(path);
        let got = std::fs::read(&written)
            .unwrap_or_else(|e| panic!("read restored {}: {e}", written.display()));
        assert_eq!(
            String::from_utf8_lossy(&got),
            expected,
            "restored {path} does not match the original plaintext"
        );
    }
}

/// A wrong master key must FAIL, not silently return garbage.  Without this a
/// broken key-derivation change could pass the other tests by producing
/// self-consistent nonsense.
#[tokio::test]
async fn wrong_master_key_is_rejected() {
    let (set_id, _) = ids();
    let mut wrong = FIXTURE_MASTER_KEY;
    wrong[0] ^= 0xff;
    let master = MasterKey::from_bytes(wrong);
    let index_key = derive_subkey(&master, KeyLabel::SnapshotIndex, &set_id).unwrap();

    let engine = RestoreEngine::new(backend());
    let res = engine.list_snapshots(&set_id, &index_key).await;
    assert!(
        res.is_err(),
        "decoding with the wrong master key must fail, got {res:?}"
    );
}
