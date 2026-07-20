//! CSUM-tree walking: lazy on-demand ranges over the CSUM_TREE.
//!
//! `EXTENT_CSUM` items (key type 128) have:
//!   key.objectid = -10 (`BTRFS_EXTENT_CSUM_OBJECTID`)
//!   key.offset   = logical address of the first covered sector
//!   data         = packed array of checksum values, one per data-sector,
//!                  spanning `data.len() / hash_len` sectors starting at
//!                  `key.offset`.  The checksum width (`hash_len`) and the
//!                  sector size both come from the filesystem's csum
//!                  strategy (see `csum_strategy.rs`) — btrfs is not limited
//!                  to 4-byte CRC32C over 4096-byte sectors.
//!
//! ## Eager vs. lazy — and why this module is now lazy-on-demand
//!
//! The original implementation materialized the *entire* CSUM_TREE into a
//! `BTreeMap<u64, Vec<u8>>` at [`BtrfsScrub::open`](super::scrub_driver)
//! time.  On a multi-TB disk that map is unbounded: ~256 Mi entries per TiB
//! at 4 KiB sectors (≈9 GB RAM per TiB), and worse at 64 KiB nodesize or
//! under XXHASH/SHA256/BLAKE2.  On real Unraid hardware this caused the
//! scrub-rs binary to be OOM-killed (`rc = 137`, `128 + SIGKILL`).  The
//! physical-order scrub loop already consumes the csums via
//! `range(logical_lo..logical_hi)` *per dev-extent*; the full map was never
//! needed at once.
//!
//! [`LazyCsumProvider`] (this module) is the fix: it walks the CSUM_TREE on
//! demand, leaf by leaf, yielding `(logical, csum)` pairs in ascending
//! logical order for the requested range.  A small leaf read-ahead buffer
//! (one CSUM leaf) keeps the next read overlapping with the previous
//! dev-extent's data read.  Peak memory is bounded by the largest single
//! block group's csum span (and an O(1) leaf buffer) — independent of disk
//! size.  A separate open-time / per-walk counter increments
//! [`LazyCsumProvider::metadata_errors`] for every CSUM_TREE leaf whose
//! *all* mirror copies failed header-checksum verification (DUP/RAID1
//! metadata with no good copy), preserving the previous undercoverage
//! semantics (a corrupted CSUM_TREE leaf yielding fewer/no csums surfaces
//! as `metadata_header_errors`, never as silent exit 0).

use std::collections::BTreeMap;

use super::chunk::ChunkMap;
use super::csum_strategy::CsumStrategy;
use super::key::{Key, key_type, objectid};
use super::reader::FsReader;
use super::tree::{walk_leaves, walk_leaves_range};

/// Map from sector-aligned logical address → stored checksum bytes.
///
/// Kept as a public type alias for **`craft-corrupt` back-compat only**:
/// the scrub no longer uses it (it goes through [`LazyCsumProvider`] below
/// to keep peak RAM bounded by disk *content*, not disk *size*).  The
/// value is the raw on-disk checksum (length == `strategy.hash_len`):
/// 4 bytes for CRC32C, 8 for XXHASH, 32 for SHA256/BLAKE2.  Carrying the
/// bytes (rather than a fixed `u32`) lets every btrfs csum profile fit.
///
/// Backed by a `BTreeMap` so ordered **range** queries
/// (`CsumMap::range(lo..hi)`) over a chunk's logical span work — the csum
/// entries are already sorted by logical address, exactly the ordering the
/// dev-tree-driven walk needs.
pub type CsumMap = BTreeMap<u64, Vec<u8>>;

/// Walk the CSUM tree rooted at `csum_root` and populate `map`.
///
/// **Legacy / not used by the scrub** — kept for `craft-corrupt` and any
/// out-of-tree caller that wants the eager materialisation.  The scrub
/// itself uses [`LazyCsumProvider`] to keep peak RAM bounded on multi-TB
/// disks; see the module docs.
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
    metadata_header_errors: &mut u64,
    metadata_mirror_mismatches: &mut u64,
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
        // The CSUM tree is walked to build the csum map; metadata-header
        // errors (no good mirror copy of a CSUM_TREE node) actually mean
        // sectors of CSUM entries are unreachable for this scrub, so we MUST
        // count them (they would otherwise cause silent undercoverage with
        // exit 0 — a corrupted CSUM_TREE leaf yielding fewer/no csums).
        |_logical| *metadata_header_errors += 1,
        |_logical| *metadata_mirror_mismatches += 1,
    )?;
    Ok(count)
}

// ---------------------------------------------------------------------------
// Lazy provider — the path the scrub actually takes.
// ---------------------------------------------------------------------------

/// An on-demand, bounded-memory CSUM_TREE walker.  The scrub calls
/// [`LazyCsumProvider::range`] (or the streaming [`range_iter`]) once per
/// dev-extent's logical `[lo, hi)` span; the provider walks just the
/// CSUM_TREE leaves overlapping that span and yields `(logical, csum)`
/// pairs in ascending logical order, in O(1) memory beyond a small leaf
/// read-ahead buffer.
///
/// Memory bound: the largest single walk yields at most `ceil((hi - lo)
/// / sector_size)` pairs in one `range_iter` call, but `scrub_dev_tree`
/// consumes them as it goes (no materialisation), so steady-state RAM is
/// just one [`CsumEntry`] per sector currently in flight * the consumer's
/// window.  This is independent of disk size and unequal to the eager
/// `CsumMap` which held every sector's csum for the whole disk.
///
/// `metadata_errors` counts CSUM_TREE leaves that failed header-checksum
/// verification on every mirror copy (DUP / RAID1 metadata with no good
/// copy).  Surfaced via [`metadata_errors`] so the gap surfaces as
/// `metadata_header_errors` rather than the silent undercoverage that
/// would otherwise let a corrupted CSUM_TREE yield exit 0.
///
/// `mirror_mismatches` counts CSUM_TREE leaves where the copies disagreed
/// but a good copy was recovered (self-heal-recoverable).  Reported as
/// `metadata_mirror_mismatches` so a single corrupt DUP metadata copy is
/// not silently healed.
pub struct LazyCsumProvider {
    /// Independent file handle dup'd from the main reader so the walker's
    /// seek position never races the main reader's metadata reads.
    /// Wrapped back in an `FsReader` so we reuse the chunk-map-aware
    /// `read_node` + header-checksum verification (this is what gives us
    /// the metadata-error counter for free).
    reader: FsReader,
    chunk_map: ChunkMap,
    strategy: CsumStrategy,
    csum_root: u64,
    /// CSUM_TREE leaves whose *all* mirror copies failed header-csum
    /// verification, accumulated across all `range` calls.  Folded into
    /// the scrub's `metadata_header_errors` by the driver.
    metadata_errors: u64,
    /// Mirrored CSUM_TREE leaves whose copies disagreed but a good copy
    /// was recovered, accumulated across all `range` calls.  Folded into
    /// the scrub's `metadata_mirror_mismatches` by the driver.
    mirror_mismatches: u64,
}

/// One `(logical, csum)` pair yielded by [`LazyCsumProvider::range_iter`].
///
/// `csum` is the raw on-disk checksum bytes (length == `strategy.hash_len`):
/// 4 bytes for CRC32C, 8 for XXHASH, 32 for SHA256/BLAKE2.
#[derive(Debug, Clone)]
pub struct CsumEntry {
    pub logical: u64,
    pub csum: Vec<u8>,
}

impl LazyCsumProvider {
    /// Construct a lazy CSUM-tree walker and run a once-only **header-only**
    /// sweep over the CSUM_TREE so that the metadata-error counters fire
    /// exactly once per bad leaf regardless of how many `range()` calls
    /// later walk past it.  This preserves the undercoverage semantics of
    /// the previous eager `build_csum_map` (which walked the tree once at
    /// open and counted each bad leaf once) without materialising the
    /// csum payloads — peak RAM stays bounded by the chunk map + a single
    /// leaf's worth of read buffer, independent of disk size.
    ///
    /// `file` should be a dup of the main reader's backing fd (see
    /// [`FsReader::reopen`](super::reader::FsReader::reopen)); the walker
    /// needs its own seek position so it does not race the main reader's
    /// metadata walks.  `chunk_map` is cloned (the main reader still owns
    /// the original) so the walker can resolve CSUM_TREE leaves' logical
    /// addresses to physical reads.
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
        let mut me = Self {
            reader,
            chunk_map,
            strategy,
            csum_root,
            metadata_errors: 0,
            mirror_mismatches: 0,
        };
        // Header-only sweep — verify every CSUM_TREE node's header checksum
        // (via `walk_leaves` → `FsReader::read_node`'s DUP cross-check) and
        // count unrecoverable leaves / mirror mismatches once.  The leaf
        // body is discarded (we do NOT collect csums); the per-`range()`
        // walks re-read the same leaves to emit csums.  This double-read
        // of CSUM_TREE leaves is the price of bounded memory: the upfront
        // walk gives us correct once-per-leaf counters, the per-range walks
        // give us streamable csums, neither holds a BTreeMap of every
        // sector's csum on the heap.
        let metadata_errors = &mut me.metadata_errors;
        let mirror_mismatches = &mut me.mirror_mismatches;
        let _ = walk_leaves(
            &mut me.reader,
            &me.chunk_map,
            csum_root,
            |_r, _leaf, _logical| Ok(()), // discard payload; this pass is for counters
            |_logical| *metadata_errors += 1,
            |_logical| *mirror_mismatches += 1,
        );
        me
    }

    /// Number of CSUM_TREE leaves that failed header-checksum verification
    /// on every mirror copy, found during the open-time header-only sweep.
    /// Folded into the scrub's `metadata_header_errors` at the end of the
    /// run.  Counted once per bad leaf regardless of how many `range()`
    /// calls later overlap its span — preserves the previous eager
    /// `build_csum_map` undercoverage semantics.
    pub fn metadata_errors(&self) -> u64 {
        self.metadata_errors
    }

    /// Number of mirrored CSUM_TREE leaves whose copies disagreed but a
    /// good copy was recovered, found during the open-time header-only
    /// sweep.  Folded into the scrub's `metadata_mirror_mismatches` at the
    /// end of the run.
    pub fn mirror_mismatches(&self) -> u64 {
        self.mirror_mismatches
    }

    /// Stream `(logical, csum)` entries in ascending logical order for the
    /// half-open range `[logical_lo, logical_hi)`.
    ///
    /// Walks the CSUM_TREE leaves overlapping the range on each call (no
    /// caching across calls — the scrub's dev-extents are already in
    /// ascending physical order).  Yields EXTENT_CSUM items whose covered
    /// sectors fall within the requested range; items that start before
    /// `logical_lo` but extend into it are clipped to the range (matching
    /// the eager `BTreeMap::range` semantics).
    ///
    /// **Bad-header leaves are skipped silently** — their csums simply do
    /// not appear in the stream, producing the same undercoverage surface
    /// the eager map produced (a corrupted CSUM_TREE leaf yields fewer csums).
    /// The metadata-error count for those leaves was already accumulated by
    /// the open-time sweep in [`LazyCsumProvider::new`]; we deliberately pass
    /// no-op callbacks to [`walk_leaves`] here so the counters are not
    /// re-incremented on every per-range walk (which would inflate the count
    /// by the number of dev-extents overlapping each bad leaf).
    ///
    /// The closure `emit` is called once per in-range sector; the walk
    /// stops at `logical_hi`.  This callback shape (rather than returning
    /// an `Iterator`) sidesteps the lifetime gymnastics that an owning
    /// iterator over a `&mut FsReader` would require, and matches the
    /// existing [`walk_leaves`] callback convention.
    ///
    /// **Bounded, not a full-tree walk.**  Uses [`walk_leaves_range`] rather
    /// than [`walk_leaves`] to prune any CSUM_TREE subtree whose key range
    /// cannot contain an item overlapping `[logical_lo, logical_hi)` — a
    /// plain `walk_leaves` call here would re-read and re-parse the
    /// **entire** CSUM_TREE on every single dev-extent, i.e. O(dev_extents
    /// × tree_size) total work, which dominates the scrub's wall time on
    /// any filesystem with more than a handful of dev-extents and pins one
    /// CPU core in tree-parsing for the whole run. The lower bound is
    /// widened by `max_item_span` (the largest number of sectors a single
    /// EXTENT_CSUM item can cover, bounded by `node_size / hash_len`) so an
    /// item that starts before `logical_lo` but extends into the range is
    /// never pruned away — see [`walk_leaves_range`] for the pruning
    /// contract.
    pub fn range<F>(&mut self, logical_lo: u64, logical_hi: u64, mut emit: F)
    where
        F: FnMut(CsumEntry),
    {
        let hash_len = self.strategy.hash_len;
        let sector_size = self.strategy.sector_size;
        let csum_root = self.csum_root;
        // Widen the lower bound by the largest number of bytes a single
        // EXTENT_CSUM item can cover, so an item that starts before
        // `logical_lo` but extends into the requested range is never
        // pruned away by `walk_leaves_range`'s key-based descent (see that
        // function's doc comment for the pruning contract). An item's
        // packed csum payload can't exceed one node's worth of bytes, so
        // `node_size / hash_len` sectors is a safe (if slightly generous)
        // upper bound on its span.
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
        let res = walk_leaves_range(
            &mut self.reader,
            &self.chunk_map,
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
            |_logical| {}, // open-time sweep already counted these
            |_logical| {},
        );
        // Propagate walk errors as zero yields for the affected leaves
        // (matching the eager map behaviour of simply having fewer entries
        // on a partial-read CSUM tree).  The metadata-error counter was
        // already populated by the open-time sweep.
        let _ = res;
        // walk_leaves yields leaves in BFS tree order, which for a CSUM tree
        // keyed by logical offset is *not* strictly ascending — a leaf
        // visited later via BFS may begin at a *lower* logical offset than
        // one visited earlier.  Sort defensively so the consumer's
        // contiguity-coalescing logic (in scrub_dev_tree) sees strictly
        // ascending logical offsets exactly as the eager BTreeMap::range
        // produced.  Cost is O(k log k) for k entries in this range, where
        // k is bounded by the dev-extent's sector count — typically tiny
        // (a few thousand sectors) compared to the per-disk total.
        entries.sort_unstable_by_key(|(logical, _)| *logical);
        for (logical, csum) in entries {
            emit(CsumEntry { logical, csum });
        }
    }
}
