//! Dev-extent parsing: enumerates a device's extents from the dev tree,
//! mapping physical device ranges to the logical chunks they back.

use std::io;

use super::chunk::ChunkMap;
use super::key::key_type;
use super::reader::FsReader;
use super::tree::walk_leaves;
use super::util::le_u64;

/// A dev extent: a contiguous run of physical space on one device backing
/// part of a chunk. The physical address of logical address L is
/// phys_start + (L - chunk_offset).
#[derive(Debug, Clone, Copy)]
pub struct DevExtent {
    pub devid: u64,
    pub phys_start: u64,
    pub length: u64,
    pub chunk_offset: u64,
}

/// Collects every DEV_EXTENT item for `devid` by walking the dev tree.
/// DEV_EXTENT keys are (devid, DEV_EXTENT_KEY, physical start). The metadata
/// error counters accumulate walk problems (header errors, mirror mismatches,
/// read errors).
pub fn build_dev_extents(
    reader: &mut FsReader,
    chunk_map: &ChunkMap,
    dev_tree_root: u64,
    devid: u64,
    metadata_header_errors: &mut u64,
    metadata_mirror_mismatches: &mut u64,
    metadata_read_errors: &mut u64,
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

                // The dev tree holds extents for every device; keep this one's.
                if slot.key.objectid != devid {
                    continue;
                }
                let data = leaf.item_data(i);

                if data.len() < 32 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("DEV_EXTENT item too short: {} bytes", data.len()),
                    ));
                }
                // DEV_EXTENT payload: chunk_offset at byte 16, length at byte
                // 24; the physical start comes from the key's offset.
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
        // All copies of the node failed the header checksum.
        |_logical| *metadata_header_errors += 1,
        // Stale (generation-mismatched) node: skipped, not counted.
        |_logical| {},
        // Corrupt copy with a good sibling: recovered from the mirror and
        // counted as a mirror mismatch.
        |_logical| *metadata_mirror_mismatches += 1,
        // The node read failed (EIO).
        |_logical| *metadata_read_errors += 1,
    )?;
    Ok(out)
}
