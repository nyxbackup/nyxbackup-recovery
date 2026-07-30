// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! Master-key derivation from user passphrase.
//!
//! Two algorithms are supported.  The choice is recorded in the
//! machine record (`Argon2Params.algorithm`) at install time and is
//! immutable thereafter - switching after the first successful backup
//! would invalidate every backup's master key.
//!
//! - **Argon2id** (default): RustCrypto `argon2` crate.  Strongest GPU
//!   brute-force resistance.  NOT FIPS-approved as of NIST SP 800-132
//!   Rev 1; under consideration for Rev 2.
//! - **PBKDF2-HMAC-SHA256**: routed through the platform vendor's
//!   FIPS-validated module via [`crate::backend::pbkdf2_hmac_sha256`]
//!   (BCryptDeriveKeyPBKDF2 on Windows, CCKeyDerivationPBKDF on macOS,
//!   aws-lc-rs `pbkdf2::derive` on Linux with `--features fips`).
//!   1,000,000 iterations (well above OWASP 2023 600k recommendation).
//!
//! # Passphrase normalisation
//!
//! NIST SP 800-132 §5.1 recommends UTF-8 NFC normalisation of the
//! input password before hashing so that two byte-different but
//! visually-identical strings produce the same key (e.g., "café"
//! entered with combining acute vs precomposed é).  We apply
//! Unicode NFC normalisation at the entry point of
//! `derive_master_key` for both algorithms.

use unicode_normalization::UnicodeNormalization;

use bkp_types::error::{Error, Result};
use bkp_types::machine::{Argon2Params, KdfAlgorithm};

use crate::keys::MasterKey;

/// PBKDF2-HMAC-SHA256 iteration count for master-key derivation.
/// Set well above OWASP 2023's 600k recommendation; ~500 ms unlock
/// latency on contemporary hardware is acceptable for a once-per-
/// session derivation that protects every backup's encryption key.
pub const PBKDF2_MASTER_ITERATIONS: u32 = 1_000_000;

/// Derive a [`MasterKey`] from `passphrase` using the KDF algorithm and
/// parameters in `params`.
///
/// The passphrase is NFC-normalised before being fed to the KDF
/// so that byte-different but Unicode-equivalent strings
/// produce the same key.
///
/// This is an intentionally slow operation.  Call it once at session
/// start and hold the resulting key in memory for the duration of the
/// backup session.
pub fn derive_master_key(passphrase: &str, params: &Argon2Params) -> Result<MasterKey> {
    if params.output_len != 32 {
        return Err(Error::Crypto(format!(
            "KDF output_len must be 32, got {}",
            params.output_len
        )));
    }
    // NIST SP 800-132 §5.1: NFC-normalise the password before hashing.
    let passphrase_nfc: String = passphrase.nfc().collect();
    let mut arr = [0u8; 32];

    match params.algorithm {
        KdfAlgorithm::Argon2id => {
            use argon2::{Algorithm, Argon2, Params, Version};
            let argon2_params = Params::new(
                params.m_cost,
                params.t_cost,
                params.p_cost,
                Some(params.output_len),
            )
            .map_err(|e| Error::Crypto(format!("invalid Argon2id params: {e}")))?;
            let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, argon2_params);
            argon2
                .hash_password_into(passphrase_nfc.as_bytes(), &params.salt, &mut arr)
                .map_err(|e| Error::Crypto(format!("Argon2id derivation failed: {e}")))?;
        }
        KdfAlgorithm::Pbkdf2HmacSha256 => {
            // routed through the per-OS backend so the PBKDF2
            // implementation comes from the platform vendor's FIPS-validated
            // module.  Both the iteration loop AND the HMAC-SHA256 PRF live
            // inside the vendor-validated boundary.  m_cost / p_cost are
            // ignored; PBKDF2 has only an iteration count and a salt.
            let derived = crate::backend::pbkdf2_hmac_sha256(
                passphrase_nfc.as_bytes(),
                &params.salt,
                PBKDF2_MASTER_ITERATIONS,
                32,
            )?;
            arr.copy_from_slice(&derived);
        }
    }

    Ok(MasterKey::from_bytes(arr))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_params(algorithm: KdfAlgorithm) -> Argon2Params {
        // Minimal Argon2id params so tests run fast.  PBKDF2 uses
        // PBKDF2_MASTER_ITERATIONS regardless of the m_cost/t_cost values,
        // so we set them to defaults that satisfy Argon2id's lower bounds
        // when the algorithm is Argon2id.
        Argon2Params {
            algorithm,
            m_cost: 8,
            t_cost: 1,
            p_cost: 1,
            output_len: 32,
            salt: *b"test-salt-32-bytes-padding-here!",
        }
    }

    #[test]
    fn argon2_derivation_is_deterministic() {
        let p = test_params(KdfAlgorithm::Argon2id);
        let k1 = derive_master_key("passphrase", &p).unwrap();
        let k2 = derive_master_key("passphrase", &p).unwrap();
        assert_eq!(k1.as_bytes(), k2.as_bytes());
    }

    #[test]
    fn different_passphrase_different_key_argon2() {
        let p = test_params(KdfAlgorithm::Argon2id);
        let k1 = derive_master_key("passphrase1", &p).unwrap();
        let k2 = derive_master_key("passphrase2", &p).unwrap();
        assert_ne!(k1.as_bytes(), k2.as_bytes());
    }

    #[test]
    fn different_salt_different_key_argon2() {
        let p1 = test_params(KdfAlgorithm::Argon2id);
        let mut p2 = test_params(KdfAlgorithm::Argon2id);
        p2.salt[0] ^= 0xFF;
        let k1 = derive_master_key("passphrase", &p1).unwrap();
        let k2 = derive_master_key("passphrase", &p2).unwrap();
        assert_ne!(k1.as_bytes(), k2.as_bytes());
    }

    #[test]
    fn nfc_normalisation_collapses_equivalent_inputs() {
        // "café" with precomposed é (U+00E9) vs combining acute
        // (U+0065 U+0301) - byte-different but NFC-equivalent.
        let precomposed = "café";
        let decomposed = "cafe\u{0301}";
        assert_ne!(precomposed.as_bytes(), decomposed.as_bytes());
        let p = test_params(KdfAlgorithm::Argon2id);
        let k1 = derive_master_key(precomposed, &p).unwrap();
        let k2 = derive_master_key(decomposed, &p).unwrap();
        assert_eq!(
            k1.as_bytes(),
            k2.as_bytes(),
            "NFC normalisation should collapse precomposed and decomposed forms"
        );
    }

    #[test]
    fn argon2_and_pbkdf2_produce_different_keys() {
        // Sanity: switching algorithm produces a different key for the
        // same passphrase + salt (the immutability lock exists because
        // this difference invalidates every backup).
        let mut p_argon = test_params(KdfAlgorithm::Argon2id);
        let mut p_pbkdf = test_params(KdfAlgorithm::Pbkdf2HmacSha256);
        // Use the same salt for both so any difference is algorithm-only.
        let salt = [0x42u8; 32];
        p_argon.salt = salt;
        p_pbkdf.salt = salt;
        let k1 = derive_master_key("passphrase", &p_argon).unwrap();
        let k2 = derive_master_key("passphrase", &p_pbkdf).unwrap();
        assert_ne!(k1.as_bytes(), k2.as_bytes());
    }
}
