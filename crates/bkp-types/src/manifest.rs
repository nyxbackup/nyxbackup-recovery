// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! Manifest and file tree types.
//!
//! These types mirror the CBOR manifest format defined in data format spec Section 9.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::chunk::ChunkId;
use crate::snapshot::SnapshotId;

/// A lightweight reference to a manifest stored at a remote path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestRef {
    /// The snapshot this manifest belongs to.
    pub snapshot_id: SnapshotId,
    /// Remote path of the encrypted manifest object, relative to the endpoint prefix.
    pub remote_path: String,
    /// Byte size of the encrypted manifest object.
    pub size_bytes: u64,
}

/// Type discriminant for a [`TreeNode`].
///
/// Numeric values match the CBOR encoding in data format spec Section 9.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum NodeType {
    /// A directory (CBOR 0).
    Directory = 0,
    /// A regular file (CBOR 1).
    File = 1,
    /// A symbolic link (CBOR 2).
    Symlink = 2,
}

/// A reference from a file to one content chunk.
///
/// Corresponds to `ChunkRef` in data format spec Section 9.1.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkRef {
    /// Content-addressing key for this chunk (see [`ChunkId`] for the family).
    pub chunk_hash: ChunkId,
    /// Byte offset of this chunk within the reconstructed file plaintext.
    pub plaintext_offset: u64,
    /// Byte length of this chunk's plaintext.
    pub plaintext_size: u64,
}

/// Metadata and content-chunk list for a single file.
///
/// Corresponds to `FileEntry` in data format spec Section 9.1.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    /// File size in bytes.
    pub size: u64,
    /// Modification time as Unix nanoseconds.
    pub mtime_ns: u64,
    /// Inode change time as Unix nanoseconds.
    pub ctime_ns: u64,
    /// POSIX permission bits (0 on Windows).
    pub mode: u32,
    /// Owner UID (0 on Windows).
    pub owner_uid: u32,
    /// Owner GID (0 on Windows).
    pub owner_gid: u32,
    /// Windows file attribute flags (0 on non-Windows).
    pub windows_attrs: u32,
    /// Extended attributes map (name → raw bytes).
    pub xattrs: HashMap<String, Vec<u8>>,
    /// Symlink target path, or `None` for regular files.
    pub symlink_target: Option<String>,
    /// Ordered list of chunk references that together reconstruct the file.
    pub chunks: Vec<ChunkRef>,
}

/// A node in the backup file tree (directory, file, or symlink).
///
/// Corresponds to `TreeNode` in data format spec Section 9.1.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TreeNode {
    /// Whether this node is a directory, file, or symlink.
    pub node_type: NodeType,
    /// File or directory name component only (not a full path).
    pub name: String,
    /// Child nodes; populated only when `node_type == Directory`.
    pub children: Vec<TreeNode>,
    /// File metadata and chunk list; `Some` only when `node_type == File` or `Symlink`.
    pub file_entry: Option<FileEntry>,
    /// Modification time (Unix nanoseconds) for directory nodes; 0 when not recorded.
    /// Deserialises as 0 on manifests produced before this field was added.
    #[serde(default)]
    pub dir_mtime_ns: u64,
}

impl TreeNode {
    /// Construct a directory node with an explicit modification time.
    pub fn directory(name: impl Into<String>, children: Vec<TreeNode>, mtime_ns: u64) -> Self {
        Self {
            node_type: NodeType::Directory,
            name: name.into(),
            children,
            file_entry: None,
            dir_mtime_ns: mtime_ns,
        }
    }

    /// Construct a file node.
    pub fn file(name: impl Into<String>, entry: FileEntry) -> Self {
        Self {
            node_type: NodeType::File,
            name: name.into(),
            children: Vec::new(),
            file_entry: Some(entry),
            dir_mtime_ns: 0,
        }
    }

    /// Construct a symlink node.
    pub fn symlink(name: impl Into<String>, target: impl Into<String>) -> Self {
        let entry = FileEntry {
            size: 0,
            mtime_ns: 0,
            ctime_ns: 0,
            mode: 0,
            owner_uid: 0,
            owner_gid: 0,
            windows_attrs: 0,
            xattrs: HashMap::new(),
            symlink_target: Some(target.into()),
            chunks: Vec::new(),
        };
        Self {
            node_type: NodeType::Symlink,
            name: name.into(),
            children: Vec::new(),
            file_entry: Some(entry),
            dir_mtime_ns: 0,
        }
    }
}

/// The complete file tree recorded in a manifest snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileTree {
    /// The root directory node containing the entire tree.
    pub root: TreeNode,
}
