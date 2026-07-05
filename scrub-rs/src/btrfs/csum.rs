//! CSUM tree walking — builds a map of logical-sector → stored CRC32C.
//!
//! EXTENT_CSUM items (key type 128) have:
//!   key.objectid = -10 (BTRFS_EXTENT_CSUM_OBJECTID)
//!   key.offset   = logical address of the first covered sector
//!   data         = packed array of u32 LE CRC32C values, one per 4096-byte
//!                  sector, spanning `data_size / 4` sectors starting at
//!                  `key.offset`.

use std::collections::HashMap;

use super::chunk::ChunkMap;
use super::key::key_type;
use super::reader::FsReader;
use super::superblock::BTRFS_SECTOR_SIZE;
use super::tree::walk_leaves;

/// Map from sector-aligned logical address → stored CRC32C.
pub type CsumMap = HashMap<u64, u32>;

/// Walk the CSUM tree rooted at `csum_root` and populate `map`.
///
/// Returns the number of checksum entries inserted.
pub fn build_csum_map(
    reader: &mut FsReader,
    chunk_map: &ChunkMap,
    csum_root: u64,
    map: &mut CsumMap,
) -> std::io::Result<usize> {
    let mut count = 0usize;
    walk_leaves(reader, chunk_map, csum_root, |_r, leaf, _logical| {
        for i in 0..leaf.slots.len() {
            let slot = leaf.slots[i];
            if slot.key.ty != key_type::EXTENT_CSUM {
                continue;
            }
            let data = leaf.item_data(i);
            let n = data.len() / 4;
            for s in 0..n {
                let csum = u32::from_le_bytes([
                    data[s * 4],
                    data[s * 4 + 1],
                    data[s * 4 + 2],
                    data[s * 4 + 3],
                ]);
                let logical = slot.key.offset + (s as u64) * BTRFS_SECTOR_SIZE as u64;
                map.insert(logical, csum);
                count += 1;
            }
        }
        Ok(())
    })?;
    Ok(count)
}