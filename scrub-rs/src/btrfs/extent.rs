//! FileExtentItem parsing — just enough to find REGULAR data extents.
//!
//! Layout of `struct btrfs_file_extent_item` (non-inline / REGULAR type):
//!   +0   generation u64
//!   +8   ram_bytes u64
//!   +16  compression u8
//!   +17  encryption u8
//!   +18  other_encoding u16
//!   +20  type u8  (0=INLINE, 1=REGULAR, 2=PREALLOC)
//!   +21  -- if not INLINE: ExtentDataRef --
//!        disk_bytenr  u64  (logical addr of the on-disk extent; 0 = sparse)
//!        disk_num_bytes u64 (size of the on-disk extent)
//!        offset       u64  (offset within the extent)
//!        num_bytes    u64  (logical bytes in the file)

use super::util::le_u64;

pub const TYPE_INLINE: u8 = 0;
pub const TYPE_REGULAR: u8 = 1;
pub const TYPE_PREALLOC: u8 = 2;

/// A parsed REGULAR/PREALLOC file extent (the fields the scrub needs).
#[derive(Debug, Clone, Copy)]
pub struct FileExtent {
    /// btrfs key.objectid — the inode this extent belongs to.
    pub inode: u64,
    /// btrfs key.offset — the file offset where this extent starts.
    pub file_offset: u64,
    pub extent_type: u8,
    /// Logical disk address of the extent (0 = sparse/hole).
    pub disk_bytenr: u64,
    pub disk_num_bytes: u64,
    /// Offset within the on-disk extent.
    pub offset: u64,
    /// Number of bytes covered in the file.
    pub num_bytes: u64,
}

impl FileExtent {
    /// Parse a FileExtentItem payload given the key (objectid, offset).
    ///
    /// Returns `None` for INLINE extents (no on-disk data to scrub).
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

    /// The logical address of the first byte of this extent's data on disk.
    pub fn disk_start(&self) -> u64 {
        self.disk_bytenr + self.offset
    }
}

/// EXTENT_ITEM flags (subset).  `EXTENT_FLAG_NODATASUM` marks an extent
/// written without a checksum (intentional `nodatasum`/`nocow`), which can
/// never be verified and must not be reported as corruption.
///
/// Values mirror the kernel's `extent-tree.h` (`btrfs_extent_item::flags`):
///   EXTENT_FLAG_DATA      = 1 << 0   (set on every normal data extent)
///   EXTENT_FLAG_TREE_BLOCK= 1 << 1
///   EXTENT_FLAG_NODATASUM = 1 << 3   <-- the one we care about
pub mod extent_flag {
    pub const NODATASUM: u64 = 1 << 3;
}

/// Result of re-confirming a csum mismatch against the **live** EXTENT_TREE
/// and CSUM_TREE (see [`crate::btrfs::open::live_data_tree_roots`]).
///
/// The mismatch filter only downgrades a mismatch to "stale" when the live
/// trees agree the snapshot we scrubbed was out of date — i.e. the sector is
/// no longer owned by a live extent, or the live csum differs from what we
/// read (the extent was rewritten under us).  Anything else is treated as a
/// genuine mismatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reconfirm {
    /// The live trees confirm this is real corruption: the sector is still
    /// owned by a live extent AND the live csum matches the (bad) data we
    /// read.  Report it.
    Corruption,
    /// Benign churn: the sector is no longer owned by a live extent (freed /
    /// orphaned csum entry), or the extent was written `nodatasum`, or the
    /// live csum differs from what we read (extent rewritten).  Do NOT report
    /// as corruption.
    Stale,
    /// The metadata needed to re-confirm *this specific sector* could not be
    /// read (the EXTENT_TREE/CSUM_TREE node covering its logical address had
    /// no good mirror copy).  This is **per-sector**, not global: a metadata
    /// error elsewhere in the filesystem does NOT make this variant — only a
    /// metadata error that actually blocks *this sector's* reconfirmation
    /// does.  The caller should treat it as "cannot verify, skip the write"
    /// for just this candidate, never as a reason to block unrelated writes.
    Unverifiable,
}

/// Re-confirm a data-sector csum mismatch against the live EXTENT_TREE and
/// CSUM_TREE.
///
/// `logical` is the sector's logical address; `stored` is the csum our
/// *frozen snapshot* (taken at `open()`) expected here.  `extent_root` /
/// `csum_root` are the *current* tree roots from
/// [`crate::btrfs::open::live_data_tree_roots`]; `hash_len` is the csum width
/// from the filesystem strategy.
///
/// The decision compares the **live** csum against the **snapshot** csum
/// (`stored`), NOT against the freshly-computed `actual` (the csum of the
/// bytes we read).  Rationale: if the live filesystem still expects exactly
/// `stored` at this logical address, then the data we read disagreeing with
/// `stored` is genuine corruption.  If the live csum *differs* from `stored`,
/// the extent was rewritten since our snapshot — our read was stale and the
/// live data is fine, so it's benign churn.  Comparing against `actual`
/// would be wrong: for a static (unmounted) corruption the live csum equals
/// `stored` but differs from `actual`, and that difference is precisely the
/// corruption signal, not staleness.
///
/// Logic (both trees must agree it's stale before we downgrade):
///   1. EXTENT_TREE lookup at `logical`:
///      - no covering EXTENT_ITEM        -> Stale (freed / orphaned)
///      - covering item has NODATASUM    -> Stale (intentionally uncsummed)
///      - covering item (normal)         -> continue to csum check
///      - tree unreadable for THIS addr  -> Unverifiable (skip write for this sector only)
///   2. CSUM_TREE lookup at `logical` (live):
///      - no entry                       -> Stale (consistent with freed)
///      - entry == `stored`              -> Corruption (live tree still expects `stored`; data is bad)
///      - entry != `stored`              -> Stale (extent rewritten under us)
///      - tree unreadable for THIS addr  -> Unverifiable
///
/// Note the gate is **per-sector**: an `Unreadable` here means the metadata
/// node covering *this logical address* had no good copy.  A metadata error
/// elsewhere in the filesystem does NOT produce `Unverifiable` for sectors
/// whose own metadata read fine — so we never block an unrelated write just
/// because some other part of the tree was unreadable.
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
    // 1. EXTENT_TREE liveness / nodatasum check.
    match extent_covers(reader, chunk_map, extent_root, logical) {
        ExtentLive::Hole => return Reconfirm::Stale,
        ExtentLive::NoDataSum => return Reconfirm::Stale,
        ExtentLive::Live => {}
        // The EXTENT_TREE node covering THIS sector was unreadable -> we
        // cannot confirm liveness, so skip the write for this sector only.
        ExtentLive::Unreadable => return Reconfirm::Unverifiable,
    }

    // 2. CSUM_TREE check: does the live expected csum still equal the
    //    snapshot's stored csum?  If yes -> real corruption; if no -> the
    //    extent was rewritten since our snapshot (benign churn).
    match csum_at(reader, chunk_map, csum_root, logical, hash_len, sector_size) {
        CsumLive::None => Reconfirm::Stale,
        // The CSUM_TREE node covering THIS sector was unreadable -> skip the
        // write for this sector only.
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

/// Whether `logical` is covered by a live data extent in `extent_root`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExtentLive {
    /// A covering EXTENT_ITEM exists (normal, csummed data extent).
    Live,
    /// No EXTENT_ITEM covers `logical` — the address is unallocated / freed.
    Hole,
    /// A covering EXTENT_ITEM carries `EXTENT_FLAG_NODATASUM`.
    NoDataSum,
    /// The extent tree could not be read; treat as "don't know".
    Unreadable,
}

/// Find the EXTENT_ITEM covering `logical` (if any) via a B-tree descent of
/// `extent_root`, reusing `read_node` (DUP cross-check + fsid/owner
/// validation) at each level.  EXTENT_ITEM keys are
/// `(start, EXTENT_ITEM, 0)`; we locate the greatest `start <= logical` and
/// test `start + num_bytes > logical`.
fn extent_covers(
    reader: &mut crate::btrfs::reader::FsReader,
    chunk_map: &crate::btrfs::chunk::ChunkMap,
    extent_root: u64,
    logical: u64,
) -> ExtentLive {
    // Descend: at each internal node, follow the child pointer with the
    // greatest key <= (logical, EXTENT_ITEM, u64::MAX).  EXTENT_ITEM keys
    // are (start, EXTENT_ITEM, num_bytes), so using offset = u64::MAX makes
    // any EXTENT_ITEM with start <= logical sort at or before the target —
    // we then pick the greatest such key and test coverage.  (Using offset 0
    // would wrongly exclude every real extent, whose offset is its length.)
    let target =
        crate::btrfs::key::Key::new(logical, crate::btrfs::key::key_type::EXTENT_ITEM, u64::MAX);
    let mut node_logical = extent_root;
    // Guard against pathological loops.
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
                // Find the child whose key is the greatest <= target.
                // Internal `ptrs` are ascending-key-sorted, so binary
                // search for the first one whose key is > target instead
                // of a linear scan over every child pointer.
                let count = internal
                    .ptrs
                    .partition_point(|ptr| key_leq(&ptr.key, &target));
                match count {
                    0 => return ExtentLive::Hole,
                    _ => node_logical = internal.ptrs[count - 1].blockptr,
                }
            }
            crate::btrfs::node::Node::Leaf(leaf) => {
                // Find the greatest EXTENT_ITEM key <= target and test
                // coverage.  Leaf slots are stored in ascending key order,
                // so binary-search (`partition_point`) for the first slot
                // whose key is > target — everything before that index has
                // key <= target — instead of the previous linear scan from
                // the start of the leaf, which was O(leaf size) per lookup.
                // Other item types (e.g. EXTENT_DATA_REF) can share the
                // same objectid and sort adjacent to an EXTENT_ITEM, so
                // after landing at `count - 1` we still walk backward to
                // the nearest EXTENT_ITEM slot — but that backward walk is
                // bounded by how many non-EXTENT_ITEM entries share this
                // one objectid, not by the whole leaf.  For a regular
                // EXTENT_ITEM (type 168) the key.offset IS the extent
                // length, so coverage is [objectid, objectid+offset).
                let count = leaf
                    .slots
                    .partition_point(|slot| key_leq(&slot.key, &target));
                let mut covering: Option<(usize, u64, u64)> = None; // (slot_idx, start, len)
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

/// Whether `logical` has a csum entry in the live `csum_root`.
#[derive(Debug, Clone)]
enum CsumLive {
    /// A csum entry exists; carries the live stored csum bytes.
    Some(Vec<u8>),
    /// No csum entry at `logical` (address unallocated / freed).
    None,
    /// The csum tree could not be read; treat as "don't know".
    Unreadable,
}

/// Look up the csum stored in the live `csum_root` for `logical`.  CSUM_TREE
/// items are `(EXTENT_CSUM_OBJECTID, EXTENT_CSUM, run_start)` with a packed
/// array of `hash_len`-byte csums, one per sector.  We locate the run
/// covering `logical` and slice the matching sector's csum.
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
                // Binary search: `ptrs` is ascending-key-sorted.
                let count = internal
                    .ptrs
                    .partition_point(|ptr| key_leq(&ptr.key, &target));
                match count {
                    0 => return CsumLive::None,
                    _ => node_logical = internal.ptrs[count - 1].blockptr,
                }
            }
            crate::btrfs::node::Node::Leaf(leaf) => {
                // Find the EXTENT_CSUM run whose [start, start+len) covers
                // `sector`.  Runs are keyed by their start logical address,
                // which lives in `key.offset` (the objectid is always
                // EXTENT_CSUM_OBJECTID = -10).  Every slot in a CSUM_TREE
                // leaf is uniformly `(EXTENT_CSUM_OBJECTID, EXTENT_CSUM, *)`
                // (unlike EXTENT_TREE leaves, no other item type shares this
                // objectid), so a plain binary search on `key.offset` lands
                // exactly on the covering run with no type filter needed —
                // no linear leaf scan required.
                let count = leaf
                    .slots
                    .partition_point(|slot| key_leq(&slot.key, &target));
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

/// `true` iff `a <= b` by (objectid, type, offset) lexicographic order —
/// btrfs's key comparison order.
fn key_leq(a: &crate::btrfs::key::Key, b: &crate::btrfs::key::Key) -> bool {
    (a.objectid, a.ty as u32, a.offset) <= (b.objectid, b.ty as u32, b.offset)
}

/// Whether the EXTENT_ITEM at `leaf[slot_idx]` was written `nodatasum`.
/// Flags live at offset 16 of `btrfs_extent_item`
/// (refs@0(8), generation@8(8), flags@16(8)).
fn extent_is_nodatasum(leaf: &crate::btrfs::node::Leaf, slot_idx: usize) -> bool {
    let data = leaf.item_data(slot_idx);
    if data.len() < 24 {
        return false;
    }
    let flags = u64::from_le_bytes(data[16..24].try_into().unwrap());
    flags & extent_flag::NODATASUM != 0
}
