// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! bkp-types - Shared domain types, error enums, and constants.
//!
//! All other crates depend on this crate. It has no internal dependencies.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod backup_set;
pub mod chunk;
pub mod endpoint;
pub mod error;
pub mod machine;
pub mod manifest;
pub mod retention;
pub mod secret;
pub mod snapshot;
