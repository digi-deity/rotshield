//! A logical-address reader for a single-device btrfs filesystem.
//!
//! Owns the open backing store (a regular file or a block device — both are
//! just seekable byte streams to `std::fs::File`), knows the node size, and
//! is given a `&ChunkMap` for logical→physical translation on every read.
//!
//! Reads go through the backing store the caller opened — typically an
//! array-partition device like `/dev/nmd1p1`.  This means the NonRAID
//! array driver is in the read path: for a present but corrupt disk the
//! corruption is visible, but for a *missing* disk the driver
//! transparently reconstructs from parity and the scrub cannot detect
//! it.  This is a known limitation, not a bug — see the "Address spaces
//! and I/O paths" doc in `array::mod` for details.
//!
//! The chunk map is deliberately **not** owned here.  It is immutable after
//! the chunk-tree walk and shared by reference with anything that needs
//! logical→physical resolution — including the scrub loop's caller, which
//! may want to resolve a mismatch's physical location for inline recovery.
//! Keeping the map as a separate `&` borrow from `&mut FsReader` lets both
//! happen without cloning or buffering.

use std::fs::File;

use super::chunk::ChunkMap;
use super::util::read_at;

pub struct FsReader {
    pub fp: File,
    pub node_size: usize,
    /// Byte offset added to every physical read.  0 for a bare btrfs image
    /// or an array partition (/dev/nmd1p1); the partition start (e.g.
    /// rdevOffset*512) for a whole-disk image or a raw rdev.  File and
    /// device paths share this single offset — there is no separate
    /// code path per backing-store kind.
    pub base_offset: u64,
}

impl FsReader {
    /// Read `n` bytes starting at logical address `logical`, using
    /// `chunk_map` for the logical→physical translation.
    ///
    /// For now this only handles the simple case where `n` lies entirely
    /// within a single chunk's first stripe — sufficient for single-device
    /// and mirrored (DUP/RAID1) filesystems, which is what this tool targets.
    pub fn read_logical(
        &mut self,
        chunk_map: &ChunkMap,
        logical: u64,
        n: usize,
    ) -> std::io::Result<Vec<u8>> {
        let (_devid, phys) = chunk_map.lookup(logical).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("no chunk mapping for logical 0x{logical:x}"),
            )
        })?;
        // The single point where file-vs-device is irrelevant: `read_at` is
        // just seek+read on a `std::fs::File`, which works on either.
        read_at(&mut self.fp, self.base_offset + phys, n)
    }

    /// Read and parse the B-tree node at logical address `logical`.
    pub fn read_node(
        &mut self,
        chunk_map: &ChunkMap,
        logical: u64,
    ) -> std::io::Result<super::node::Node> {
        let buf = self.read_logical(chunk_map, logical, self.node_size)?;
        Ok(super::node::Node::parse(buf))
    }
}