// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com
//
//! Mixed-master-key endpoint discovery.
//!
//! One endpoint can hold backup sets from several machines, and master keys are
//! PER MACHINE.  Google Drive makes this the default rather than the exception:
//! `appDataFolder` is a single flat per-account space with no sub-folders and
//! no path field in the Connect screen, so every machine that ever backed up to
//! that Google account lands in one namespace.
//!
//! The failure mode this guards is not a crash - it is silence.  A set that
//! cannot be decrypted with the pasted key used to be skipped without a word,
//! so a user looking for a machine that IS backed up saw a short list and
//! reasonably concluded the backup was gone.  During a disaster recovery that
//! is the worst possible wrong answer.
//!
//! The fixture under `tests/fixtures/mixed-keys/` contains two real sets
//! written by the main app's writer under two DIFFERENT master keys, which is
//! the only way to exercise this for real.

use std::sync::Arc;

use bkp_crypto::keys::MasterKey;
use bkp_recover::commands::discover_snapshots;
use bkp_storage::backend::StorageBackend;
use bkp_storage::backends::local::LocalBackend;

/// Opens the first set in the fixture; cannot open the second.
const OUR_KEY: [u8; 32] = [
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
    0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f,
];

/// The "other machine" key that wrote the second set.
const FOREIGN_KEY: [u8; 32] = [
    0xa0, 0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xab, 0xac, 0xad, 0xae, 0xaf,
    0xb0, 0xb1, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xbb, 0xbc, 0xbd, 0xbe, 0xbf,
];

const OUR_SET: &str = "11111111-1111-4111-8111-111111111111";
const FOREIGN_SET: &str = "55555555-5555-4555-8555-555555555555";

fn backend() -> Arc<dyn StorageBackend> {
    let root =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mixed-keys");
    Arc::new(LocalBackend::new(root).expect("open mixed-key fixture"))
}

/// The headline case: our set is listed, the other machine's set is COUNTED,
/// not silently dropped.
#[tokio::test]
async fn foreign_set_is_reported_not_hidden() {
    let reply = discover_snapshots(backend().as_ref(), &MasterKey::from_bytes(OUR_KEY))
        .await
        .expect("our key opens at least one set, so this must not error");

    assert_eq!(
        reply.undecryptable_sets, 1,
        "the other machine's set must be reported, not dropped"
    );

    let sets: Vec<&str> = reply
        .snapshots
        .iter()
        .map(|s| s.set_id.as_str())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    assert_eq!(sets, vec![OUR_SET], "only our set should be readable");
    assert!(
        !reply.snapshots.iter().any(|s| s.set_id == FOREIGN_SET),
        "a set we cannot decrypt must never appear as browsable"
    );
}

/// Same endpoint, the other machine's key: the mirror image.  Proves the count
/// tracks the key rather than something incidental about set ordering.
#[tokio::test]
async fn other_key_sees_the_other_set() {
    let reply = discover_snapshots(backend().as_ref(), &MasterKey::from_bytes(FOREIGN_KEY))
        .await
        .expect("the foreign key opens its own set");

    assert_eq!(reply.undecryptable_sets, 1);
    assert!(
        reply.snapshots.iter().all(|s| s.set_id == FOREIGN_SET),
        "expected only the foreign set, got {:?}",
        reply
            .snapshots
            .iter()
            .map(|s| &s.set_id)
            .collect::<Vec<_>>()
    );
}

/// A key that opens NOTHING is a different situation - the user pasted the
/// wrong key entirely - and must be a hard error carrying guidance, not an
/// empty list with a quiet footnote.
#[tokio::test]
async fn key_that_opens_nothing_is_an_error() {
    let mut wrong = OUR_KEY;
    wrong[0] ^= 0xff;

    let res = discover_snapshots(backend().as_ref(), &MasterKey::from_bytes(wrong)).await;
    let err = res.expect_err("a key that opens no set must error");
    assert!(
        err.contains("could not decode") || err.contains("2 backup set"),
        "error should tell the user how many sets were found and why none opened: {err}"
    );
}

/// Sanity: the readable set really is readable, so the assertions above are
/// about key scoping rather than a fixture that fails to decode at all.
#[tokio::test]
async fn readable_set_has_its_snapshot() {
    let reply = discover_snapshots(backend().as_ref(), &MasterKey::from_bytes(OUR_KEY))
        .await
        .expect("discovery");

    assert_eq!(reply.snapshots.len(), 1, "fixture set has one snapshot");
    let snap = &reply.snapshots[0];
    assert_eq!(snap.set_name, "Conformance Fixture");
    assert_eq!(snap.hostname, "fixture-host");
    assert_eq!(snap.files_total, 5);
}
