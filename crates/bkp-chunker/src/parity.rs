// Copyright (c) 2026 Nyx Software, LLC. All rights reserved.
// Nyx Backup - https://nyxbackup.com

//! Reed-Solomon repair for pack objects, from their `.par` sibling.
//!
//! **Read-only port.**  The main application both writes and repairs parity;
//! this crate only repairs.  There is no public `encode_parity` here, because
//! the Recovery Tool must never create or modify a `.par` object - it exists to
//! get data out of an archive, not to change one.  A test-only encoder lives at
//! the bottom of this file so the repair path can be proven against a real
//! `.par`; it is `#[cfg(test)]` and never compiled into the shipped binary.
//!
//! The other deliberate difference from the application: a successful repair is
//! **not** written back to storage.  The application heals the stored pack so
//! later reads are clean.  Here the repaired bytes are used for the restore in
//! hand and then discarded, because this tool may be pointed at an archive the
//! operator does not own, and mutating someone's backup during their disaster
//! is a risk that buys the current restore nothing.
//!
//! RS is an erasure code: it reconstructs when it knows *which* shards are
//! bad.  Corrupt (as opposed to merely absent) shards are identified by the
//! per-shard CRC32C stored in the `.par`, marked as erasures, and reconstructed
//! when at most `parity_shards` are erased.
//!
//! `.par` object layout (all multi-byte integers big-endian) - this must stay
//! byte-compatible with the application's writer:
//!
//! ```text
//! 0   4            magic "BKPR"
//! 4   2            par_version u16 = 1
//! 6   16           pack_id (raw UUID; must equal the .pack it protects)
//! 22  2            data_shards   K
//! 24  2            parity_shards M
//! 26  8            shard_size    (bytes per shard)
//! 34  8            pack_len      (original .pack length; last data shard padded)
//! 42  4            header_crc32c (CRC32C over bytes 0..42)
//! 46  (K+M)*4      shard_crc32c[]  (data shards 0..K-1, then parity K..K+M-1)
//! ..  M*shard_size parity shard bytes (M shards, in order)
//! ```

use bkp_types::error::{Error, Result};
use bkp_types::snapshot::PackId;
use reed_solomon_erasure::galois_8::ReedSolomon;

/// Magic bytes at the start of a `.par` object.
const PAR_MAGIC: &[u8; 4] = b"BKPR";
/// Version of the `.par` object layout.
const PAR_VERSION: u16 = 1;
/// Header length up to and including `header_crc32c`, before the CRC table.
const PAR_HEADER_LEN: usize = 46;

/// Outcome of a [`repair_pack`] call.
#[derive(Debug, Clone)]
pub struct RepairOutcome {
    /// The pack bytes, repaired if any shard was reconstructed.
    pub bytes: Vec<u8>,
    /// True if any shard had to be reconstructed (the input was corrupt).
    pub repaired: bool,
    /// Number of shards that were missing or corrupt (erasures).
    pub erasures: usize,
}

fn rs_err(e: reed_solomon_erasure::Error) -> Error {
    Error::Internal(format!("reed-solomon: {e}"))
}

/// Split `pack_bytes` into `k` data shards of `shard_size`, zero-padding the
/// tail so every shard is exactly `shard_size` bytes.  A pack shorter than the
/// aligned length (truncated / partially lost) yields zero-filled tail shards,
/// which the CRC check then flags as erasures.
fn split_data_shards(pack_bytes: &[u8], k: usize, shard_size: usize) -> Vec<Vec<u8>> {
    let mut shards = Vec::with_capacity(k);
    for i in 0..k {
        let start = i * shard_size;
        let mut shard = vec![0u8; shard_size];
        if start < pack_bytes.len() {
            let end = (start + shard_size).min(pack_bytes.len());
            shard[..end - start].copy_from_slice(&pack_bytes[start..end]);
        }
        shards.push(shard);
    }
    shards
}

/// Parsed `.par` geometry, CRC table, and parity shards.
struct ParFile {
    k: usize,
    m: usize,
    shard_size: usize,
    pack_len: usize,
    crcs: Vec<u32>,       // K + M entries
    parity: Vec<Vec<u8>>, // M shards
}

fn parse_par(par_bytes: &[u8], pack_id: &PackId) -> Result<ParFile> {
    if par_bytes.len() < PAR_HEADER_LEN {
        return Err(Error::Internal("parity object too short".into()));
    }
    if &par_bytes[0..4] != PAR_MAGIC {
        return Err(Error::Internal("parity object bad magic".into()));
    }
    let ver = u16::from_be_bytes([par_bytes[4], par_bytes[5]]);
    if ver != PAR_VERSION {
        return Err(Error::Internal(format!("unsupported parity version {ver}")));
    }
    let stored_hdr_crc =
        u32::from_be_bytes([par_bytes[42], par_bytes[43], par_bytes[44], par_bytes[45]]);
    if crc32c::crc32c(&par_bytes[0..42]) != stored_hdr_crc {
        return Err(Error::Internal("parity header CRC mismatch".into()));
    }
    if &par_bytes[6..22] != pack_id.as_bytes() {
        return Err(Error::Internal("parity object pack id mismatch".into()));
    }
    let k = u16::from_be_bytes([par_bytes[22], par_bytes[23]]) as usize;
    let m = u16::from_be_bytes([par_bytes[24], par_bytes[25]]) as usize;
    // Fixed-width reads out of a possibly-corrupt object: convert rather than
    // unwrap, so a malformed `.par` is reported and the caller can fall back to
    // the original decrypt error instead of the process dying mid-restore.
    let be_u64 = |at: usize| -> Result<u64> {
        par_bytes
            .get(at..at + 8)
            .and_then(|s| <[u8; 8]>::try_from(s).ok())
            .map(u64::from_be_bytes)
            .ok_or_else(|| Error::Internal("parity header truncated".into()))
    };
    let shard_size = be_u64(26)? as usize;
    let pack_len = be_u64(34)? as usize;
    if k == 0 || m == 0 || k + m > 256 || shard_size == 0 {
        return Err(Error::Internal("parity geometry invalid".into()));
    }
    let expected = PAR_HEADER_LEN + (k + m) * 4 + m * shard_size;
    if par_bytes.len() < expected {
        return Err(Error::Internal("parity object truncated".into()));
    }
    let mut crcs = Vec::with_capacity(k + m);
    let mut off = PAR_HEADER_LEN;
    for _ in 0..(k + m) {
        let word = par_bytes
            .get(off..off + 4)
            .and_then(|s| <[u8; 4]>::try_from(s).ok())
            .ok_or_else(|| Error::Internal("parity CRC table truncated".into()))?;
        crcs.push(u32::from_be_bytes(word));
        off += 4;
    }
    let mut parity = Vec::with_capacity(m);
    for _ in 0..m {
        parity.push(par_bytes[off..off + shard_size].to_vec());
        off += shard_size;
    }
    Ok(ParFile {
        k,
        m,
        shard_size,
        pack_len,
        crcs,
        parity,
    })
}

/// Repair `pack_bytes` using its `.par` sibling.
///
/// Returns the original bytes untouched when nothing is corrupt
/// (`repaired == false`), the reconstructed bytes when up to `parity_shards`
/// shards were lost, or [`Error::IntegrityMismatch`] when too many shards are
/// corrupt to recover.
///
/// The caller is responsible for NOT writing the result back to storage - see
/// the module docs for why this fork does not heal the stored object.
pub fn repair_pack(
    pack_bytes: Vec<u8>,
    par_bytes: &[u8],
    pack_id: &PackId,
) -> Result<RepairOutcome> {
    let par = parse_par(par_bytes, pack_id)?;
    let k = par.k;
    let m = par.m;

    // Rebuild the K data shards from the (possibly corrupt) pack bytes, then
    // assemble all K+M shards as Option, flagging CRC mismatches as erasures.
    let data_shards = split_data_shards(&pack_bytes, k, par.shard_size);
    let mut shards: Vec<Option<Vec<u8>>> = Vec::with_capacity(k + m);
    let mut erasures = 0usize;
    for (i, shard) in data_shards.into_iter().enumerate() {
        if crc32c::crc32c(&shard) == par.crcs[i] {
            shards.push(Some(shard));
        } else {
            shards.push(None);
            erasures += 1;
        }
    }
    for (j, shard) in par.parity.into_iter().enumerate() {
        if crc32c::crc32c(&shard) == par.crcs[k + j] {
            shards.push(Some(shard));
        } else {
            shards.push(None);
            erasures += 1;
        }
    }

    if erasures == 0 {
        return Ok(RepairOutcome {
            bytes: pack_bytes,
            repaired: false,
            erasures: 0,
        });
    }
    if erasures > m {
        return Err(Error::IntegrityMismatch {
            expected: format!("at most {m} corrupt shards"),
            actual: format!("{erasures} corrupt shards (unrepairable)"),
        });
    }

    let rs = ReedSolomon::new(k, m).map_err(rs_err)?;
    rs.reconstruct(&mut shards).map_err(rs_err)?;

    let mut repaired = Vec::with_capacity(k * par.shard_size);
    for shard in shards.iter().take(k) {
        repaired.extend_from_slice(shard.as_ref().expect("reconstructed data shard"));
    }
    repaired.truncate(par.pack_len);
    Ok(RepairOutcome {
        bytes: repaired,
        repaired: true,
        erasures,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bkp_types::parity::ParityGeometry;

    /// Test-only `.par` writer.
    ///
    /// The shipped crate has no encoder on purpose (see the module docs), but
    /// the repair path is worth nothing unless it is proven against a real
    /// `.par` rather than a hand-rolled fixture.  Kept byte-identical to the
    /// application's `bkp_pack::parity::encode_parity` - if the two ever
    /// diverge, these tests pass while the Recovery Tool fails on live data,
    /// which is the whole failure mode this fork keeps hitting.
    fn encode_parity(pack_bytes: &[u8], pack_id: &PackId, geom: ParityGeometry) -> Result<Vec<u8>> {
        geom.validate().map_err(Error::Internal)?;
        let k = geom.data_shards as usize;
        let m = geom.parity_shards as usize;
        let pack_len = pack_bytes.len();
        let shard_size = pack_len.div_ceil(k).max(1);

        let mut shards = split_data_shards(pack_bytes, k, shard_size);
        for _ in 0..m {
            shards.push(vec![0u8; shard_size]);
        }

        let rs = ReedSolomon::new(k, m).map_err(rs_err)?;
        rs.encode(&mut shards).map_err(rs_err)?;

        let crcs: Vec<u32> = shards.iter().map(|s| crc32c::crc32c(s)).collect();

        let mut out = Vec::with_capacity(PAR_HEADER_LEN + crcs.len() * 4 + m * shard_size);
        out.extend_from_slice(PAR_MAGIC);
        out.extend_from_slice(&PAR_VERSION.to_be_bytes());
        out.extend_from_slice(pack_id.as_bytes());
        out.extend_from_slice(&geom.data_shards.to_be_bytes());
        out.extend_from_slice(&geom.parity_shards.to_be_bytes());
        out.extend_from_slice(&(shard_size as u64).to_be_bytes());
        out.extend_from_slice(&(pack_len as u64).to_be_bytes());
        let header_crc = crc32c::crc32c(&out);
        out.extend_from_slice(&header_crc.to_be_bytes());
        for c in &crcs {
            out.extend_from_slice(&c.to_be_bytes());
        }
        for shard in &shards[k..] {
            out.extend_from_slice(shard);
        }
        Ok(out)
    }

    fn sample_pack(n: usize) -> Vec<u8> {
        (0..n)
            .map(|i| (i.wrapping_mul(7).wrapping_add(3)) as u8)
            .collect()
    }

    fn geom(k: u16, m: u16) -> ParityGeometry {
        ParityGeometry {
            data_shards: k,
            parity_shards: m,
        }
    }

    #[test]
    fn intact_pack_is_returned_untouched() {
        let pack = sample_pack(10_000);
        let id = PackId::new();
        let par = encode_parity(&pack, &id, geom(10, 2)).unwrap();
        let out = repair_pack(pack.clone(), &par, &id).unwrap();
        assert!(!out.repaired, "an intact pack must not be reconstructed");
        assert_eq!(out.erasures, 0);
        assert_eq!(out.bytes, pack);
    }

    #[test]
    fn corrupt_shard_is_reconstructed_byte_for_byte() {
        let pack = sample_pack(10_000);
        let id = PackId::new();
        let par = encode_parity(&pack, &id, geom(10, 2)).unwrap();

        // Flip bytes inside the first shard.
        let mut damaged = pack.clone();
        for b in damaged.iter_mut().take(64) {
            *b ^= 0xFF;
        }

        let out = repair_pack(damaged, &par, &id).unwrap();
        assert!(out.repaired);
        assert_eq!(out.erasures, 1);
        assert_eq!(
            out.bytes, pack,
            "a repaired pack must be byte-identical to the original"
        );
    }

    #[test]
    fn damage_beyond_the_parity_budget_is_refused_not_guessed() {
        // 2 parity shards can cover 2 erasures; damage 3.
        let pack = sample_pack(10_000);
        let id = PackId::new();
        let par = encode_parity(&pack, &id, geom(10, 2)).unwrap();
        let shard = 10_000usize.div_ceil(10);

        let mut damaged = pack.clone();
        for s in 0..3 {
            let at = s * shard;
            for b in damaged.iter_mut().skip(at).take(16) {
                *b ^= 0xFF;
            }
        }

        let err = repair_pack(damaged, &par, &id).unwrap_err();
        assert!(
            matches!(err, Error::IntegrityMismatch { .. }),
            "unrepairable damage must be reported, never silently returned: {err:?}"
        );
    }

    #[test]
    fn truncated_pack_is_reconstructed() {
        // Truncation is the shape a partial write or a bad tail sector takes:
        // the missing bytes read as zero-filled shards, which fail their CRC.
        let pack = sample_pack(10_000);
        let id = PackId::new();
        let par = encode_parity(&pack, &id, geom(10, 2)).unwrap();

        let mut truncated = pack.clone();
        truncated.truncate(9_000);

        let out = repair_pack(truncated, &par, &id).unwrap();
        assert!(out.repaired);
        assert_eq!(out.bytes, pack);
    }

    #[test]
    fn par_for_a_different_pack_is_rejected() {
        // A mismatched sibling must be refused rather than used to "repair"
        // one pack with another's parity, which would produce plausible
        // garbage.
        let pack = sample_pack(4_096);
        let id = PackId::new();
        let other = PackId::new();
        let par = encode_parity(&pack, &other, geom(4, 2)).unwrap();
        assert!(repair_pack(pack, &par, &id).is_err());
    }

    #[test]
    fn corrupt_par_header_is_rejected() {
        let pack = sample_pack(4_096);
        let id = PackId::new();
        let mut par = encode_parity(&pack, &id, geom(4, 2)).unwrap();
        par[24] ^= 0xFF; // parity_shards, inside the header CRC's range
        assert!(repair_pack(pack, &par, &id).is_err());
    }
}
