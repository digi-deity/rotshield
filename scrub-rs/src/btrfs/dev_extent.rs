//! DEV_TREE walking — enumerate a device's dev-extents in ascending
//! physical order.
//!
//! The device tree (`BTRFS_DEV_TREE_OBJECTID = 4`) holds `DEV_EXTENT`
//! items keyed by `(devid, DEV_EXTENT_KEY, physical_offset)`.  Each item
//! records that the byte range `[physical_offset, physical_offset+length)`
//! on device `devid` backs the chunk whose logical space starts at
//! `chunk_offset`.  Because the key sorts by `(devid, offset)`, an
//! in-order leaf walk for a single `devid` yields dev-extents in strictly
//! ascending physical order — a single front-to-back pass over the disk.
//!
//! That physical ordering is exactly what the physical-order scrub
//! ([`crate::btrfs::scrub::scrub_dev_tree`]) wants to drive its reads
//! from, instead of the CSUM tree's logical ordering (which scatters
//! reads all over the disk).  See the plan doc "Physical-Order Scrub via
//! the Device Tree" for the rationale.
//!
//! ## Single-device assumption
//!
//! Every NonRAID slot is its own **single-device** btrfs filesystem, so
//! the only profiles that can occur are `SINGLE` and `DUP` — both linear
//! (a dev-extent is a contiguous sub-range of its chunk's logical space,
//! and `dev_extent.length == chunk.length` always holds; `DUP` simply
//! contributes two equal-length dev-extents on the same disk).  Striped
//! profiles (RAID0/5/6/10) are impossible on one device, so no stripe-unit
//! math and no striped-profile guard are needed here — the linear mapping
//! `logical = chunk_offset + (physical - phys_start)` is always valid.

use std::io;

use super::chunk::ChunkMap;
use super::key::key_type;
use super::reader::FsReader;
use super::tree::walk_leaves;
use super::util::le_u64;

/// A single device extent as enumerated from the DEV_TREE.
///
/// `phys_start`/`length` are byte offsets on the array partition
/// (`/dev/nmd1p1`-space); `chunk_offset` is the logical start of the chunk
/// this extent backs.  The physical→logical mapping for a linear profile
/// is `logical = chunk_offset + (physical - phys_start)`.
#[derive(Debug, Clone, Copy)]
pub struct DevExtent {
    pub devid: u64,
    pub phys_start: u64,
    pub length: u64,
    pub chunk_offset: u64,
}

/// Walk the DEV_TREE rooted at `dev_tree_root` and return every `DEV_EXTENT`
/// item for `devid`, in ascending physical order.
///
/// The tree key already sorts by `(devid, offset)`, so a plain in-order
/// leaf walk gives ascending physical order for free — no extra sort step.
/// Items for other devids (only relevant on multi-device filesystems, which
/// this tool does not target) are skipped.
///
/// `btrfs_dev_extent` value layout: `chunk_tree(8) | chunk_objectid(8) |
/// chunk_offset(8) | length(8) | chunk_tree_uuid(16)`.  We only need
/// `chunk_offset` (offset 16) and `length` (offset 24).
pub fn build_dev_extents(
    reader: &mut FsReader,
    chunk_map: &ChunkMap,
    dev_tree_root: u64,
    devid: u64,
    metadata_header_errors: &mut u64,
    metadata_mirror_mismatches: &mut u64,
) -> io::Result<Vec<DevExtent>> {
    let mut out = Vec::new();
    walk_leaves(
        reader,
        chunk_map,
        dev_tree_root,
        |_r, leaf, _logical| {
            for i in 0..leaf.slots.len() {
                let slot = leaf.slots[i];
                if slot.key.ty != key_type::DEV_EXTENT_KEY {
                    continue;
                }
                // Skip dev-extents belonging to other devices.
                if slot.key.objectid != devid {
                    continue;
                }
                let data = leaf.item_data(i);
                // Minimum: chunk_tree(8)+chunk_objectid(8)+chunk_offset(8)+length(8).
                if data.len() < 32 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("DEV_EXTENT item too short: {} bytes", data.len()),
                    ));
                }
                let chunk_offset = le_u64(data, 16);
                let length = le_u64(data, 24);
                out.push(DevExtent {
                    devid,
                    phys_start: slot.key.offset,
                    length,
                    chunk_offset,
                });
            }
            Ok(())
        },
        // The DEV_TREE drives scrub_dev_tree; a corrupted DEV_TREE leaf
        // means the scrub would silently enumerate fewer/no dev-extents
        // and report 0 mismatches with exit 0. Count it as a metadata
        // header error so the gap surfaces in the summary and exit code.
        |_logical| *metadata_header_errors += 1,
        |_logical| *metadata_mirror_mismatches += 1,
    )?;
    Ok(out)
}
