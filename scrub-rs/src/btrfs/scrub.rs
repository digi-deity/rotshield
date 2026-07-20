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
//!
//! ## Pipeline: reader thread + rayon checksum + serial verify
//!
//! The bulk-data read is decoupled from the checksum/verify work via a
//! dedicated **reader thread**: the main thread coalesces contiguous
//! sectors into runs (bounded by [`MAX_RUN_SECTORS`]), sends each run's
//! `(devid, phys, len, Vec<(logical, csum)>)` to the reader thread via a
//! depth-1 command channel, and the reader thread (which owns its own
//! dup'd `File` handle so its seek position never races the main
//! reader's metadata walks) issues one `read_at` syscall per run and
//! sends the bytes back on a depth-1 result channel.  While the main
//! thread's rayon workers checksum run N (CPU-bound), the reader thread
//! is already reading run N+1 from the disk (I/O-bound) — the two are
//! fully decoupled, so the disk stays busy for the whole checksum
//! window, not just for a 1-MiB `POSIX_FADV_WILLNEED` window as in the
//! previous kernel-mediated design.  This bridges the gap between
//! 200 MB/s HDD (1 MiB consumed in ~5 ms) and multi-second SHA256/BLAKE2
//! checksum windows where the previous design left the disk idle.
//! Memory stays bounded: depth-1 channels mean at most one run is in
//! flight on the disk while one is being checksummed in main-thread
//! memory — peak RAM is ≤ 2 × `MAX_RUN_SECTORS` × `sector_size`
//! (128 MiB at the default cap), independent of disk size.

use std::sync::mpsc;
use std::thread;

use rayon::prelude::*;

use crate::btrfs::chunk::ChunkMap;
use crate::btrfs::csum::LazyCsumProvider;
use crate::btrfs::csum_strategy::CsumStrategy;
use crate::btrfs::dev_extent::DevExtent;
use crate::btrfs::key::bg_flag;
use crate::btrfs::reader::FsReader;
use crate::btrfs::util::read_at;

/// Minimum run size (in sectors) before parallel checksumming kicks in.
/// Below this the per-thread dispatch overhead exceeds the savings, so
/// the small synthesized test images (often just a handful of sectors)
/// stay on the parallel path too — the threshold only guards runs of 1
/// sector where there is literally nothing to parallelise.
const PARALLEL_MIN_SECTORS: usize = 2;

/// Maximum sectors per coalesced read run.  Caps peak memory at
/// (depth-1 + 1 in-flight) × `MAX_RUN_SECTORS` × `sector_size` =
/// ~128 MiB at the default cap, independent of disk size.  See
/// [`scrub_dev_tree`] for the pipeline's bounded-memory invariant.
const MAX_RUN_SECTORS: usize = 16384; // 16384 * 4096 = 64 MiB at default sector size

/// Command sent to the reader thread: read `len` bytes at `phys` on
/// `devid`, attached to which dev-extent (`dext_idx`, `dext`) for the
/// logical→physical translation the checksum loop needs.  `entries` is
/// the slice of `(sector_logical, stored_csum)` pairs that the run
/// covers — appended on the main thread (which owns `csum_entries`) and
/// shipped with the command so the reader thread doesn't need to know
/// about the CSUM tree.
struct ReadCmd {
    devid: u64,
    phys: u64,
    len: usize,
    /// The dev-extent this run lives in.  Sent as a `DevExtent` copy so
    /// the result side can compute `array_phys = dext.phys_start + (logical - dext.chunk_offset)`
    /// without re-borrowing the dev_extents slice.
    dext: DevExtent,
    /// The `(sector_logical, stored_csum)` pairs covered by this run.
    /// Owned (not a reference) so it can move across the channel.
    entries: Vec<(u64, Vec<u8>)>,
}

/// Response from the reader thread: the bytes read for the run, plus
/// the dev-extent + csum entries the main thread needs to drive the
/// checksum/verify loop.  An `Err` carries the per-sector `(logical, devid)`
/// list so the error path can attribute the read failure to every sector
/// in the run (matching the previous inline-error attribution).
enum ReadRsp {
    Ok {
        buf: Vec<u8>,
        dext: DevExtent,
        entries: Vec<(u64, Vec<u8>)>,
    },
    Err {
        dext: DevExtent,
        entries: Vec<(u64, Vec<u8>)>,
        err: std::io::Error,
    },
}

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

/// Scrub all sectors of all REGULAR extents (lazy CSUM-tree walker).
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
    csum_provider: &mut LazyCsumProvider,
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
    let sz = sector_size as usize;

    // ----- Reader thread ---------------------------------------------------
    //
    // Spawn a dedicated reader thread that owns a dup'd `File` (so its
    // seek position never races the main `FsReader`'s metadata walks —
    // the reconfirm path on the main thread keeps using `reader`, and the
    // reader thread keeps using its own dup'd handle).  The main thread
    // emits a `ReadCmd` per coalesced run on a depth-1 command channel;
    // the reader issues one `read_at` per run and ships the bytes back on
    // a depth-1 result channel.
    //
    // Pipeline ordering (the *whole point* of this reader thread):
    //
    //   flush(run_N):
    //     1. cmd_tx.send(cmd_N)         — reader starts reading cmd_N from disk
    //     2. process pending(rsp_N-1)  — rayon checksum + reconfirm + emit
    //     3. pending = rsp_rx.recv()    — block until reader finishes cmd_N
    //
    // Step 2 runs in parallel with the reader's disk read for cmd_N.  In the
    // previous design the read and the checksum were serial in user space
    // (the kernel's 1-MiB `POSIX_FADV_WILLNEED` was the only overlap, and
    // at 200 MB/s / GB/s disk rates a 1-MiB prefetch window is exhausted
    // in milliseconds while a 64-MiB SHA256 checksum run takes seconds —
    // the disk sat idle for ~99% of the checksum window).  The depth-1
    // channels bound memory so this overlap stays bounded: at most one
    // run is in flight on the disk while one run is being checksummed +
    // verified on the main thread.  Peak RAM is
    // ~2 × `MAX_RUN_SECTORS` × `sector_size` (128 MiB at the default cap),
    // independent of disk size.
    let base_offset = reader.base_offset();
    let reader_file = match reader.reopen() {
        Ok(f) => f,
        Err(e) => {
            eprintln!(
                "error: could not dup reader handle for reader thread: {e}; \
                 falling back to inline (non-pipelined) reads"
            );
            // Fall back to the previous inline path: spawn the reader thread
            // never; the loop below will detect cmd_tx is None and use the
            // main `reader` directly.  Keep the function correct at the
            // cost of throughput when fd dup fails (rare on Linux).
            return scrub_dev_tree_inline(
                reader,
                chunk_map,
                csum_provider,
                dev_extents,
                strategy,
                batch,
                freeze,
                on_sector,
            );
        }
    };
    let (cmd_tx, cmd_rx) = mpsc::sync_channel::<ReadCmd>(1);
    let (rsp_tx, rsp_rx) = mpsc::sync_channel::<ReadRsp>(1);
    let reader_handle = thread::Builder::new()
        .name("scrub-reader".into())
        .spawn(move || {
            let mut f = reader_file;
            // Read-ahead hint cap.  Matches `MAX_RUN_SECTORS × sector_size`
            // (64 MiB at 4 KiB sectors) — the same cap `read_physical`
            // uses.  Larger than that and the kernel's readahead queue
            // would either spill pages or stall on its own internal
            // limit; smaller and a long checksum window (BLAKE2/SHA256
            // over 64 MiB takes seconds on a single core) would let the
            // disk go idle before the next run is dispatched.
            const READAHEAD_CAP: u64 = 64 << 20; // 64 MiB
            while let Ok(cmd) = cmd_rx.recv() {
                // `read_at` does seek+read_exact; the offset is in bytes
                // from the start of the backing store, so we add
                // `base_offset` (the partition start inside the image/rdev)
                // here — same as `FsReader::read_physical` does.  `phys`
                // in `ReadCmd` is already in array-partition space.
                let off = base_offset.saturating_add(cmd.phys);

                // Prefetch the NEXT contiguous range — the bytes
                // immediately after this run.  On HDD this keeps the disk
                // busy during the long CPU-bound checksum window that
                // will follow on the main thread.  The kernel's readahead
                // cap is ~1 MiB, but the hint is per-call: it instructs
                // the kernel to keep extending its readahead window for
                // the duration of this hint, which the page-cache layer
                // uses to size the elevator.  The hint is a no-op where
                // `posix_fadvise` is unavailable.  We hint immediately
                // `off + len` onward (the byte right after we finish),
                // capped to READAHEAD_CAP — same pattern the inline
                // `FsReader::read_physical` uses, so the pipelined path
                // (which calls `util::read_at` directly) does not lose the
                // prefetch it previously got implicitly via `read_physical`.
                let readahead = (cmd.len as u64).min(READAHEAD_CAP);
                crate::btrfs::reader::advise_willneed(
                    &f,
                    off.saturating_add(cmd.len as u64),
                    readahead,
                );

                let rsp = match read_at(&mut f, off, cmd.len) {
                    Ok(buf) => ReadRsp::Ok {
                        buf,
                        dext: cmd.dext,
                        entries: cmd.entries,
                    },
                    Err(e) => ReadRsp::Err {
                        dext: cmd.dext,
                        entries: cmd.entries,
                        err: e,
                    },
                };
                // Now that we have the bytes in user space, drop the page-
                // cache backing we just consumed.  The bytes are about to
                // be checksummed on the main thread and won't be re-read
                // by anything else during this scrub pass — the data is
                // physically front-to-back, the NEXT read starts at `off
                // + len`, so we never re-visit.  Hinting DONTNEED here
                // keeps a multi-TiB scrub from displacing every other
                // cached page behind itself (the reconfirm path's
                // metadata reads, the OS's filesystem cache for other
                // workloads, etc.).  No-op where `posix_fadvise` is
                // unavailable.
                crate::btrfs::reader::advise_dontneed(&f, off, cmd.len as u64);

                if rsp_tx.send(rsp).is_err() {
                    break;
                }
            }
        })
        .expect("spawn scrub-reader thread");

    // The most recent reader response, received but not yet processed.
    // Drained at the start of the *next* flush so the CPU work overlaps
    // with the reader's disk read for the just-dispatched run (see the
    // pipeline ordering comment above).  Initial value `None` because the
    // very first flush has no previous result to process.
    let mut pending: Option<ReadRsp> = None;

    for (dev_extent_idx, dext) in dev_extents.iter().enumerate() {
        // Resolve the owning chunk.  Every dev-extent must have a matching
        // chunk item; if not, the chunk map is inconsistent with the dev
        // tree and we cannot map this extent — bail loudly rather than
        // silently skipping (which would hide unscrubbed data).
        let chunk = match chunk_map.info(dext.chunk_offset) {
            Some(c) => c,
            None => {
                eprintln!(
                    "error: dev extent at phys 0x{:x} (devid {}) has no matching chunk item",
                    dext.phys_start, dext.devid
                );
                continue;
            }
        };

        // Index of this dev-extent in `dev_extents`, used by the
        // look-ahead prefetch below to find the *next* dev-extent's
        // physical start so we can `WILLNEED` it before the long
        // checksum window of this chunk's runs lets the disk go idle.
        // `iter().position(|d| ...)` would re-walk; carrying the index
        // via enumerate is O(1) and keeps the prefetch lookahead tight.
        let current_dev_extent_idx = dev_extent_idx;

        // Only scrub DATA chunks here (metadata/system handled elsewhere,
        // and they carry no csum-tree entries anyway).
        if chunk.flags & bg_flag::DATA == 0 {
            continue;
        }

        let logical_lo = dext.chunk_offset;
        let logical_hi = dext.chunk_offset + dext.length;

        // Look-ahead prefetch: hint the kernel to start pulling the *next*
        // dev-extent's first sector while we're still streaming the csum
        // entries (and issuing run reads) for the current one.  On HDD the
        // checksum window for a 64-MiB BLAKE2 run is multiple seconds, and
        // Step 3's `advise_willneed(off + len, ...)` only prefetches the
        // next contiguous range *within this chunk* — once we cross the
        // chunk boundary into a different `dext`, the previous hint does
        // not cover the new physical start.  Hinting the next dev-extent's
        // first run here means the disk has its first pages cached before
        // the reader thread ever issues the read for it.  The kernel's
        // `WILLNEED` cap (~1 MiB) is plenty for a single first-run hint;
        // for the long-tail of subsequent runs inside that chunk, Step 3's
        // `advise_willneed(off + len, ...)` keeps the read-ahead going.
        //
        // The hint is bounded: we only prefetch the first `MAX_RUN_SECTORS
        // × sector_size` (64 MiB at the default cap) of the next dev-extent
        // — exactly one run's worth, matching `MAX_RUN_SECTORS`.  Memory
        // budget unchanged: the prefetch fills page-cache pages, which the
        // reader thread will immediately `advise_dontneed` after it
        // consumes them, so steady-state RAM stays at the same 2×
        // MAX_RUN_SECTORS bound.  We skip the hint when the next dev-extent
        // is the *same* physical run (no gap to bridge) or when there is
        // no next dev-extent (last one — nothing to prefetch).
        let next_idx = current_dev_extent_idx + 1;
        if next_idx < dev_extents.len() {
            let next = &dev_extents[next_idx];
            // Only DATA chunks carry bulk reads, so only hint DATA
            // dev-extents.  Resolving the chunk flag here is the same
            // `chunk_map.info` call the next loop iteration will do
            // anyway, so we're not paying for a metadata walk that
            // wouldn't otherwise happen — we're just shifting it earlier.
            if let Some(next_chunk) = chunk_map.info(next.chunk_offset)
                && next_chunk.flags & bg_flag::DATA != 0
            {
                let hint_len = (next.length as usize)
                    .min(MAX_RUN_SECTORS * sz)
                    .max(strategy.sector_size as usize);
                reader.prefetch_logical(chunk_map, next.chunk_offset, hint_len);
            }
        }

        // Run-coalescing state.  Consecutive csum entries that are
        // physically contiguous (`next_logical == sector_logical +
        // sector_size`) are also physically contiguous on disk, so we
        // accumulate a run and issue a single `read_at` for the whole run
        // — turning "N syscalls of 4–16 KiB" into "one syscall of however
        // big the contiguous run is".  A break in contiguity or the
        // `MAX_RUN_SECTORS` cap flushes the pending run first.  The cap
        // bounds peak memory at `MAX_RUN_SECTORS × sector_size` per run
        // (64 MiB at the default) regardless of on-disk extent size.
        let mut run: Vec<(u64, Vec<u8>)> = Vec::new();
        let mut prev_logical: Option<u64> = None;

        // `flush` drives the pipelined reader thread: send the run's
        // ReadCmd to the reader (so the disk read starts), process the
        // PREVIOUS run's response (rayon checksum + reconfirm + on_sector),
        // then block on the just-sent cmd's response.  Captures all the
        // borrows the inner processing path needs — `reader` for the
        // reconfirm walk, `freeze` for the live-mount freeze guard,
        // `on_sector` for the mismatch emit, `stats` for counters, plus
        // the channel endpoints.
        let mut flush = |run: &mut Vec<(u64, Vec<u8>)>| {
            if run.is_empty() {
                return;
            }
            let run_phys = dext.phys_start + (run[0].0 - dext.chunk_offset);
            let run_len = run.len() * sz;
            let cmd = ReadCmd {
                devid: dext.devid,
                phys: run_phys,
                len: run_len,
                dext: *dext,
                entries: std::mem::take(run),
            };
            // 1. Send the next cmd FIRST so the reader can start reading it
            //    while we process the previous result below.
            if cmd_tx.send(cmd).is_err() {
                return;
            }
            // 2. Process the previous response (CPU-bound rayon work) WHILE
            //    the reader reads the just-sent cmd from disk.
            if let Some(rsp) = pending.take() {
                process_rsp(
                    rsp,
                    reader,
                    chunk_map,
                    strategy,
                    batch,
                    &mut freeze,
                    &mut on_sector,
                    &mut stats,
                );
            }
            // 3. Pull the just-sent cmd's response — block until reader done.
            pending = rsp_rx.recv().ok();
        };

        // The csum-provider callback drives the run coalescer: it streams
        // `(logical, csum)` pairs in ascending order for this dev-extent's
        // span, and the closure above flushes each completed run into the
        // reader pipeline as it fills.
        csum_provider.range(logical_lo, logical_hi, |e| {
            let contiguous = match prev_logical {
                Some(p) => e.logical == p + sector_size,
                None => true,
            };
            if !contiguous || run.len() >= MAX_RUN_SECTORS {
                flush(&mut run);
            }
            run.push((e.logical, e.csum));
            prev_logical = Some(e.logical);
        });
        // Flush the trailing run for this dev-extent.
        flush(&mut run);
    }

    // Drain the pipeline: the last `flush` left one response in `pending`
    // (the run we sent but never processed because no subsequent flush
    // ran).  Process it now.  Then drop the command sender so the reader
    // thread sees channel close and exits cleanly.
    if let Some(rsp) = pending.take() {
        process_rsp(
            rsp,
            reader,
            chunk_map,
            strategy,
            batch,
            &mut freeze,
            &mut on_sector,
            &mut stats,
        );
    }
    drop(cmd_tx);
    let _ = reader_handle.join();

    stats
}

// ---------------------------------------------------------------------------
// Inline (non-pipelined) fallback — used only when `reader.reopen()` fails
// (rare on Linux: dup fd failure under extreme fd pressure).  Preserves
// the exact pre-pipeline semantics so a transient dup failure degrades to
// the previous throughput, never to a correctness regression.  This is the
// same read-coalesce + parallel-checksum + serial-verify loop as the
// pipelined path, just with the read happening inline on the main thread
// instead of on the reader thread.
//
// Kept as a separate function (rather than an `if let Some(thread)` switch
// inside `scrub_dev_tree`) so the pipelined path doesn't carry the inline
// fallback's borrow complexity around for every flush call.
#[allow(clippy::too_many_arguments)]
fn scrub_dev_tree_inline<F>(
    reader: &mut FsReader,
    chunk_map: &ChunkMap,
    csum_provider: &mut LazyCsumProvider,
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
    let sz = sector_size as usize;

    for dext in dev_extents {
        let chunk = match chunk_map.info(dext.chunk_offset) {
            Some(c) => c,
            None => continue,
        };
        if chunk.flags & bg_flag::DATA == 0 {
            continue;
        }
        let logical_lo = dext.chunk_offset;
        let logical_hi = dext.chunk_offset + dext.length;

        let mut run: Vec<(u64, Vec<u8>)> = Vec::new();
        let mut prev_logical: Option<u64> = None;
        let mut flush = |run: &mut Vec<(u64, Vec<u8>)>, reader: &mut FsReader| {
            if run.is_empty() {
                return;
            }
            let run_phys = dext.phys_start + (run[0].0 - dext.chunk_offset);
            let run_len = run.len() * sz;
            match reader.read_physical(dext.devid, run_phys, run_len) {
                Ok(buf) => process_buf(
                    &buf,
                    run,
                    dext,
                    strategy,
                    batch,
                    &mut freeze,
                    reader,
                    chunk_map,
                    &mut on_sector,
                    &mut stats,
                ),
                Err(e) => {
                    for (sector_logical, _stored) in run.iter() {
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
        csum_provider.range(logical_lo, logical_hi, |e| {
            let contiguous = match prev_logical {
                Some(p) => e.logical == p + sector_size,
                None => true,
            };
            if !contiguous || run.len() >= MAX_RUN_SECTORS {
                flush(&mut run, reader);
            }
            run.push((e.logical, e.csum));
            prev_logical = Some(e.logical);
        });
        flush(&mut run, reader);
    }

    stats
}

// ---------------------------------------------------------------------------
// Shared run-processing helper — used by both the pipelined path (from the
// reader thread's `ReadRsp::Ok` arm of `process_rsp`) and the inline
// fallback (directly after `reader.read_physical`).  Runs the parallel
// checksum via rayon, then the serial comparison — counting
// ok/mismatch/no-csum/stale + invoking the reconfirm/freeze/emit path on
// actual mismatches.  This is the same logic that previously lived inline
// in `scrub_dev_tree::flush`; extracted so the two call sites don't drift.
#[allow(clippy::too_many_arguments)]
fn process_buf(
    buf: &[u8],
    run: &[(u64, Vec<u8>)],
    dext: &DevExtent,
    strategy: &CsumStrategy,
    batch: bool,
    freeze: &mut Option<&mut crate::freeze::FreezeController>,
    reader: &mut FsReader,
    chunk_map: &ChunkMap,
    on_sector: &mut impl FnMut(&SectorResult),
    stats: &mut ScrubStats,
) {
    let sz = strategy.sector_size as usize;
    // Parallel per-sector checksum.  Threshold is parity, not perf — at 1
    // sector there's literally nothing to parallelise so we skip the
    // rayon dispatch overhead; at ≥ 2 we always go parallel regardless of
    // algorithm (no per-algo dispatch, per the design call) so the code
    // stays one path.
    let actuals: Vec<Vec<u8>> = if run.len() >= PARALLEL_MIN_SECTORS {
        let slices: Vec<(usize, usize)> = (0..run.len()).map(|i| (i * sz, (i + 1) * sz)).collect();
        slices
            .par_iter()
            .map(|&(s, e)| strategy.compute(&buf[s..e]))
            .collect()
    } else {
        run.iter()
            .enumerate()
            .map(|(i, _)| {
                let s = i * sz;
                let e = s + sz;
                strategy.compute(&buf[s..e])
            })
            .collect()
    };

    for (i, (sector_logical, stored)) in run.iter().enumerate() {
        let actual = &actuals[i];
        stats.sectors_checked += 1;
        stats.bytes_checked += strategy.sector_size;
        if actual == stored {
            stats.sectors_ok += 1;
        } else if batch {
            // Batched recovery mode: emit the raw mismatch as a candidate
            // for the (separate) recovery sink.  We do NOT re-confirm or
            // count it here — the sink owns mismatch/stale accounting for
            // the batch so the count stays honest.
            let phys = dext.phys_start + (*sector_logical - dext.chunk_offset);
            on_sector(&SectorResult {
                logical: *sector_logical,
                devid: dext.devid,
                array_phys: phys,
                inode: 0,
                file_offset: 0,
                stored_csum: Some(stored.clone()),
                actual_csum: actual.clone(),
                ok: false,
            });
        } else {
            // Re-confirm against the LIVE EXTENT_TREE + CSUM_TREE before
            // reporting corruption.  Only runs on the rare mismatch path.
            // The reconfirm AND the recovery write (via `on_sector`) are
            // wrapped in a scoped filesystem freeze so a live mount cannot
            // race the write.  The freeze is held only for this sector's
            // reconfirm+write window.
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
                    stored_csum: Some(stored.clone()),
                    actual_csum: actual.clone(),
                    ok: false,
                });
            } else {
                stats.sectors_stale += 1;
            }
            // `_freeze_guard` dropped here -> filesystem thawed.
        }
    }
}

// `process_rsp` is the entry point the pipelined path calls inside its
// flush closure.  It dispatches a `ReadRsp` from the reader thread into
// `process_buf` (Ok) or the per-sector read-error attribution path (Err).
// Separated from `process_buf` so the inline fallback can call
// `process_buf` directly without having to wrap a fake `ReadRsp`.
#[allow(clippy::too_many_arguments)]
fn process_rsp(
    rsp: ReadRsp,
    reader: &mut FsReader,
    chunk_map: &ChunkMap,
    strategy: &CsumStrategy,
    batch: bool,
    freeze: &mut Option<&mut crate::freeze::FreezeController>,
    on_sector: &mut impl FnMut(&SectorResult),
    stats: &mut ScrubStats,
) {
    match rsp {
        ReadRsp::Ok { buf, dext, entries } => {
            process_buf(
                &buf, &entries, &dext, strategy, batch, freeze, reader, chunk_map, on_sector, stats,
            );
        }
        ReadRsp::Err { dext, entries, err } => {
            for (sector_logical, _stored) in entries.iter() {
                stats.sectors_checked += 1;
                stats.bytes_checked += strategy.sector_size;
                stats.sectors_read_error += 1;
                eprintln!(
                    "read error at phys 0x{:x} (devid {}, logical 0x{:x}): {}",
                    dext.phys_start + (*sector_logical - dext.chunk_offset),
                    dext.devid,
                    *sector_logical,
                    err
                );
            }
        }
    }
}
