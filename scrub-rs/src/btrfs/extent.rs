//! Live extent- and csum-tree lookups that re-classify mismatched sectors at write time.

use super::util::le_u64;
use crate::fs::Reconfirm;

pub const TYPE_INLINE: u8 = 0;

/// On-disk EXTENT_DATA extent of one inode.
#[derive(Debug, Clone, Copy)]
pub struct FileExtent {
    pub inode: u64,

    pub file_offset: u64,
    /// EXTENT_DATA type code (0 = inline, 1 = regular, 2 = prealloc).
    pub extent_type: u8,

    /// Logical address of the extent's data on disk.
    pub disk_bytenr: u64,
    /// Allocated on-disk size (may exceed `num_bytes`).
    pub disk_num_bytes: u64,

    /// Byte offset into the disk extent where the file data starts.
    pub offset: u64,

    /// Number of file bytes this extent covers.
    pub num_bytes: u64,
}

impl FileExtent {
    /// Parse a regular or prealloc extent; inline extents (data carried in
    /// the item itself) return None.
    pub fn parse(buf: &[u8], inode: u64, file_offset: u64) -> Option<Self> {
        if buf.len() < 21 {
            return None;
        }
        let extent_type = buf[20];
        if extent_type == TYPE_INLINE {
            return None;
        }
        if buf.len() < 21 + 32 {
            return None;
        }
        let disk_bytenr = le_u64(buf, 21);
        let disk_num_bytes = le_u64(buf, 29);
        let offset = le_u64(buf, 37);
        let num_bytes = le_u64(buf, 45);
        Some(Self {
            inode,
            file_offset,
            extent_type,
            disk_bytenr,
            disk_num_bytes,
            offset,
            num_bytes,
        })
    }

    /// Logical address where the file data physically begins.
    pub fn disk_start(&self) -> u64 {
        self.disk_bytenr + self.offset
    }
}

/// EXTENT_ITEM flag bits.
pub mod extent_flag {
    /// Extent was written without checksums.
    pub const NODATASUM: u64 = 1 << 3;
}

/// Re-check one mismatched sector against live metadata at write time.
/// Returns Corruption (still bad — recover), Stale (freed or rewritten —
/// skip), or Unverifiable (metadata unreadable — skip).
#[allow(clippy::too_many_arguments)]
pub fn reconfirm_mismatch(
    reader: &mut crate::btrfs::reader::FsReader,
    chunk_map: &crate::btrfs::chunk::ChunkMap,
    extent_root: u64,
    csum_root: u64,
    logical: u64,
    stored: &[u8],
    hash_len: usize,
    sector_size: u64,
) -> Reconfirm {
    // A hole or a no-checksum extent means the sector was rewritten or
    // freed since the scan: nothing to recover.
    match extent_covers(reader, chunk_map, extent_root, logical) {
        ExtentLive::Hole => return Reconfirm::Stale,
        ExtentLive::NoDataSum => return Reconfirm::Stale,
        ExtentLive::Live => {}

        ExtentLive::Unreadable => return Reconfirm::Unverifiable,
    }

    // Still owned: the live checksum decides corruption vs. churn.
    match csum_at(reader, chunk_map, csum_root, logical, hash_len, sector_size) {
        CsumLive::None => Reconfirm::Stale,

        CsumLive::Unreadable => Reconfirm::Unverifiable,
        CsumLive::Some(live) => {
            if live == stored {
                Reconfirm::Corruption
            } else {
                Reconfirm::Stale
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExtentLive {
    Live,

    Hole,

    NoDataSum,

    Unreadable,
}

/// Find the EXTENT_ITEM covering `logical`, or classify the gap.
fn extent_covers(
    reader: &mut crate::btrfs::reader::FsReader,
    chunk_map: &crate::btrfs::chunk::ChunkMap,
    extent_root: u64,
    logical: u64,
) -> ExtentLive {
    let target =
        crate::btrfs::key::Key::new(logical, crate::btrfs::key::key_type::EXTENT_ITEM, u64::MAX);
    let mut node_logical = extent_root;

    for _depth in 0..16 {
        let res = reader
            .read_node(
                chunk_map,
                node_logical,
                crate::btrfs::reader::GEN_DONT_CHECK,
                None,
                None,
            )
            .ok();
        let node = match res {
            Some(r) if !r.all_mirrors_failed => r.node.unwrap(),
            _ => return ExtentLive::Unreadable,
        };
        match node {
            crate::btrfs::node::Node::Internal(internal) => {
                let count = internal
                    .ptrs
                    .partition_point(|ptr| key_leq(&ptr.key, &target));
                match count {
                    // Descend into the child whose key range can contain
                    // the target; 0 means the target lies before the
                    // first key — a hole.
                    0 => return ExtentLive::Hole,
                    _ => node_logical = internal.ptrs[count - 1].blockptr,
                }
            }
            crate::btrfs::node::Node::Leaf(leaf) => {
                let count = leaf
                    .slots
                    .partition_point(|slot| key_leq(&slot.key, &target));
                // Scan backwards for the last EXTENT_ITEM at or before the
                // target, then check whether it actually covers `logical`.
                let mut covering: Option<(usize, u64, u64)> = None;
                for i in (0..count).rev() {
                    let slot = &leaf.slots[i];
                    if slot.key.ty == crate::btrfs::key::key_type::EXTENT_ITEM {
                        covering = Some((i, slot.key.objectid, slot.key.offset));
                        break;
                    }
                }
                return match covering {
                    None => ExtentLive::Hole,
                    Some((idx, start, len)) => {
                        if start + len > logical {
                            if extent_is_nodatasum(&leaf, idx) {
                                ExtentLive::NoDataSum
                            } else {
                                ExtentLive::Live
                            }
                        } else {
                            ExtentLive::Hole
                        }
                    }
                };
            }
        }
    }
    ExtentLive::Unreadable
}

#[derive(Debug, Clone)]
enum CsumLive {
    Some(Vec<u8>),

    None,

    Unreadable,
}

/// Fetch the live checksum for the sector containing `logical` from the
/// CSUM_TREE.
fn csum_at(
    reader: &mut crate::btrfs::reader::FsReader,
    chunk_map: &crate::btrfs::chunk::ChunkMap,
    csum_root: u64,
    logical: u64,
    hash_len: usize,
    sector_size: u64,
) -> CsumLive {
    let sector = (logical / sector_size) * sector_size;
    let target = crate::btrfs::key::Key::new(
        crate::btrfs::key::objectid::EXTENT_CSUM_OBJECTID,
        crate::btrfs::key::key_type::EXTENT_CSUM,
        sector,
    );
    let mut node_logical = csum_root;
    for _depth in 0..16 {
        let res = reader
            .read_node(
                chunk_map,
                node_logical,
                crate::btrfs::reader::GEN_DONT_CHECK,
                None,
                None,
            )
            .ok();
        let node = match res {
            Some(r) if !r.all_mirrors_failed => r.node.unwrap(),
            _ => return CsumLive::Unreadable,
        };
        match node {
            crate::btrfs::node::Node::Internal(internal) => {
                let count = internal
                    .ptrs
                    .partition_point(|ptr| key_leq(&ptr.key, &target));
                match count {
                    // No child pointer precedes the target: the range has
                    // no csum item.
                    0 => return CsumLive::None,
                    _ => node_logical = internal.ptrs[count - 1].blockptr,
                }
            }
            crate::btrfs::node::Node::Leaf(leaf) => {
                let count = leaf
                    .slots
                    .partition_point(|slot| key_leq(&slot.key, &target));
                // The last EXTENT_CSUM item at or before the target sector;
                // return its hash if the item's run covers the sector.
                if count > 0 {
                    let i = count - 1;
                    let slot = &leaf.slots[i];
                    if slot.key.ty == crate::btrfs::key::key_type::EXTENT_CSUM {
                        let run_start = slot.key.offset;
                        let data = leaf.item_data(i);
                        if data.len() % hash_len == 0 {
                            let n = (data.len() / hash_len) as u64;
                            let run_end = run_start + n * sector_size;
                            if sector >= run_start && sector < run_end {
                                let idx = ((sector - run_start) / sector_size) as usize;
                                let base = idx * hash_len;
                                return CsumLive::Some(data[base..base + hash_len].to_vec());
                            }
                        }
                    }
                }
                return CsumLive::None;
            }
        }
    }
    CsumLive::Unreadable
}

/// Key comparison used for the tree descents; matches btrfs item order.
fn key_leq(a: &crate::btrfs::key::Key, b: &crate::btrfs::key::Key) -> bool {
    (a.objectid, a.ty as u32, a.offset) <= (b.objectid, b.ty as u32, b.offset)
}

/// True if the extent item's flags (bytes 16..24 of its data) carry
/// NODATASUM.
fn extent_is_nodatasum(leaf: &crate::btrfs::node::Leaf, slot_idx: usize) -> bool {
    let data = leaf.item_data(slot_idx);
    if data.len() < 24 {
        return false;
    }
    let flags = u64::from_le_bytes(data[16..24].try_into().unwrap());
    flags & extent_flag::NODATASUM != 0
}
