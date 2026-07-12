//! CSUM tree walking — builds a map of logical-sector → stored checksum.
//!
//! EXTENT_CSUM items (key type 128) have:
//!   key.objectid = -10 (BTRFS_EXTENT_CSUM_OBJECTID)
//!   key.offset   = logical address of the first covered sector
//!   data         = packed array of checksum values, one per data-sector,
//!                  spanning `data.len() / hash_len` sectors starting at
//!                  `key.offset`.  The checksum width (`hash_len`) and the
//!                  sector size both come from the filesystem's csum
//!                  strategy (see `csum_strategy.rs`) — btrfs is not limited
//!                  to 4-byte CRC32C over 4096-byte sectors.

use std::collections::BTreeMap;

use super::chunk::ChunkMap;
use super::csum_strategy::CsumStrategy;
use super::key::key_type;
use super::reader::FsReader;
use super::tree::walk_leaves;

/// Map from sector-aligned logical address → stored checksum bytes.
///
/// The value is the raw on-disk checksum (length == `strategy.hash_len`):
/// 4 bytes for CRC32C, 8 for XXHASH, 32 for SHA256/BLAKE2.  Carrying the
/// bytes (rather than a fixed `u32`) lets every btrfs csum profile fit.
///
/// Backed by a `BTreeMap` (not a `HashMap`) so the physical-order scrub can
/// issue ordered **range** queries (`CsumMap::range(lo..hi)`) over a chunk's
/// logical span — the csum entries are already sorted by logical address,
/// which is exactly the ordering the dev-tree-driven walk needs.
pub type CsumMap = BTreeMap<u64, Vec<u8>>;

/// Walk the CSUM tree rooted at `csum_root` and populate `map`.
///
/// `strategy` supplies the checksum width (`hash_len`) and the sector size
/// used to stride the packed csum array and to compute each sector's
/// logical address.  Returns the number of checksum entries inserted.
pub fn build_csum_map(
    reader: &mut FsReader,
    chunk_map: &ChunkMap,
    csum_root: u64,
    strategy: &CsumStrategy,
    map: &mut CsumMap,
) -> std::io::Result<usize> {
    let hash_len = strategy.hash_len;
    let sector_size = strategy.sector_size;
    let mut count = 0usize;
    walk_leaves(
        reader,
        chunk_map,
        csum_root,
        |_r, leaf, _logical| {
        for i in 0..leaf.slots.len() {
            let slot = leaf.slots[i];
            if slot.key.ty != key_type::EXTENT_CSUM {
                continue;
            }
            let data = leaf.item_data(i);
            // The csum array is packed `hash_len`-byte entries; any trailing
            // partial entry (shouldn't happen on a well-formed fs) is ignored.
            let n = data.len() / hash_len;
            for s in 0..n {
                let start = s * hash_len;
                let csum = data[start..start + hash_len].to_vec();
                let logical = slot.key.offset + (s as u64) * sector_size;
                map.insert(logical, csum);
                count += 1;
            }
        }
        Ok(())
    },
        // The csum tree walk only needs the csum entries; metadata-header
        // errors are surfaced by the scrub's own tree walks, not here.
        |_logical| {},
        // Mirror-divergence reporting is not needed for csum-map building.
        |_logical| {},
    )?;
    Ok(count)
}