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
//! bounded command channel (depth [`PREFETCH_DEPTH_FAST`] /
//! [`PREFETCH_DEPTH_SLOW`], see `prefetch_depth_for`), and the reader
//! thread (which owns its own dup'd `File` handle so its seek position
//! never races the main reader's metadata walks — reads on both handles
//! use `pread`/`util::pread_at`, a *positional* read, never `seek`, since
//! `try_clone` shares the underlying open-file-description's seek
//! position with the original) issues one positional read syscall per
//! run and sends the bytes back on a same-depth result channel.  The main
//! thread lets up to `prefetch_depth` cmds be outstanding before it
//! blocks on the oldest response (FIFO order, always via blocking `recv`
//! — never `try_recv`, which was tried and broke correctness, see
//! `docs/PROGRESS_HDD_BURST.md`), so the reader thread can race several
//! runs ahead of the main thread's CPU-bound checksum work instead of a
//! strict 1:1 lockstep.  This is what keeps the disk busy across the
//! *whole* multi-second BLAKE2/SHA256 checksum window on HDDs, rather
//! than just for a single run's `POSIX_FADV_WILLNEED` window (200 MB/s
//! HDD: 64 MiB run consumed in ~320 ms, checksummed in ~4 s — a depth-1
//! lockstep leaves the disk idle for ~90% of that window). Memory stays
//! bounded: at most `prefetch_depth` runs are in flight at once, so peak
//! RAM is ≤ 2 × `prefetch_depth` × `MAX_RUN_SECTORS` × `sector_size`
//! (512 MiB at depth 4 / the default `MAX_RUN_SECTORS` cap), independent
//! of disk size.

use std::sync::mpsc;
use std::thread;

use rayon::prelude::*;

use crate::btrfs::chunk::ChunkMap;
use crate::btrfs::csum::LazyCsumProvider;
use crate::btrfs::csum_strategy::CsumStrategy;
use crate::btrfs::dev_extent::DevExtent;
use crate::btrfs::key::bg_flag;
use crate::btrfs::reader::FsReader;
use crate::btrfs::util::pread_at;
use crate::status::StatusCounters;
use std::sync::atomic::Ordering;

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

/// Fan-out for the EIO divide-and-conquer ([`isolate_run`]): how many
/// sector-aligned pieces a failing read range is split into per level.
///
/// A binary split (2) needs `log2(MAX_RUN_SECTORS)` = 14 *failing* reads to
/// pinpoint one bad sector in a full 64 MiB run; an 8-way split cuts that
/// to `log8(16384)` ≈ 5.  On a dying disk a failing read is the expensive
/// event (it re-touches the bad sector), so we want that count low; the
/// extra successful reads on good sectors that a wider fan-out costs are
/// cheap normal wear.  Must be ≥ 2 — a factor of 1 would recurse forever.
/// The split clamps to the range's sector count, so small runs (the common
/// case) isolate in a single level regardless of this value.  See
/// `docs/EIO-robustness-design.md` §5.1.
const EIO_SPLIT_FACTOR: usize = 8;

/// Pipeline depth (number of in-flight `ReadCmd`s / `ReadRsp`s on each
/// side of the scrub reader thread).  Greater than 1 lets the reader
/// thread issue several `read_at`s back-to-back without waiting for the
/// main thread to finish checksumming the previous run — the key
/// throughput lever on HDDs where a single 64 MiB BLAKE2/SHA256 run
/// takes seconds of CPU while a 200 MB/s HDD reads it in ~320 ms, leaving
/// the disk otherwise idle for ~99% of the checksum window with depth-1
/// channels (the 2026-07-19 implementation).  Higher depth keeps more
/// runs pre-buffered in the page cache so the disk stays busy for the
/// whole checksum window instead of saw-toothing between 240 MB/s bursts
/// and 0-idle valleys.
///
/// Sized per-algorithm to bound memory:
/// * Slow hashes (sha256, blake2): the checksum window is multi-second,
///   so the disk needs ~4× the per-run bytes pre-buffered to ride out
///   one CPU window → depth 4.  Peak RAM: 2 × 4 × 64 MiB = 512 MiB at
///   default `MAX_RUN_SECTORS`, still bounded and independent of disk
///   size.
/// * Fast hashes (crc32c, xxhash): the disk is the bottleneck, so
///   prefetch buffering > 1 just dirties the cache without throughput
///   benefit.  Depth 2 gives one run in flight on disk + one ready in
///   the result queue so the main thread never blocks on the disk head,
///   without piling up redundancy.  Peak RAM: 2 × 2 × 64 MiB = 256 MiB.
///
/// The depth is selected from the strategy at the start of `scrub_dev_tree`
/// and moved into the channel constructors.  Changing this constant is
/// the single knob for tuning the throughput-vs-RAM curve.
const PREFETCH_DEPTH_FAST: usize = 2; // crc32c, xxhash
const PREFETCH_DEPTH_SLOW: usize = 4; // sha256, blake2

fn prefetch_depth_for(strategy: &CsumStrategy) -> usize {
    match strategy.name {
        "sha256" | "blake2" => PREFETCH_DEPTH_SLOW,
        // crc32c, xxhash, and any future fast hash default to the small
        // depth — disk-bound algorithms don't benefit from buffering.
        _ => PREFETCH_DEPTH_FAST,
    }
}

/// Command sent to the reader thread: read `len` bytes at `phys`,
/// attached to which dev-extent (`dext`) for the logical→physical
/// translation the checksum loop needs.  `entries` is the slice of
/// `(sector_logical, stored_csum)` pairs that the run covers — appended
/// on the main thread (which owns `csum_entries`) and shipped with the
/// command so the reader thread doesn't need to know about the CSUM tree.
struct ReadCmd {
    phys: u64,
    len: usize,
    /// The dev-extent this run lives in.  Sent as a `DevExtent` copy so
    /// the result side can compute `array_phys = dext.phys_start + (logical - dext.chunk_offset)`
    /// without re-borrowing the dev_extents slice.
    dext: DevExtent,
    /// The `(sector_logical, stored_csum)` pairs covered by this run.
    /// Owned (not a reference) so it can move across the channel.
    entries: Vec<(u64, Vec<u8>)>,
    /// Sector size in bytes.  Needed by the reader thread's EIO
    /// divide-and-conquer (which splits the run on sector boundaries).
    sector_size: u64,
}

/// Response from the reader thread: the run's bytes (or the isolated
/// good/bad regions if a sector failed to read), plus the dev-extent and
/// csum entries the main thread needs to drive the checksum/verify loop.
///
/// This is the ONLY result shape.  On a healthy disk the reader's
/// divide-and-conquer takes its first step (a whole-run `pread`), succeeds,
/// and yields exactly one good region covering the whole run with no bad
/// sectors — the degenerate case IS the happy path, so there is no separate
/// `Ok` variant to drift.  When the run's read fails, the reader has
/// already divided-and-conquered the byte range down to sector granularity:
/// `good` lists the sectors that read fine (as `(byte_start_within_run,
/// bytes)` regions, each sector-aligned) and `bad` lists the byte starts of
/// the sectors that failed with `EIO`.  This bounds the blast radius of one
/// bad sector to itself instead of discarding the whole (up to 64 MiB) run
/// — see `docs/EIO-robustness-design.md` §5.1.
enum ReadRsp {
    Isolated {
        dext: DevExtent,
        entries: Vec<(u64, Vec<u8>)>,
        /// Sector-aligned good regions: `(byte_start_within_run, bytes)`.
        good: Vec<(usize, Vec<u8>)>,
        /// Sector-aligned byte starts of the sectors that failed (EIO set).
        bad: Vec<usize>,
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
    /// The sector's source bytes were **unreadable** (device `EIO`) — the
    /// sector could not be read at all, so `actual_csum` is empty and there
    /// is no on-disk data to compare against the stored csum.  This is the
    /// clearest "the disk is broken" signal and, with a stored csum
    /// available, is recoverable from parity.  `false` for a sector that
    /// read fine (whether its csum matched or not).
    pub unreadable: bool,
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
    counters: Option<&StatusCounters>,
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
    // Spawn a dedicated reader thread that owns a dup'd `File`. `try_clone`
    // shares the same underlying open-file-description (and thus seek
    // position) with the original — so both this thread's reads and the
    // main thread's reconfirm reads via `reader` use *positional* reads
    // (`util::pread_at` / `FsReader::read_physical`, never `seek`) to avoid
    // racing each other's cursor.  The main thread emits a `ReadCmd` per
    // coalesced run on a `prefetch_depth`-deep command channel; the reader
    // issues one positional read per run and ships the bytes back on a
    // same-depth result channel.
    //
    // Pipeline ordering (the *whole point* of this reader thread):
    //
    //   flush(run_N):
    //     1. cmd_tx.send(cmd_N)           — reader starts reading cmd_N from disk
    //     2. if inflight >= prefetch_depth:
    //          process the OLDEST outstanding response (rayon checksum +
    //          reconfirm + emit), blocking on `rsp_rx.recv()` if needed
    //
    // Because responses are consumed strictly FIFO (never `try_recv`, see
    // `docs/PROGRESS_HDD_BURST.md`), correctness does not depend on depth —
    // but letting up to `prefetch_depth` cmds accumulate before the first
    // blocking receive lets the reader thread's disk reads run several
    // runs ahead of the main thread's CPU-bound checksum work, instead of
    // stalling on a strict 1:1 send/recv lockstep. In the previous design
    // the read and the checksum were serial in user space (the kernel's
    // 1-MiB `POSIX_FADV_WILLNEED` was the only overlap, and at 200 MB/s /
    // GB/s disk rates a 1-MiB prefetch window is exhausted in milliseconds
    // while a 64-MiB SHA256 checksum run takes seconds — the disk sat idle
    // for ~99% of the checksum window, and even a depth-1 pipelined
    // lockstep still leaves the disk idle between runs once the CPU window
    // exceeds the disk's per-run read time).  Peak RAM is bounded to
    // ~2 × `prefetch_depth` × `MAX_RUN_SECTORS` × `sector_size`
    // (512 MiB at depth 4 / the default cap), independent of disk size.
    let base_offset = reader.base_offset();
    // Channel depth: tunes the throughput-vs-RAM curve.  Slow hashes
    // (sha256, blake2) pre-buffer multiple runs so the disk stays busy
    // during the long CPU checksum window on HDDs; fast hashes (crc32c,
    // xxhash) keep the depth small because the disk is the bottleneck
    // and deeper prefetch just dirties cache.  See `prefetch_depth_for`.
    let prefetch_depth = prefetch_depth_for(strategy);
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
                counters,
                on_sector,
            );
        }
    };
    let (cmd_tx, cmd_rx) = mpsc::sync_channel::<ReadCmd>(prefetch_depth);
    let (rsp_tx, rsp_rx) = mpsc::sync_channel::<ReadRsp>(prefetch_depth);
    let reader_handle = thread::Builder::new()
        .name("scrub-reader".into())
        .spawn(move || {
            let f = reader_file;
            // Read-ahead hint cap.  Matches `MAX_RUN_SECTORS × sector_size`
            // (64 MiB at 4 KiB sectors) — the same cap `read_physical`
            // uses.  `POSIX_FADV_WILLNEED` is per-call: each hint asks
            // the kernel to prefetch `len` bytes starting at `off`.  The
            // kernel's internal read-ahead window per hint is ~1 MiB but
            // it services the hint asynchronously, pulling pages from
            // disk in its own elevator-sized chunks — so a single 64-MiB
            // hint becomes ~64 disk-sized fetches spread out in time,
            // exactly what we want to fill a multi-second checksum window.
            //
            // The pipelined path now uses `PREFETCH_DEPTH` (multiple
            // in-flight commands) instead of depth-1.  This lets the
            // reader thread issue several `read_at`s back-to-back without
            // waiting for the main thread to finish processing each
            // result, which is what keeps the disk busy during the long
            // BLAKE2/SHA256 checksum window on HDDs — previously the
            // disk went idle for ~99% of the checksum window between
            // depth-1 runs.  Memory bound unchanged in steady-state: at
            // any time at most `PREFETCH_DEPTH` runs are in flight (one
            // per channel slot × 2 channels), where each run is ≤
            // `MAX_RUN_SECTORS × sector_size` (64 MiB).  The caller-side
            // `pending` queue holds at most one *processed-but-not-yet-
            // consumed* response, so peak RAM stays bounded.
            const READAHEAD_CAP: u64 = 64 << 20; // 64 MiB
            while let Ok(cmd) = cmd_rx.recv() {
                // `pread_at` is a positional read (no seek) — required
                // here because this handle is a `try_clone` of the main
                // reader's `File`, which *shares the same underlying
                // open-file-description and seek position* with the
                // original. A seek-based read here would race any
                // concurrent seek-based read the main thread issues via
                // `FsReader` (e.g. a reconfirm read on mismatch), silently
                // reading the wrong bytes. See `util::pread_at` for
                // details.  The offset is in bytes from the start of the
                // backing store, so we add `base_offset` (the partition
                // start inside the image/rdev) here — same as
                // `FsReader::read_physical` does.  `phys` in `ReadCmd` is
                // already in array-partition space.
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
                // (which calls `util::pread_at` directly) does not lose the
                // prefetch it previously got implicitly via `read_physical`.
                let readahead = (cmd.len as u64).min(READAHEAD_CAP);
                crate::btrfs::reader::advise_willneed(
                    &f,
                    off.saturating_add(cmd.len as u64),
                    readahead,
                );

                // The reader thread owns the `File` (a dup of the main
                // reader's), so it is the right place to own the EIO
                // divide-and-conquer.  `isolate_run_read` always produces
                // the single `Isolated` result shape: on a healthy disk it
                // does a whole-run `pread` (its first step) and yields one
                // good region with no bad sectors — the degenerate happy
                // path — and on a read failure it isolates the good regions
                // from the truly-bad sectors so a single bad sector no
                // longer discards the whole (up to 64 MiB) run.
                let rsp = isolate_run_read(&f, off, cmd);
                // Skip `POSIX_FADV_DONTNEED` here.  An earlier version
                // dropped pages immediately after read so a multi-TiB one-
                // pass scrub would not evict every other cached page
                // behind itself, but with `PREFETCH_DEPTH > 1` the next
                // run's `advise_willneed(off + len, …)` is already
                // fetching into the page cache moments later and racing
                // the eviction for the SAME range (the prefetch hint
                // starts at `off + len`, but the eviction drops `off` —
                // on a contiguous data track the two ranges touch and the
                // kernel sometimes evicts pages it just prefetched,
                // producing a saw-tooth throughput pattern).  The one-
                // pass nature of the scrub (front-to-back, never revisits)
                // means the kernel's own LRU eviction will reclaim those
                // pages naturally as the prefetch head advances; an
                // explicit `DONTNEED` here only fights the prefetch we
                // want.  Re-enable only behind a `--cache-purge` flag for
                // users who explicitly want to free cache during the
                // scrub; the default behaviour is now no eviction.
                if rsp_tx.send(rsp).is_err() {
                    break;
                }
            }
        })
        .expect("spawn scrub-reader thread");

    // Number of `ReadCmd`s sent to the reader thread that have not yet
    // had their `ReadRsp` received and processed.  Bounded by
    // `prefetch_depth`: once `inflight` reaches the depth we block on
    // `rsp_rx.recv()` (FIFO, so always the oldest outstanding response)
    // before sending the next cmd.  This lets the reader thread race up
    // to `prefetch_depth` runs ahead of the main thread's CPU-bound
    // checksum work — the fix for the HDD saw-tooth: a depth-1 lockstep
    // (send cmd, process previous, block-recv the one just sent) only
    // ever overlaps a single run's disk read with the *previous* run's
    // checksum, so once the read finishes (fast on HDD) the reader sits
    // idle waiting for the next cmd, which only arrives after the much
    // longer checksum window completes.  Buffering multiple cmds ahead
    // (still drained via blocking `recv`, never `try_recv` — see
    // `docs/PROGRESS_HDD_BURST.md` for why `try_recv` broke correctness)
    // keeps the reader busy across the whole checksum window while
    // preserving strict FIFO response ordering.
    let mut inflight: usize = 0;

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
        // ReadCmd to the reader thread (letting up to `prefetch_depth`
        // cmds be outstanding at once so the reader can race ahead of the
        // main thread's checksum work), then — only once `inflight`
        // reaches `prefetch_depth` — block-receive and process the
        // *oldest* outstanding response to make room.  Captures all the
        // borrows the inner processing path needs — `reader` for the
        // reconfirm walk, `on_sector` for the mismatch emit, `stats` for
        // counters, plus the channel endpoints.
        let mut flush = |run: &mut Vec<(u64, Vec<u8>)>| {
            if run.is_empty() {
                return;
            }
            let run_phys = dext.phys_start + (run[0].0 - dext.chunk_offset);
            let run_len = run.len() * sz;
            let cmd = ReadCmd {
                phys: run_phys,
                len: run_len,
                dext: *dext,
                entries: std::mem::take(run),
                sector_size,
            };
            // 1. Send the cmd.  The command channel itself is bounded to
            //    `prefetch_depth` slots, so this blocks only if the
            //    reader thread is already that far behind.
            if cmd_tx.send(cmd).is_err() {
                return;
            }
            inflight += 1;
            // 2. Only block for a response once we've let `prefetch_depth`
            //    cmds accumulate — this is what lets the reader thread
            //    read several runs ahead of the checksum work instead of
            //    stalling on a 1:1 send/recv lockstep.
            if inflight >= prefetch_depth
                && let Ok(rsp) = rsp_rx.recv()
            {
                inflight -= 1;
                process_rsp(
                    rsp,
                    reader,
                    chunk_map,
                    strategy,
                    batch,
                    counters,
                    &mut on_sector,
                    &mut stats,
                );
            }
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

        // Coarse progress: this DATA dev-extent's physical bytes are now
        // fully scrubbed.  Bump the numerator by the extent's full length
        // so the status server's `progress_pct` advances.  Crediting the
        // whole extent at completion (rather than per-sector) is
        // deliberate: free space / inline / nodatasum gaps inside the
        // extent were never csum-yielded but are still "walked past", so
        // crediting the full length keeps the bar monotonic and lands it
        // on exactly 100% when the last extent finishes.
        if let Some(c) = counters {
            c.progress_done.fetch_add(dext.length, Ordering::Relaxed);
        }
    }

    // Drain the pipeline: close the command channel so the reader thread
    // knows no more cmds are coming, then block-receive (in strict FIFO
    // order — never `try_recv`, see `docs/PROGRESS_HDD_BURST.md`) every
    // remaining outstanding response before joining the reader thread.
    drop(cmd_tx);
    while inflight > 0 {
        match rsp_rx.recv() {
            Ok(rsp) => {
                inflight -= 1;
                process_rsp(
                    rsp,
                    reader,
                    chunk_map,
                    strategy,
                    batch,
                    counters,
                    &mut on_sector,
                    &mut stats,
                );
            }
            Err(_) => break,
        }
    }
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
    counters: Option<&StatusCounters>,
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
            // Single path, same as the pipelined reader: isolate the run
            // into good regions + bad sectors.  On a healthy disk this is
            // one whole-run read (`isolate_run`'s first step) -> one good
            // region, no bad sectors — the degenerate happy path.
            let mut good: Vec<(usize, Vec<u8>)> = Vec::new();
            let mut bad: Vec<usize> = Vec::new();
            {
                let mut read = |phys: u64, len: usize| {
                    reader.read_physical(dext.devid, phys, len)
                };
                isolate_run(
                    &mut read,
                    run_phys,
                    0,
                    run_len,
                    sector_size,
                    &mut good,
                    &mut bad,
                );
            }
            process_isolated(
                good,
                bad,
                dext,
                run,
                strategy,
                batch,
                counters,
                reader,
                chunk_map,
                &mut on_sector,
                &mut stats,
            );
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

        // Coarse progress: same numerator bump as the pipelined path (see
        // `scrub_dev_tree`), so the inline fallback reports identical
        // dev-tree-position progress.
        if let Some(c) = counters {
            c.progress_done.fetch_add(dext.length, Ordering::Relaxed);
        }
    }

    stats
}

// ---------------------------------------------------------------------------
// Shared run-processing helper — used by both the pipelined path (from
// `process_rsp`'s [`ReadRsp`] arm) and the inline fallback (directly after
// `isolate_run`).  Runs the parallel checksum via rayon, then the serial
// comparison — counting ok/mismatch/no-csum/stale + invoking the
// reconfirm/emit path on actual mismatches.  This is the same logic that
// previously lived inline in `scrub_dev_tree::flush`; extracted so the
// call sites don't drift.
#[allow(clippy::too_many_arguments)]
fn process_buf(
    buf: &[u8],
    run: &[(u64, Vec<u8>)],
    dext: &DevExtent,
    strategy: &CsumStrategy,
    batch: bool,
    counters: Option<&StatusCounters>,
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
        if let Some(c) = counters {
            c.sectors_checked.fetch_add(1, Ordering::Relaxed);
            c.bytes_checked
                .fetch_add(strategy.sector_size, Ordering::Relaxed);
        }
        if actual == stored {
            stats.sectors_ok += 1;
            if let Some(c) = counters {
                c.sectors_ok.fetch_add(1, Ordering::Relaxed);
            }
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
                unreadable: false,
                ok: false,
            });
        } else {
            // Re-confirm against the LIVE EXTENT_TREE + CSUM_TREE before
            // reporting corruption.  Only runs on the rare mismatch path.
            // The scrub walker never freezes and never writes here — a
            // live-mount freeze (if any) is owned by the recovery sink at
            // write time, not by the walker.
            let tree_says_corrupt = match crate::btrfs::open::live_data_tree_roots(
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
                        crate::fs::Reconfirm::Corruption
                    )
                }
                None => true,
            };
            // `reconfirm_mismatch` only checks TREE state (does the live
            // CSUM_TREE still expect `stored` here?) — it never re-reads
            // the actual on-disk bytes, so it cannot see a live rewrite
            // that hasn't committed a new csum yet (NODATACOW in-place
            // rewrites, or the ordinary lag between a COW write landing on
            // disk and its transaction committing to the CSUM_TREE). Read
            // the CURRENT bytes at this physical offset now and only keep
            // treating this as corruption if they *still* disagree with
            // `stored` — otherwise it already self-healed since we first
            // read it, and reporting it would be a scary false positive.
            let phys = dext.phys_start + (*sector_logical - dext.chunk_offset);
            let is_corruption = tree_says_corrupt
                && match reader.read_physical(dext.devid, phys, sz) {
                    Ok(fresh) => &strategy.compute(&fresh) != stored,
                    // Couldn't re-read — be conservative, keep the tree's
                    // verdict rather than silently dropping a possible
                    // mismatch.
                    Err(_) => true,
                };
            if is_corruption {
                stats.sectors_mismatch += 1;
                if let Some(c) = counters {
                    c.mismatch.fetch_add(1, Ordering::Relaxed);
                }
                on_sector(&SectorResult {
                    logical: *sector_logical,
                    devid: dext.devid,
                    array_phys: phys,
                    inode: 0,
                    file_offset: 0,
                    stored_csum: Some(stored.clone()),
                    actual_csum: actual.clone(),
                    unreadable: false,
                    ok: false,
                });
            } else {
                stats.sectors_stale += 1;
                if let Some(c) = counters {
                    c.stale.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }
}

// `process_rsp` is the entry point the pipelined path calls inside its
// flush closure.  There is a single [`ReadRsp`] shape — the reader's
// divide-and-conquer output (good regions + bad sectors) — so this just
// hands it to `process_isolated`, which re-verifies the good regions via
// `process_buf` and counts/emits the bad (EIO) sectors.
#[allow(clippy::too_many_arguments)]
fn process_rsp(
    rsp: ReadRsp,
    reader: &mut FsReader,
    chunk_map: &ChunkMap,
    strategy: &CsumStrategy,
    batch: bool,
    counters: Option<&StatusCounters>,
    on_sector: &mut impl FnMut(&SectorResult),
    stats: &mut ScrubStats,
) {
    let ReadRsp::Isolated {
        dext,
        entries,
        good,
        bad,
    } = rsp;
    process_isolated(
        good,
        bad,
        &dext,
        &entries,
        strategy,
        batch,
        counters,
        reader,
        chunk_map,
        on_sector,
        stats,
    );
}

/// The reader thread's run-read handler: always divide-and-conquer the
/// byte range, isolating the good regions from the truly-bad sectors (see
/// `docs/EIO-robustness-design.md` §5.1).
///
/// `isolate_run`'s first step is a whole-run `pread`; on a healthy disk it
/// succeeds and yields one good region covering the whole run with no bad
/// sectors — so the happy path flows through this same function (no
/// separate `Ok` variant).  On a failed read it recurses down to sector
/// granularity.
///
/// Takes ownership of the `ReadCmd` (whose `entries`/`dext` must be moved
/// into the resulting [`ReadRsp`]).  `phys` here is raw-rdev space (already
/// had `base_offset` added in the reader thread); `read_exact_at` is
/// positional so the offset is exact.
fn isolate_run_read(f: &std::fs::File, off: u64, cmd: ReadCmd) -> ReadRsp {
    let mut good: Vec<(usize, Vec<u8>)> = Vec::new();
    let mut bad: Vec<usize> = Vec::new();
    isolate_run(
        &mut |phys: u64, len: usize| pread_at(f, phys, len),
        off,
        0,
        cmd.len,
        cmd.sector_size,
        &mut good,
        &mut bad,
    );
    ReadRsp::Isolated {
        dext: cmd.dext,
        entries: cmd.entries,
        good,
        bad,
    }
}

/// Divide-and-conquer a failed read range into sector-aligned good regions
/// and the truly-bad (EIO) sector starts.
///
/// `phys`/`len` describe the byte range being probed (in whatever space the
/// caller's `read` closure expects — raw-rdev for the reader thread, array-
/// partition for the inline fallback).  `start` is the byte offset of this
/// sub-range **within the original run** (used to index `entries`); it is
/// what the caller maps back to per-sector `(logical, csum)` pairs.
///
/// - A successful read of a sub-range ≥ sector size that is a whole number
///   of sectors is a **good region** returned as `(start, bytes)`.  Every
///   sub-range this function recurses into is constructed sector-aligned on
///   both ends, so a successful read is always a valid good region (the
///   alignment check below is defensive).
/// - A failed read splits the range into [`EIO_SPLIT_FACTOR`] sector-aligned
///   pieces and recurses, down to a single sector.  A sector that still
///   fails is a **bad sector** (its `start` is recorded).
/// - `len == 0` is a no-op (defensive; the recursion only produces
///   non-empty sub-ranges).
///
/// Splitting by 8 instead of 2 cuts the number of *failing* reads needed to
/// pinpoint a bad sector in a full 64 MiB run from `log2(16384)` = 14 to
/// `log8(16384)` ≈ 5 — on a dying disk every failing read is extra strain,
/// while the extra successful reads on good sectors are cheap normal wear.
/// A range with fewer sectors than the factor splits into one piece per
/// sector (`EIO_SPLIT_FACTOR.min(len/sz)`), so small runs — the common case
/// — isolate in a single level.
///
/// Correctness guarantees:
/// - The pieces partition `[start, start + len)` **exactly** — no sub-range
///   is ever skipped or double-read, even when `len` is not divisible by
///   the factor (the remainder is spread across the leading pieces as
///   uneven, still sector-aligned splits).
/// - Every piece is ≥ 1 sector and strictly smaller than its parent, so the
///   recursion always terminates.
/// - Pieces are visited strictly left-to-right, so `good`/`bad` stay
///   ascending (which `process_isolated` relies on to slice `entries`).
///
/// This guarantees the bad set is exactly the physically-bad sectors and
/// the good set is everything else — no whole-run discard, no blast radius.
fn isolate_run(
    read: &mut impl FnMut(u64, usize) -> std::io::Result<Vec<u8>>,
    phys: u64,
    start: usize,
    len: usize,
    sector_size: u64,
    good: &mut Vec<(usize, Vec<u8>)>,
    bad: &mut Vec<usize>,
) {
    debug_assert!(
        start.is_multiple_of(sector_size as usize),
        "start not sector-aligned"
    );
    let sz = sector_size as usize;
    // `len` is always a whole number of sectors here: the top-level call
    // passes `run.len() * sector_size` and every recursive piece is
    // `piece_nsec * sector_size`.  Assert it so a future caller can't
    // silently lose a partial sector to the integer division below.
    debug_assert!(
        len.is_multiple_of(sz),
        "len {len} not a multiple of sector size {sz}"
    );
    if len == 0 {
        return;
    }
    // Base case: this range is exactly one sector.  Try to read it; a
    // failure is a true EIO sector.
    if len <= sz {
        match read(phys, len) {
            Ok(buf) => good.push((start, buf)),
            Err(_) => bad.push(start),
        }
        return;
    }
    // Try the whole range first — a transient failure that reads fine on
    // retry is simply good data (exactly what we want).
    if let Ok(buf) = read(phys, len) {
        // Only accept as a good region when sector-aligned on both ends.
        if start.is_multiple_of(sz) && len.is_multiple_of(sz) {
            good.push((start, buf));
            return;
        }
    }
    // Split into `k` sector-aligned pieces and recurse left-to-right so
    // `good`/`bad` stay ascending (see the guarantees above).  `k` is at
    // most `EIO_SPLIT_FACTOR` but never more than the range's sector count,
    // so a range too small to split into `K` splits into one piece per
    // sector instead.  A remainder (range not divisible by `k`) is spread
    // across the leading pieces as uneven splits — no bytes are lost.
    let nsec = len / sz; // ≥ 2 here (len > sz and aligned)
    let k = EIO_SPLIT_FACTOR.min(nsec);
    let base = nsec / k; // ≥ 1 since k ≤ nsec
    let rem = nsec % k;
    let mut off = 0usize;
    for i in 0..k {
        let piece_nsec = base + if i < rem { 1 } else { 0 };
        let piece_len = piece_nsec * sz;
        isolate_run(
            read,
            phys + off as u64,
            start + off,
            piece_len,
            sector_size,
            good,
            bad,
        );
        off += piece_len;
    }
}

/// Process the output of the EIO divide-and-conquer for a run.
///
/// `good` is a list of sector-aligned `(byte_start_within_run, bytes)`
/// regions that read fine; each is checksummed via [`process_buf`] (so the
/// coverage lost by the whole-run discard is restored — every good sector
/// is verified as usual).  `bad` is the list of `byte_start_within_run` of
/// the sectors that physically failed (EIO); each is counted as a read
/// error and, when a stored csum exists and `batch` mode is on, emitted as
/// an `unreadable` recovery candidate (see
/// `docs/EIO-robustness-design.md` §5.2).
#[allow(clippy::too_many_arguments)]
fn process_isolated(
    good: Vec<(usize, Vec<u8>)>,
    bad: Vec<usize>,
    dext: &DevExtent,
    entries: &[(u64, Vec<u8>)],
    strategy: &CsumStrategy,
    batch: bool,
    counters: Option<&StatusCounters>,
    reader: &mut FsReader,
    chunk_map: &ChunkMap,
    on_sector: &mut impl FnMut(&SectorResult),
    stats: &mut ScrubStats,
) {
    let sz = strategy.sector_size as usize;

    // 1. Re-verify every good region (restore the coverage the old whole-run
    //    discard lost).  Each good region is sector-aligned, so slice it
    //    into per-sector `(logical, csum)` entries for `process_buf`.
    for (start, buf) in &good {
        let first = start / sz;
        let last = (start + buf.len()) / sz;
        // Pass the region's csum entries as a slice directly — no clone.
        // On a healthy run `good` is one region covering the whole run, so
        // this is `&entries[..]`, byte-for-byte the old `Ok` arm.
        let region_entries = &entries[first..last.min(entries.len())];
        if region_entries.is_empty() {
            continue;
        }
        process_buf(
            buf,
            region_entries,
            dext,
            strategy,
            batch,
            counters,
            reader,
            chunk_map,
            on_sector,
            stats,
        );
    }

    // 2. Count + emit the truly-bad (EIO) sectors.  A sector with a stored
    //    csum is a recoverable candidate (the stored csum is what lets
    //    parity recovery verify its reconstruction); a sector without one
    //    is a plain read error.
    for &start in &bad {
        let idx = start / sz;
        if idx >= entries.len() {
            continue;
        }
        let (sector_logical, stored) = &entries[idx];
        stats.sectors_checked += 1;
        stats.bytes_checked += strategy.sector_size;
        stats.sectors_read_error += 1;
        if let Some(c) = counters {
            c.sectors_checked.fetch_add(1, Ordering::Relaxed);
            c.bytes_checked
                .fetch_add(strategy.sector_size, Ordering::Relaxed);
            c.sectors_read_error.fetch_add(1, Ordering::Relaxed);
        }
        let phys = dext.phys_start + (*sector_logical - dext.chunk_offset);
        eprintln!(
            "read error at phys 0x{:x} (devid {}, logical 0x{:x})",
            phys, dext.devid, *sector_logical
        );
        if batch && stored.len() == strategy.hash_len {
            on_sector(&SectorResult {
                logical: *sector_logical,
                devid: dext.devid,
                array_phys: phys,
                inode: 0,
                file_offset: 0,
                stored_csum: Some(stored.clone()),
                actual_csum: Vec::new(), // source bytes were unreadable
                unreadable: true,
                ok: false,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake read source over a `Vec<u8>` that returns EIO (errno 5) for
    /// any byte range overlapping `fault` — stands in for a block device
    /// with a bad sector, so the divide-and-conquer can be unit-tested
    /// without a real device or `dmsetup`.
    struct FaultyRead {
        data: Vec<u8>,
        fault: (usize, usize), // (start, len)
    }

    impl FaultyRead {
        fn read(&mut self, phys: u64, len: usize) -> std::io::Result<Vec<u8>> {
            let start = phys as usize;
            let end = start + len;
            let (fstart, flen) = self.fault;
            let fend = fstart.saturating_add(flen);
            if start < fend && end > fstart {
                return Err(std::io::Error::from_raw_os_error(5)); // EIO
            }
            Ok(self.data[start..end].to_vec())
        }
    }

    fn sector(fill: u8) -> Vec<u8> {
        vec![fill; 4096]
    }

    #[test]
    fn isolate_bad_sector_keeps_neighbours() {
        // 8 sectors; sector 3 (byte range 12288..16384) faults.  The whole
        // run's read fails, but divide-and-conquer must keep every other
        // sector as a good region and isolate exactly sector 3 as bad.
        let mut data = Vec::new();
        for i in 0..8u8 {
            data.extend_from_slice(&sector(i));
        }
        let mut r = FaultyRead {
            data,
            fault: (3 * 4096, 4096),
        };
        let mut good: Vec<(usize, Vec<u8>)> = Vec::new();
        let mut bad: Vec<usize> = Vec::new();
        isolate_run(&mut |p, l| r.read(p, l), 0, 0, 8 * 4096, 4096, &mut good, &mut bad);

        assert_eq!(bad, vec![3 * 4096], "only the faulted sector is bad");

        // Coverage: every non-faulted sector is inside exactly one good
        // region, and its bytes match the source.
        let mut covered = std::collections::BTreeSet::new();
        for (s, b) in &good {
            let first = s / 4096;
            let last = (s + b.len()) / 4096;
            for idx in first..last {
                covered.insert(idx);
            }
            // The region's bytes are contiguous from the source.
            for (off, byte) in b.iter().enumerate() {
                assert_eq!(*byte, ((s + off) / 4096) as u8, "region byte mismatch");
            }
        }
        for i in 0..8usize {
            if i == 3 {
                assert!(!covered.contains(&i), "sector 3 must be bad");
            } else {
                assert!(covered.contains(&i), "sector {i} must be covered");
            }
        }
    }

    #[test]
    fn isolate_fully_failing_range_reports_all_bad() {
        // Every sector faults -> all reported bad, none good.
        let data: Vec<u8> = vec![0u8; 4 * 4096];
        let mut r = FaultyRead {
            data,
            fault: (0, usize::MAX),
        };
        let mut good = Vec::new();
        let mut bad = Vec::new();
        isolate_run(&mut |p, l| r.read(p, l), 0, 0, 4 * 4096, 4096, &mut good, &mut bad);
        assert!(good.is_empty(), "no good regions when everything faults");
        assert_eq!(bad, vec![0, 4096, 2 * 4096, 3 * 4096]);
    }

    #[test]
    fn isolate_uneven_split_covers_every_sector() {
        // 10 sectors with a single fault at sector 6 — a range NOT divisible
        // by the 8-way split factor (10 % 8 == 2).  The remainder must be
        // spread across the leading pieces as uneven (still sector-aligned)
        // splits, and every non-faulted sector must be covered by exactly one
        // good region with the faulted one reported bad.  This is the
        // completeness guarantee: no subrange may be skipped or double-read.
        let mut data = Vec::new();
        for i in 0..10u8 {
            data.extend_from_slice(&sector(i));
        }
        let mut r = FaultyRead {
            data,
            fault: (6 * 4096, 4096),
        };
        let mut good: Vec<(usize, Vec<u8>)> = Vec::new();
        let mut bad: Vec<usize> = Vec::new();
        isolate_run(&mut |p, l| r.read(p, l), 0, 0, 10 * 4096, 4096, &mut good, &mut bad);

        assert_eq!(bad, vec![6 * 4096], "only the faulted sector is bad");

        let mut covered = std::collections::BTreeSet::new();
        for (s, b) in &good {
            assert!(
                s % 4096 == 0 && b.len() % 4096 == 0,
                "good region must be sector-aligned on both ends"
            );
            let first = s / 4096;
            let last = (s + b.len()) / 4096;
            for idx in first..last {
                assert!(covered.insert(idx), "sector {idx} covered twice");
            }
            // The region's bytes are contiguous from the source.
            for (off, byte) in b.iter().enumerate() {
                assert_eq!(*byte, ((s + off) / 4096) as u8, "region byte mismatch");
            }
        }
        for i in 0..10usize {
            if i == 6 {
                assert!(!covered.contains(&i), "sector 6 must be bad");
            } else {
                assert!(covered.contains(&i), "sector {i} must be covered");
            }
        }
    }

    #[test]
    fn isolate_small_range_smaller_than_split_factor() {
        // 3 sectors — fewer than the 8-way split factor.  The split must
        // clamp to one piece per sector (no zero-length pieces, no dropped
        // sectors) and still isolate the single bad one.
        let mut data = Vec::new();
        for i in 0..3u8 {
            data.extend_from_slice(&sector(i));
        }
        let mut r = FaultyRead {
            data,
            fault: (4096, 4096),
        };
        let mut good: Vec<(usize, Vec<u8>)> = Vec::new();
        let mut bad: Vec<usize> = Vec::new();
        isolate_run(&mut |p, l| r.read(p, l), 0, 0, 3 * 4096, 4096, &mut good, &mut bad);

        assert_eq!(bad, vec![4096], "only the faulted sector is bad");

        let mut covered = std::collections::BTreeSet::new();
        for (s, b) in &good {
            let first = s / 4096;
            let last = (s + b.len()) / 4096;
            for idx in first..last {
                covered.insert(idx);
            }
        }
        assert!(
            covered.contains(&0) && covered.contains(&2),
            "good sectors 0 and 2 must be covered"
        );
        assert!(!covered.contains(&1), "sector 1 must be bad");
    }
}
