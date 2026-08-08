//! Lazy CSUM_TREE walking: on-demand checksum ranges with bounded memory.

use std::collections::HashSet;
use std::sync::atomic::Ordering;

use super::chunk::ChunkMap;
use super::csum_strategy::CsumStrategy;
use super::key::{Key, key_type, objectid};
use super::reader::FsReader;
use super::tree::walk_leaves_range;
use crate::status::StatusCounters;

/// On-demand CSUM_TREE walker: `range()` streams the checksums covering a
/// logical span in bounded memory, and counts metadata failures it
/// encounters (deduplicated per node).
pub struct LazyCsumProvider {
    reader: FsReader,
    chunk_map: ChunkMap,
    strategy: CsumStrategy,
    csum_root: u64,

    /// Distinct CSUM_TREE nodes whose every mirror copy failed the header checksum.
    metadata_errors: u64,

    /// Distinct mirrored nodes whose copies disagreed (a good copy existed).
    mirror_mismatches: u64,

    /// Distinct nodes that failed with a read (EIO) error.
    metadata_read_errors: u64,

    /// Nodes skipped as stale (freed/repurposed) — a coverage gap, not an error.
    stale_branches: u64,

    /// Bytenrs already counted, per counter, to deduplicate re-read nodes.
    reported_stale: HashSet<u64>,

    reported_header: HashSet<u64>,

    reported_mirror: HashSet<u64>,

    reported_read: HashSet<u64>,
}

/// One (logical address, stored checksum bytes) pair from the CSUM_TREE.
#[derive(Debug, Clone)]
pub struct CsumEntry {
    pub logical: u64,
    pub csum: Vec<u8>,
}

impl LazyCsumProvider {
    /// Build a walker with its own reader (a dup'd fd), so it never races
    /// the main scrub reader's seek position. Cheap: no tree I/O happens here.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        file: std::fs::File,
        node_size: usize,
        base_offset: u64,
        strategy: CsumStrategy,
        devid: u64,
        fsid: [u8; 16],
        chunk_map: ChunkMap,
        csum_root: u64,
    ) -> Self {
        let reader = FsReader::new(file, node_size, base_offset, Some(strategy))
            .with_devid(devid)
            .with_fsid(fsid);
        Self {
            reader,
            chunk_map,
            strategy,
            csum_root,
            metadata_errors: 0,
            mirror_mismatches: 0,
            metadata_read_errors: 0,
            stale_branches: 0,
            reported_stale: HashSet::new(),
            reported_header: HashSet::new(),
            reported_mirror: HashSet::new(),
            reported_read: HashSet::new(),
        }
    }

    pub fn metadata_errors(&self) -> u64 {
        self.metadata_errors
    }

    pub fn mirror_mismatches(&self) -> u64 {
        self.mirror_mismatches
    }

    pub fn metadata_read_errors(&self) -> u64 {
        self.metadata_read_errors
    }

    pub fn stale_branches(&self) -> u64 {
        self.stale_branches
    }

    /// Stream `(logical, csum)` entries in ascending logical order for the
    /// half-open range `[logical_lo, logical_hi)`, counting metadata failures
    /// encountered along the way (deduplicated per node, optionally mirrored
    /// into the shared status `counters`).
    pub fn range<F>(
        &mut self,
        logical_lo: u64,
        logical_hi: u64,
        counters: Option<&StatusCounters>,
        mut emit: F,
    ) where
        F: FnMut(CsumEntry),
    {
        let hash_len = self.strategy.hash_len;
        let sector_size = self.strategy.sector_size;
        let csum_root = self.csum_root;

        // Widen the lower bound by the largest span a single EXTENT_CSUM
        // item can cover, so an item starting before logical_lo but
        // extending into the range is not pruned by the key descent.
        let max_item_span = (self.reader.node_size() as u64 / hash_len as u64) * sector_size;
        let key_lo = Key::new(
            objectid::EXTENT_CSUM_OBJECTID,
            key_type::EXTENT_CSUM,
            logical_lo.saturating_sub(max_item_span),
        );
        let key_hi = Key::new(
            objectid::EXTENT_CSUM_OBJECTID,
            key_type::EXTENT_CSUM,
            logical_hi,
        );
        let mut entries: Vec<(u64, Vec<u8>)> = Vec::new();

        // Split borrows: the walk owns reader/chunk_map while the error
        // callbacks own the dedup sets and counters.
        let Self {
            reader,
            chunk_map,
            reported_header,
            reported_mirror,
            reported_read,
            reported_stale,
            metadata_errors,
            mirror_mismatches,
            metadata_read_errors,
            stale_branches,
            ..
        } = self;
        let res = walk_leaves_range(
            reader,
            chunk_map,
            csum_root,
            key_lo,
            key_hi,
            |_r, leaf, _leaf_logical| {
                for i in 0..leaf.slots.len() {
                    let slot = leaf.slots[i];
                    if slot.key.ty != key_type::EXTENT_CSUM {
                        continue;
                    }
                    let data = leaf.item_data(i);
                    let n = data.len() / hash_len;
                    let item_lo = slot.key.offset;
                    let item_hi = item_lo + (n as u64) * sector_size;
                    if item_hi <= logical_lo || item_lo >= logical_hi {
                        continue;
                    }
                    for s in 0..n {
                        let logical = item_lo + (s as u64) * sector_size;
                        if logical < logical_lo || logical >= logical_hi {
                            continue;
                        }
                        let start = s * hash_len;
                        let csum = data[start..start + hash_len].to_vec();
                        entries.push((logical, csum));
                    }
                }
                Ok(())
            },
            // Header-verify failure: count once per distinct node; bump
            // the live status counter if one is attached.
            |logical| {
                if reported_header.insert(logical) {
                    *metadata_errors += 1;
                    if let Some(c) = counters {
                        c.metadata_header_errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
            },
            // Stale node (freed/repurposed by a live transaction): normal
            // churn, but a coverage gap — counted so the run refuses
            // exit 0, not as a metadata error.
            |logical| {
                if reported_stale.insert(logical) {
                    *stale_branches += 1;
                    if let Some(c) = counters {
                        c.stale_csum_branches.fetch_add(1, Ordering::Relaxed);
                    }
                }
            },
            // Mirror divergence (a good copy was recovered): count once.
            |logical| {
                if reported_mirror.insert(logical) {
                    *mirror_mismatches += 1;
                    if let Some(c) = counters {
                        c.metadata_mirror_mismatches.fetch_add(1, Ordering::Relaxed);
                    }
                }
            },
            // Read (EIO) failure: count once per distinct node.
            |logical| {
                if reported_read.insert(logical) {
                    *metadata_read_errors += 1;
                    if let Some(c) = counters {
                        c.metadata_read_errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
            },
        );

        // Walk errors surface as fewer yielded entries; the counts above
        // already recorded the failures.
        let _ = res;

        // DFS leaf order is not strictly ascending in logical offset; sort
        // so the consumer sees ascending order as the eager map produced.
        entries.sort_unstable_by_key(|(logical, _)| *logical);
        for (logical, csum) in entries {
            emit(CsumEntry { logical, csum });
        }
    }
}
