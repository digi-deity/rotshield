//! The scrub loop.
//!
//! The single scrub strategy is [`scrub_dev_tree`]: drive reads off the
//! **DEV_TREE** (ascending physical order) rather than the CSUM tree
//! (logical order).  The DEV_TREE enumerates every data dev-extent in
//! strictly ascending physical order, so the scrub is a single
//! front-to-back pass over the disk — turning the full-disk scrub from
//! effectively-random I/O into sequential reads.  The CSUM tree is still
//! consulted (via `csum_map`) for the per-sector expected checksum, so
//! coverage is identical to a CSUM-tree walk (every checksummed data
//! sector, across all subvolumes/snapshots, deduplicated for shared
//! COW/reflink extents) — we just read the bytes in physical order.

use crate::btrfs::chunk::ChunkMap;
use crate::btrfs::csum::CsumMap;
use crate::btrfs::csum_strategy::CsumStrategy;
use crate::btrfs::dev_extent::DevExtent;
use crate::btrfs::key::bg_flag;
use crate::btrfs::reader::FsReader;

/// Result of scrubbing a single sector.
///
/// `devid` and `array_phys` give the on-disk physical location in
/// **array-partition space**: which disk and at what byte offset on that
/// disk's array partition (`/dev/nmd1p1`).  These are filesystem-agnostic
/// — any array recovery layer only needs "which disk, which byte" to do
/// XOR parity reconstruction, no knowledge of btrfs chunks or logical
/// addresses.  See the "Address spaces and I/O paths" doc in
/// `array::mod` for what each space means and how the I/O paths differ.
///
/// `logical`, `inode`, and `file_offset` are kept for logging and for
/// filesystem-specific callers but are not needed by recovery.
///
/// Checksums are carried as raw bytes here (btrfs's on-disk layout — 4
/// bytes for CRC32C, 8 for XXHASH, 32 for SHA256/BLAKE2) so the
/// [`crate::btrfs::BtrfsScrub`] adapter can pack them into
/// `Box<dyn Fn(&[u8]) -> bool>` closures without re-deriving the algorithm
/// at the boundary.  `stored_csum` is `None` for sectors with no CSUM-tree
/// entry; `actual_csum` is always populated (the freshly computed hash).
#[derive(Debug)]
pub struct SectorResult {
    pub logical: u64,
    /// btrfs device ID (== NonRAID slot number for our arrays).
    pub devid: u64,
    /// Physical offset in **array-partition space** (on the array
    /// partition device, before `rdevOffset` is added).  Recovery adds
    /// `rdevOffset` to get raw-rdev space.
    pub array_phys: u64,
    pub inode: u64,
    pub file_offset: u64,
    /// Stored checksum from the CSUM tree, as raw bytes (length ==
    /// `strategy.hash_len`).  `None` if no CSUM entry covers this sector.
    pub stored_csum: Option<Vec<u8>>,
    /// The freshly computed checksum of the on-disk data, as raw bytes.
    pub actual_csum: Vec<u8>,
    pub ok: bool,
}

/// Scrub statistics.
#[derive(Debug, Default)]
pub struct ScrubStats {
    pub sectors_checked: u64,
    pub sectors_ok: u64,
    pub sectors_mismatch: u64,
    pub sectors_no_csum: u64,
    pub sectors_read_error: u64,
    /// Sectors whose stored csum did NOT match the on-disk data, but which
    /// the LIVE EXTENT_TREE + CSUM_TREE show are no longer owned by a live
    /// data extent (orphaned/freed csum entry, `nodatasum` extent, or an
    /// extent rewritten under us since the scrub's frozen snapshot was
    /// taken).  Benign churn, NOT corruption — not counted in
    /// `sectors_mismatch` and does not trigger recovery.  Folded into
    /// [`crate::fs::ScrubStats::sectors_stale`] by the driver.
    pub sectors_stale: u64,
    pub bytes_checked: u64,
    /// Metadata nodes whose *all* mirror copies failed header-checksum
    /// verification (DUP/RAID1 metadata with no good copy).  The data-scrub
    /// loops themselves don't traverse metadata, so this is folded in from
    /// the chunk/root-tree walks in `open.rs` by the driver — see
    /// [`crate::fs::ScrubStats::metadata_header_errors`].
    pub metadata_header_errors: u64,
}

/// Scrub all sectors of all REGULAR extents.
///
/// Calls `on_sector` for each sector that mismatches or has no checksum,
/// so the caller can print/report them.  The callback receives a fully
/// populated `SectorResult` including the on-disk physical location
/// `(devid, phys)` — computed via `chunk_map.lookup` inside the scrub —
/// so callers that want to act on a mismatch (e.g. parity recovery) get a
/// filesystem-agnostic physical address without needing to borrow the
/// chunk map themselves.
///
/// `strategy` carries the checksum algorithm and the data sector size, both
/// taken from the superblock — the scrub no longer assumes CRC32C over
/// fixed 4096-byte sectors.
///
/// The single scrub strategy is [`scrub_dev_tree`]: it drives reads off the
/// DEV_TREE in ascending physical order (a single front-to-back pass over
/// the disk) while still consulting the CSUM tree (via `csum_map`) for the
/// per-sector expected checksum.  Coverage is identical to a CSUM-tree
/// walk — every checksummed data sector, across all subvolumes/snapshots,
/// deduplicated for shared COW/reflink extents — but the bytes are read in
/// physical order for sequential I/O.  The earlier `scrub_extents`
/// (per-inode FS-tree walk) and `scrub_csum_tree` (logical-order CSUM-tree
/// walk) variants were removed; `scrub_dev_tree` is the sole path.
///
/// Scrub every DATA sector by driving reads off the **device tree** instead
/// of the CSUM tree.
///
/// This walks the DEV_TREE's dev-extents for a single `devid`, which are
/// already sorted in strictly ascending **physical** order.  For each
/// dev-extent we resolve its owning chunk via `chunk_map.info()` and, for
/// DATA chunks only, issue ordered `read_physical` calls across the chunk's
/// logical span.  Because every NonRAID slot is a single-device
/// filesystem, the only profiles are `SINGLE` and `DUP` — both linear — so
/// the physical→logical mapping `logical = chunk_offset + (physical -
/// phys_start)` holds and no
/// striped-profile guard is needed (RAID0/5/6/10 cannot occur on one disk).
///
/// `dev_extents` must be sorted by `(devid, phys_start)` — which
/// `dev_extent::build_dev_extents` already guarantees — so the reads
/// proceed in a single front-to-back pass over the disk.
///
/// Differences from [`scrub_csum_tree`] to be aware of when consuming
/// [`ScrubStats`]:
///
/// * **No `sectors_no_csum` for intra-chunk gaps.**  We iterate the csum
///   entries *within* each chunk's logical span, so free space inside an
///   allocated chunk (or inline extents) shows up as gaps with no csum
///   entry.  We deliberately do **not** report those as `sectors_no_csum`
///   — doing so would make every run extremely noisy.  `sectors_no_csum`
///   therefore stays 0 for this path; only `scrub_csum_tree` (which only
///   ever sees entries that exist) populates it.
/// * **Metadata/system chunks are skipped** via the `bg_flag::DATA` filter,
///   matching the existing convention that the data-scrub loops don't
///   traverse metadata.  Their dev-extents are simply not scrubbed here.
#[allow(clippy::too_many_arguments)]
pub fn scrub_dev_tree<F>(
    reader: &mut FsReader,
    chunk_map: &ChunkMap,
    csum_map: &CsumMap,
    dev_extents: &[DevExtent],
    strategy: &CsumStrategy,
    batch: bool,
    mut freeze: Option<&mut crate::freeze::FreezeController>,
    mut on_sector: F,
) -> ScrubStats
where
    F: FnMut(&SectorResult),
{
    let mut stats = ScrubStats::default();
    let sector_size = strategy.sector_size;

    for dext in dev_extents {
        // Resolve the owning chunk.  Every dev-extent must have a matching
        // chunk item; if not, the chunk map is inconsistent with the dev
        // tree and we cannot map this extent — bail loudly rather than
        // silently skipping (which would hide unscrubbed data).
        let chunk = chunk_map
            .info(dext.chunk_offset)
            .expect("dev extent with no matching chunk item");

        // Only scrub DATA chunks here (metadata/system handled elsewhere,
        // and they carry no csum-tree entries anyway).
        if chunk.flags & bg_flag::DATA == 0 {
            continue;
        }

        let logical_lo = dext.chunk_offset;
        let logical_hi = dext.chunk_offset + dext.length;

        // Ordered within [logical_lo, logical_hi) == ordered physically,
        // because the mapping is linear for SINGLE/DUP profiles.
        //
        // Read coalescing: consecutive csum entries that are physically
        // contiguous (`next_logical == sector_logical + sector_size`) are
        // also physically contiguous on disk, so instead of one syscall per
        // sector we accumulate a run and issue a single `read_physical`
        // covering the whole run, then checksum each sector in memory.  This
        // is where most of the throughput on a raw block device actually
        // comes from — turning "N syscalls of 4–16 KiB" into "one syscall
        // of however big the contiguous run is".  A break in contiguity (or
        // the end of the chunk) flushes the pending run first.
        //
        // The batched read buffers the whole run in memory before slicing,
        // so an unbounded run could grow to the size of the largest
        // contiguous data extent (btrfs caps a single file extent at 128
        // MiB, but adjacent extents concatenate the run further, and a
        // fully-allocated chunk is larger still).  `MAX_RUN_SECTORS` caps the
        // run so peak memory stays bounded (~64 MiB at 4 KiB sectors) on
        // modern systems regardless of extent size; a run that hits the cap
        // flushes early even if the next sector is still contiguous.  The
        // cap is far larger than typical on-disk contiguity, so it rarely
        // triggers and costs at most a handful of extra syscalls per scrub.
        const MAX_RUN_SECTORS: usize = 16384; // 16384 * 4096 = 64 MiB at default sector size
        let mut run: Vec<(u64, &Vec<u8>)> = Vec::new();
        let mut flush = |reader: &mut FsReader,
                         chunk_map: &ChunkMap,
                         strategy: &CsumStrategy,
                         batch: bool,
                         freeze: &mut Option<&mut crate::freeze::FreezeController>,
                         run: &[(u64, &Vec<u8>)],
                         stats: &mut ScrubStats| {
            if run.is_empty() {
                return;
            }
            let run_phys = dext.phys_start + (run[0].0 - dext.chunk_offset);
            let run_len = run.len() * sector_size as usize;
            match reader.read_physical(dext.devid, run_phys, run_len) {
                Ok(buf) => {
                    for (i, (sector_logical, stored)) in run.iter().enumerate() {
                        let start = i * sector_size as usize;
                        let end = start + sector_size as usize;
                        let data = &buf[start..end];
                        let actual = strategy.compute(data);
                        stats.sectors_checked += 1;
                        stats.bytes_checked += sector_size;
                        if actual == **stored {
                            stats.sectors_ok += 1;
                        } else if batch {
                            // Batched recovery mode: emit the raw mismatch as
                            // a candidate and let the (separate) recovery
                            // sink re-confirm + write it later, under a single
                            // batched freeze.  We do NOT re-confirm or count
                            // it here — the sink owns mismatch/stale
                            // accounting for the batch.  `sectors_mismatch`
                            // is left to the sink so the count stays honest.
                            let phys = dext.phys_start + (*sector_logical - dext.chunk_offset);
                            on_sector(&SectorResult {
                                logical: *sector_logical,
                                devid: dext.devid,
                                array_phys: phys,
                                inode: 0,
                                file_offset: 0,
                                stored_csum: Some((*stored).clone()),
                                actual_csum: actual,
                                ok: false,
                            });
                        } else {
                            // Re-confirm against the LIVE EXTENT_TREE +
                            // CSUM_TREE before reporting corruption (see
                            // scrub_csum_tree for the rationale).  Only runs
                            // on the rare mismatch path.  The reconfirm AND
                            // the recovery write (via `on_sector`) are wrapped
                            // in a scoped filesystem freeze so a live mount
                            // cannot race the write.  The freeze is held only
                            // for this sector's reconfirm+write window.
                            let _freeze_guard = freeze.as_mut().and_then(|fc| fc.guard());
                            let is_corruption = match crate::btrfs::open::live_data_tree_roots(
                                reader,
                                chunk_map,
                                reader.base_offset(),
                            ) {
                                Some((ext_root, csum_root)) => {
                                    use crate::btrfs::extent::reconfirm_mismatch;
                                    matches!(
                                        reconfirm_mismatch(
                                            reader,
                                            chunk_map,
                                            ext_root,
                                            csum_root,
                                            *sector_logical,
                                            stored,
                                            strategy.hash_len,
                                            strategy.sector_size,
                                        ),
                                        crate::btrfs::extent::Reconfirm::Corruption
                                    )
                                }
                                None => true,
                            };
                            if is_corruption {
                                stats.sectors_mismatch += 1;
                                let phys = dext.phys_start + (*sector_logical - dext.chunk_offset);
                                on_sector(&SectorResult {
                                    logical: *sector_logical,
                                    devid: dext.devid,
                                    array_phys: phys,
                                    inode: 0,
                                    file_offset: 0,
                                    stored_csum: Some((*stored).clone()),
                                    actual_csum: actual,
                                    ok: false,
                                });
                            } else {
                                stats.sectors_stale += 1;
                            }
                            // `_freeze_guard` dropped here -> filesystem thawed.
                        }
                    }
                }
                Err(e) => {
                    // The whole run failed as one read; attribute the error
                    // to each sector in the run so none is silently dropped.
                    for (sector_logical, _stored) in run {
                        stats.sectors_checked += 1;
                        stats.bytes_checked += sector_size;
                        stats.sectors_read_error += 1;
                        eprintln!(
                            "read error at phys 0x{:x} (devid {}, logical 0x{:x}): {}",
                            dext.phys_start + (*sector_logical - dext.chunk_offset),
                            dext.devid,
                            *sector_logical,
                            e
                        );
                    }
                }
            }
        };

        let mut prev_logical: Option<u64> = None;
        for (&sector_logical, stored) in csum_map.range(logical_lo..logical_hi) {
            let contiguous = match prev_logical {
                Some(p) => sector_logical == p + sector_size,
                None => true,
            };
            // Flush when contiguity breaks, when the run hits the memory
            // cap, or at the end of the chunk.
            if !contiguous || run.len() >= MAX_RUN_SECTORS {
                flush(
                    &mut *reader,
                    chunk_map,
                    strategy,
                    batch,
                    &mut freeze,
                    &run,
                    &mut stats,
                );
                run.clear();
            }
            run.push((sector_logical, stored));
            prev_logical = Some(sector_logical);
        }
        flush(
            &mut *reader,
            chunk_map,
            strategy,
            batch,
            &mut freeze,
            &run,
            &mut stats,
        );
    }

    stats
}
