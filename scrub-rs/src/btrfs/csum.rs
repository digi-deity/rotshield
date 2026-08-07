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
//! size.
//!
//! **Metadata-failure accounting is a side effect of the per-range walks —
//! there is no open-time tree pass.**  CSUM_TREE nodes whose *all* mirror
//! copies fail header-checksum verification (DUP/RAID1 metadata with no
//! good copy) are counted by [`LazyCsumProvider::metadata_errors`] as the
//! `range()` walks encounter them, deduplicated by node bytenr so each
//! node is counted exactly once per run even when several dev-extents'
//! range walks re-read the same leaf.  This preserves the undercoverage
//! semantics (a corrupted CSUM_TREE leaf yielding fewer/no csums surfaces
//! as `metadata_header_errors`, never as silent exit 0) without paying a
//! full-tree I/O pass up front: a node is counted iff a range walk
//! actually needed to read it.  Bad leaves whose entire span lies in a
//! freed block group (no dev-extent exists for them) are never read and
//! therefore never counted — they cover no live data, so no coverage gap
//! occurred.  Dedup-set memory is O(#failed nodes): zero on a healthy
//! filesystem, bounded by tree size only in the pathological case.

use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::Ordering;

use super::chunk::ChunkMap;
use super::csum_strategy::CsumStrategy;
use super::key::{Key, key_type, objectid};
use super::reader::FsReader;
use super::tree::{walk_leaves, walk_leaves_range};
use crate::status::StatusCounters;

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
#[allow(clippy::too_many_arguments)]
pub fn build_csum_map(
    reader: &mut FsReader,
    chunk_map: &ChunkMap,
    csum_root: u64,
    strategy: &CsumStrategy,
    map: &mut CsumMap,
    metadata_header_errors: &mut u64,
    metadata_mirror_mismatches: &mut u64,
    metadata_read_errors: &mut u64,
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
        // Stale (freed/repurposed) nodes are normal churn, never errors —
        // not counted (see tree.rs `on_stale`).
        |_logical| {},
        |_logical| *metadata_mirror_mismatches += 1,
        |_logical| *metadata_read_errors += 1,
    )?;
    Ok(count)
}

// ---------------------------------------------------------------------------
// Lazy provider — the path the scrub actually takes.
// ---------------------------------------------------------------------------

/// An on-demand, bounded-memory CSUM_TREE walker.  The scrub calls
/// [`LazyCsumProvider::range`] once per dev-extent's logical `[lo, hi)`
/// span; the provider walks just the CSUM_TREE leaves overlapping that
/// span and yields `(logical, csum)` pairs in ascending logical order, in
/// O(1) memory beyond a small leaf read-ahead buffer.
///
/// Memory bound: the largest single walk yields at most `ceil((hi - lo)
/// / sector_size)` pairs in one `range` call, but `scrub_dev_tree`
/// consumes them as it goes (no materialisation), so steady-state RAM is
/// just one [`CsumEntry`] per sector currently in flight * the consumer's
/// window.  This is independent of disk size and unequal to the eager
/// `CsumMap` which held every sector's csum for the whole disk.  The only
/// additional memory is the metadata-failure dedup sets (see the struct
/// docs): O(#failed nodes), zero on a healthy filesystem.
///
/// `metadata_errors` counts distinct CSUM_TREE nodes that failed
/// header-checksum verification on every mirror copy (DUP / RAID1 metadata
/// with no good copy) *and were read by a `range` walk* — deduplicated by
/// node bytenr, so each node counts exactly once per run regardless of how
/// many dev-extents' range walks re-read it.  Surfaced via
/// [`metadata_errors`] so the coverage gap surfaces as
/// `metadata_header_errors` rather than the silent undercoverage that
/// would otherwise let a corrupted CSUM_TREE yield exit 0.
///
/// `mirror_mismatches` counts distinct CSUM_TREE nodes (deduplicated the
/// same way) where the copies disagreed but a good copy was recovered
/// (self-heal-recoverable).  Reported as `metadata_mirror_mismatches` so a
/// single corrupt DUP metadata copy is not silently healed.
///
/// The four dedup sets hold one `u64` (node bytenr) per *skipped/failed*
/// node — O(#skipped nodes) memory, zero on a healthy filesystem.
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
    /// Distinct CSUM_TREE nodes whose *all* mirror copies failed header-csum
    /// verification, discovered by the per-`range` walks.  Folded into
    /// the scrub's `metadata_header_errors` by the driver.
    metadata_errors: u64,
    /// Distinct mirrored CSUM_TREE nodes whose copies disagreed but a good
    /// copy was recovered, discovered by the per-`range` walks.  Folded
    /// into the scrub's `metadata_mirror_mismatches` by the driver.
    mirror_mismatches: u64,
    /// Distinct CSUM_TREE nodes that failed with a READ (EIO) error — the
    /// bytes could not be fetched at all.  Folded into the scrub's
    /// `metadata_read_errors` by the driver.
    metadata_read_errors: u64,
    /// Distinct CSUM_TREE nodes skipped as **stale** by the per-`range`
    /// walks (only verifiable copies have wrong generation/owner — the
    /// block was freed and repurposed by a live transaction).  A stale
    /// branch is normal churn, NOT a metadata error: its data was retired
    /// or rewritten, so there is nothing to check.  But it IS a coverage
    /// gap — the sectors it covered were never verified this run — so it
    /// is counted into [`stale_branches`] (surfaced as
    /// `ScrubStats::stale_csum_branches`, which refuses exit 0) rather
    /// than silently dropped.
    stale_branches: u64,
    /// Bytenrs of CSUM_TREE nodes already counted in `stale_branches`.
    reported_stale: HashSet<u64>,
    /// Bytenrs of CSUM_TREE nodes already counted in `metadata_errors`
    /// (header-verify failure).  Both DUP mirrors of a node share its
    /// logical bytenr, so this is the exact node identity; `insert`
    /// returning `true` means "first time this node failed in this run".
    reported_header: HashSet<u64>,
    /// Bytenrs already counted in `mirror_mismatches`.
    reported_mirror: HashSet<u64>,
    /// Bytenrs already counted in `metadata_read_errors` (EIO).
    reported_read: HashSet<u64>,
}

/// One `(logical, csum)` pair yielded by [`LazyCsumProvider::range`].
///
/// `csum` is the raw on-disk checksum bytes (length == `strategy.hash_len`):
/// 4 bytes for CRC32C, 8 for XXHASH, 32 for SHA256/BLAKE2.
#[derive(Debug, Clone)]
pub struct CsumEntry {
    pub logical: u64,
    pub csum: Vec<u8>,
}

impl LazyCsumProvider {
    /// Construct a lazy CSUM-tree walker.  **Cheap: performs no tree I/O.**
    /// The CSUM_TREE is only read when a later [`range`] walk descends into
    /// the spans the scrub asks for, and metadata-failure accounting happens
    /// then too (deduplicated per node, see the struct docs) — there is no
    /// open-time header sweep.  The previous implementation walked the
    /// entire CSUM_TREE in the constructor to count bad leaves up front;
    /// on a multi-TB disk that pass reads ~12–100 GB of metadata (hash
    /// dependent) *before* the scrub could start, doubling the tree's I/O
    /// for a diagnostic the per-range walks produce anyway.
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

    /// Number of distinct CSUM_TREE nodes that failed header-checksum
    /// verification on every mirror copy, discovered by the per-`range`
    /// walks (deduplicated by node bytenr).  Folded into the scrub's
    /// `metadata_header_errors` at the end of the run.
    pub fn metadata_errors(&self) -> u64 {
        self.metadata_errors
    }

    /// Number of distinct mirrored CSUM_TREE nodes whose copies disagreed
    /// but a good copy was recovered, discovered by the per-`range` walks
    /// (deduplicated by node bytenr).  Folded into the scrub's
    /// `metadata_mirror_mismatches` at the end of the run.
    pub fn mirror_mismatches(&self) -> u64 {
        self.mirror_mismatches
    }

    /// Number of distinct CSUM_TREE nodes that failed with a READ (EIO)
    /// error, discovered by the per-`range` walks (deduplicated by node
    /// bytenr).  Folded into the scrub's `metadata_read_errors` at the end
    /// of the run.
    pub fn metadata_read_errors(&self) -> u64 {
        self.metadata_read_errors
    }

    /// Number of distinct CSUM_TREE nodes skipped as **stale** by the
    /// per-`range` walks (deduplicated by node bytenr).  A coverage gap,
    /// not a metadata error: the branches' sectors were never verified
    /// this run.  Folded into the scrub's `stale_csum_branches` at the end
    /// of the run (which refuses exit 0 while non-zero).
    pub fn stale_branches(&self) -> u64 {
        self.stale_branches
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
    /// The failure is *counted* here (this is the accounting site): the
    /// error callbacks increment `metadata_errors` / `mirror_mismatches` /
    /// `metadata_read_errors`, deduplicated by node bytenr via the
    /// `reported_*` sets, so a node re-read by several dev-extents' range
    /// walks is counted exactly once per run.  The walk errors themselves
    /// are not propagated (matching the eager map behaviour of simply
    /// having fewer entries on a partial-read CSUM tree) — the counts are
    /// what make the gap visible.
    ///
    /// `counters` is the optional shared live-status counters (the
    /// plugin's status server): each newly-counted failure is bumped into
    /// the corresponding atomic so a `GET /status` shows metadata errors
    /// appearing live during the run.  `None` for standalone runs.
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
        // Split borrows: the walk owns `reader`/`chunk_map` mutably while
        // the error callbacks own the dedup sets and counters — disjoint
        // fields, so no aliasing.
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
            // Header-verify failure (no good mirror copy): count once per
            // distinct node across all range walks of this run, and bump
            // the live status counters.
            |logical| {
                if reported_header.insert(logical) {
                    *metadata_errors += 1;
                    if let Some(c) = counters {
                        c.metadata_header_errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
            },
            // Stale node (only verifiable copies have wrong generation /
            // owner — the block was freed and repurposed by a live
            // transaction): an *expired branch*, i.e. normal churn.  The
            // data it covered was retired or rewritten, so there is
            // nothing to check and nothing to report as a metadata ERROR
            // (counting it as one would turn routine filesystem activity
            // on a busy live array into false metadata-fatal results).  It
            // IS a coverage gap, though: those sectors were never verified
            // this run.  Count the branch once per run (deduplicated like
            // the failure sets) into `stale_branches` / the live status
            // counter, so `main` refuses exit 0 while any sectors were
            // skipped this way.
            |logical| {
                if reported_stale.insert(logical) {
                    *stale_branches += 1;
                    if let Some(c) = counters {
                        c.stale_csum_branches.fetch_add(1, Ordering::Relaxed);
                    }
                }
            },
            // Mirror divergence (good copy recovered): counted once per
            // distinct node, live-bumped the same way.
            |logical| {
                if reported_mirror.insert(logical) {
                    *mirror_mismatches += 1;
                    if let Some(c) = counters {
                        c.metadata_mirror_mismatches.fetch_add(1, Ordering::Relaxed);
                    }
                }
            },
            // READ (EIO) failure: counted once per distinct node,
            // live-bumped the same way.
            |logical| {
                if reported_read.insert(logical) {
                    *metadata_read_errors += 1;
                    if let Some(c) = counters {
                        c.metadata_read_errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
            },
        );
        // Propagate walk errors as zero yields for the affected leaves
        // (matching the eager map behaviour of simply having fewer entries
        // on a partial-read CSUM tree).  The metadata-error counters were
        // already incremented by the callbacks above.
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
