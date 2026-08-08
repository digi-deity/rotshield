//! Physical-order data scrub: walk this device's dev-extents, stream their
//! checksums, read and verify every sector, isolating EIO regions by
//! divide-and-conquer.

use std::io;
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

// Runs of at least this many sectors compute checksums in parallel.
const PARALLEL_MIN_SECTORS: usize = 2;

// Upper bound on sectors per contiguous read run (bounds memory and
// reader-thread messages).
const MAX_RUN_SECTORS: usize = 16384;

// A failing read is split into this many sector-aligned pieces per level.
const EIO_SPLIT_FACTOR: usize = 8;

// Default isolation budget: failing reads tolerated before giving up.
const MAX_ISOLATION_FAILING_READS: usize = 64;

/// Budget for EIO isolation: how many failing reads to tolerate before
/// marking the remainder unreadable without probing.
struct IsolationBudget {
    remaining: usize,

    /// Set once the budget is spent; remaining sectors are marked bad.
    exhausted: bool,
}

impl IsolationBudget {
    fn new() -> Self {
        Self {
            remaining: isolation_budget_limit(),
            exhausted: false,
        }
    }
}

/// The budget, overridable via the ROTSHIELD_ISOLATION_BUDGET env var.
fn isolation_budget_limit() -> usize {
    use std::sync::OnceLock;
    static LIMIT: OnceLock<usize> = OnceLock::new();
    *LIMIT.get_or_init(|| {
        std::env::var("ROTSHIELD_ISOLATION_BUDGET")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(MAX_ISOLATION_FAILING_READS)
    })
}

// Pipeline depth (inflight reads) for fast checksums.
const PREFETCH_DEPTH_FAST: usize = 2;
// Deeper pipeline for slow checksums (sha256/blake2), where computing the
// checksums dominates the read time.
const PREFETCH_DEPTH_SLOW: usize = 4;

fn prefetch_depth_for(strategy: &CsumStrategy) -> usize {
    match strategy.name {
        "sha256" | "blake2" => PREFETCH_DEPTH_SLOW,

        _ => PREFETCH_DEPTH_FAST,
    }
}

/// One contiguous read command sent to the reader thread.
struct ReadCmd {
    phys: u64,
    len: usize,

    /// The dev-extent this run belongs to (maps logical offsets to physical
    /// reads and supplies the devid).
    dext: DevExtent,

    /// (logical, stored checksum) per sector of the run, in order.
    entries: Vec<(u64, Vec<u8>)>,

    sector_size: u64,
}

/// Reader-thread response: the run's sectors split into good regions and
/// failed sector offsets.
enum ReadRsp {
    Isolated {
        dext: DevExtent,
        entries: Vec<(u64, Vec<u8>)>,

        /// Sector-aligned regions that read successfully.
        good: Vec<(usize, Vec<u8>)>,

        /// Byte offsets of sectors that failed to read.
        bad: Vec<usize>,

        /// The isolation budget was spent; the remaining sectors were not
        /// probed.
        isolation_truncated: bool,
    },
}

/// One sector's verdict, emitted to the scrub driver.
#[derive(Debug)]
pub struct SectorResult {
    pub logical: u64,

    pub devid: u64,

    /// Physical offset on the device (array-partition space).
    pub array_phys: u64,
    pub inode: u64,
    pub file_offset: u64,

    /// The stored checksum, when the sector has one.
    pub stored_csum: Option<Vec<u8>>,

    pub actual_csum: Vec<u8>,

    /// The sector could not be read (EIO); `actual_csum` is empty.
    pub unreadable: bool,
    /// False for every emitted event — only problems are reported.
    pub ok: bool,
}

/// Per-run scrub accounting.
#[derive(Debug, Default)]
pub struct ScrubStats {
    pub sectors_checked: u64,
    pub sectors_ok: u64,
    pub sectors_mismatch: u64,
    pub sectors_no_csum: u64,
    pub sectors_read_error: u64,

    /// Mismatches that live metadata shows were rewritten or freed (non-batch
    /// mode only).
    pub sectors_stale: u64,

    /// CSUM_TREE branches skipped as stale mid-scrub (a coverage gap).
    pub stale_csum_branches: u64,

    /// Runs where the EIO isolation budget was exhausted.
    pub isolation_truncated: u64,
    pub bytes_checked: u64,

    /// Not written by the pipeline; the driver rolls up metadata errors itself.
    pub metadata_header_errors: u64,
}

/// Scrub every data dev-extent of this device: stream the extent's checksums,
/// read the bytes on a reader thread, verify, and emit a SectorResult per
/// problem. Falls back to synchronous inline reads when the reader thread
/// cannot be spawned.
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
) -> io::Result<ScrubStats>
where
    F: FnMut(&SectorResult),
{
    let mut stats = ScrubStats::default();
    let sector_size = strategy.sector_size;
    let sz = sector_size as usize;

    // The reader thread owns a cloned fd and resolves physical offsets
    // itself, so the main thread's seek position never races it.
    let base_offset = reader.base_offset();

    let prefetch_depth = prefetch_depth_for(strategy);
    // Duplicate the fd for the reader thread; on failure run inline.
    let reader_file = match reader.reopen() {
        Ok(f) => f,
        Err(e) => {
            eprintln!(
                "error: could not dup reader handle for reader thread: {e}; \
                 falling back to inline (non-pipelined) reads"
            );

            return Ok(scrub_dev_tree_inline(
                reader,
                chunk_map,
                csum_provider,
                dev_extents,
                strategy,
                batch,
                counters,
                on_sector,
            ));
        }
    };
    let (cmd_tx, cmd_rx) = mpsc::sync_channel::<ReadCmd>(prefetch_depth);
    let (rsp_tx, rsp_rx) = mpsc::sync_channel::<ReadRsp>(prefetch_depth);
    // Reader thread: hint read-ahead past the command, isolate any EIO
    // sectors, and reply with the results.
    let reader_handle = thread::Builder::new()
        .name("scrub-reader".into())
        .spawn(move || {
            let f = reader_file;

            // Read-ahead past the current command keeps the device busy
            // while the main thread verifies the previous run.
            const READAHEAD_CAP: u64 = 64 << 20;
            while let Ok(cmd) = cmd_rx.recv() {
                let off = base_offset.saturating_add(cmd.phys);

                let readahead = (cmd.len as u64).min(READAHEAD_CAP);
                crate::btrfs::reader::advise_willneed(
                    &f,
                    off.saturating_add(cmd.len as u64),
                    readahead,
                );

                // Split the read into good regions and failed sector offsets.
                let rsp = isolate_run_read(&f, off, cmd);

                if rsp_tx.send(rsp).is_err() {
                    break;
                }
            }
        })
        .expect("spawn scrub-reader thread");

    // At most prefetch_depth read commands are in flight at once.
    let mut inflight: usize = 0;

    // Set when the reader thread dies; remaining work is skipped.
    let pipeline_failed = std::cell::Cell::new(false);

    // Per dev-extent: gather its checksums into contiguous runs and feed
    // them to the pipeline.
    for (dev_extent_idx, dext) in dev_extents.iter().enumerate() {
        let chunk = match chunk_map.info(dext.chunk_offset) {
            Some(c) => c,
            // No chunk mapping for this dev extent — cannot map it to
            // data; log and skip the extent.
            None => {
                eprintln!(
                    "error: dev extent at phys 0x{:x} (devid {}) has no matching chunk item",
                    dext.phys_start, dext.devid
                );
                continue;
            }
        };

        let current_dev_extent_idx = dev_extent_idx;

        // Only data extents are scrubbed; metadata is walked by the
        // tree readers instead.
        if chunk.flags & bg_flag::DATA == 0 {
            continue;
        }

        let logical_lo = dext.chunk_offset;
        let logical_hi = dext.chunk_offset + dext.length;

        let next_idx = current_dev_extent_idx + 1;
        if next_idx < dev_extents.len() {
            let next = &dev_extents[next_idx];

            if let Some(next_chunk) = chunk_map.info(next.chunk_offset)
                && next_chunk.flags & bg_flag::DATA != 0
            {
                let hint_len = (next.length as usize)
                    .min(MAX_RUN_SECTORS * sz)
                    .max(strategy.sector_size as usize);
                reader.prefetch_logical(chunk_map, next.chunk_offset, hint_len);
            }
        }

        // Runs of contiguous sectors: each becomes one read command.
        let mut run: Vec<(u64, Vec<u8>)> = Vec::new();
        let mut prev_logical: Option<u64> = None;

        // Send a run to the reader thread, draining responses when the
        // pipeline is full.
        let mut flush = |run: &mut Vec<(u64, Vec<u8>)>| {
            if run.is_empty() || pipeline_failed.get() {
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

            if cmd_tx.send(cmd).is_err() {
                pipeline_failed.set(true);
                return;
            }
            inflight += 1;

            // Pipeline full: process the oldest response before sending more.
            if inflight >= prefetch_depth {
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
                    Err(_) => {
                        pipeline_failed.set(true);
                    }
                }
            }
        };

        // Break runs at gaps in logical order and at MAX_RUN_SECTORS.
        csum_provider.range(logical_lo, logical_hi, counters, |e| {
            if pipeline_failed.get() {
                return;
            }
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

        flush(&mut run);
        if pipeline_failed.get() {
            break;
        }

        if let Some(c) = counters {
            c.progress_done.fetch_add(dext.length, Ordering::Relaxed);
        }
    }

    // End of commands; drain the responses still in flight.
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
            Err(_) => {
                pipeline_failed.set(true);
                break;
            }
        }
    }

    // A panicked reader thread means incomplete results — not a clean scrub.
    if reader_handle.join().is_err() {
        eprintln!("error: scrub reader thread panicked — results are incomplete");
        pipeline_failed.set(true);
    }
    if pipeline_failed.get() {
        return Err(io::Error::other(
            "scrub pipeline failed: the reader thread died or responses were lost — \
             results are incomplete (not a clean scrub)",
        ));
    }

    stats.stale_csum_branches = csum_provider.stale_branches();
    Ok(stats)
}

/// Same scrub without the reader thread: reads and verifies synchronously
/// (fallback when the pipeline cannot be spawned).
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

            let mut good: Vec<(usize, Vec<u8>)> = Vec::new();
            let mut bad: Vec<usize> = Vec::new();
            let mut budget = IsolationBudget::new();
            {
                let mut read = |phys: u64, len: usize| reader.read_physical(dext.devid, phys, len);
                isolate_run(
                    &mut read,
                    run_phys,
                    0,
                    run_len,
                    sector_size,
                    &mut budget,
                    &mut good,
                    &mut bad,
                );
            }
            process_isolated(
                good,
                bad,
                budget.exhausted,
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
        csum_provider.range(logical_lo, logical_hi, counters, |e| {
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

        if let Some(c) = counters {
            c.progress_done.fetch_add(dext.length, Ordering::Relaxed);
        }
    }

    stats.stale_csum_branches = csum_provider.stale_branches();
    stats
}

/// Verify every sector of one read region against its stored checksum and
/// emit problems to the driver.
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

    // Compute checksums in parallel for large runs.
    let actuals: Vec<Vec<u8>> = if run.len() >= PARALLEL_MIN_SECTORS {
        let slices: Vec<(usize, usize)> = (0..run.len()).map(|i| (i * sz, (i + 1) * sz)).collect();
        slices
            .par_iter()
            .map(|&(s, e)| strategy.compute(&buf[s..e]))
            .collect()
    // Small run: compute checksums sequentially.
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
        // Batch mode: emit every mismatch raw; the recovery writer
        // re-confirms them against live metadata at write time.
        } else if batch {
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
            let tree_says_corrupt = match crate::btrfs::open::live_data_tree_roots(
                reader,
                chunk_map,
                reader.base_offset(),
                counters,
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
                None => {
                    stats.sectors_read_error += 1;
                    if let Some(c) = counters {
                        c.sectors_read_error.fetch_add(1, Ordering::Relaxed);
                    }
                    continue;
                }
            };

            let phys = dext.phys_start + (*sector_logical - dext.chunk_offset);
            // Fresh read: a sector that now verifies was rewritten in
            // between — stale, not corruption.
            let is_corruption = tree_says_corrupt
                && match reader.read_physical(dext.devid, phys, sz) {
                    Ok(fresh) => &strategy.compute(&fresh) != stored,

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
        isolation_truncated,
    } = rsp;
    process_isolated(
        good,
        bad,
        isolation_truncated,
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

/// Read a command's range on the reader thread and isolate any failing
/// sectors within it.
fn isolate_run_read(f: &std::fs::File, off: u64, cmd: ReadCmd) -> ReadRsp {
    let mut good: Vec<(usize, Vec<u8>)> = Vec::new();
    let mut bad: Vec<usize> = Vec::new();
    let mut budget = IsolationBudget::new();
    isolate_run(
        &mut |phys: u64, len: usize| pread_at(f, phys, len),
        off,
        0,
        cmd.len,
        cmd.sector_size,
        &mut budget,
        &mut good,
        &mut bad,
    );
    ReadRsp::Isolated {
        dext: cmd.dext,
        entries: cmd.entries,
        good,
        bad,

        isolation_truncated: budget.exhausted,
    }
}

/// Divide-and-conquer EIO isolation: try the whole range; on failure split
/// it into sector-aligned pieces and recurse until each failing sector is
/// identified. A sector-aligned region that reads cleanly is kept whole.
#[allow(clippy::too_many_arguments)]
fn isolate_run(
    read: &mut impl FnMut(u64, usize) -> std::io::Result<Vec<u8>>,
    phys: u64,
    start: usize,
    len: usize,
    sector_size: u64,
    budget: &mut IsolationBudget,
    good: &mut Vec<(usize, Vec<u8>)>,
    bad: &mut Vec<usize>,
) {
    debug_assert!(
        start.is_multiple_of(sector_size as usize),
        "start not sector-aligned"
    );
    let sz = sector_size as usize;

    debug_assert!(
        len.is_multiple_of(sz),
        "len {len} not a multiple of sector size {sz}"
    );
    if len == 0 {
        return;
    }

    // Budget spent: mark everything remaining bad without further reads.
    if budget.exhausted {
        let nsec = len / sz;
        for i in 0..nsec {
            bad.push(start + i * sz);
        }
        return;
    }

    // Single sector: one read decides good or bad.
    if len <= sz {
        // Clean read → good region; error → consume budget, mark sector bad.
        match read(phys, len) {
            Ok(buf) => good.push((start, buf)),
            Err(_) => {
                budget.remaining = budget.remaining.saturating_sub(1);
                if budget.remaining == 0 {
                    budget.exhausted = true;
                }
                bad.push(start);
            }
        }
        return;
    }

    match read(phys, len) {
        Ok(buf) => {
            if start.is_multiple_of(sz) && len.is_multiple_of(sz) {
                good.push((start, buf));
                return;
            }
        }
        Err(_) => {
            budget.remaining = budget.remaining.saturating_sub(1);
            if budget.remaining == 0 {
                budget.exhausted = true;
                let nsec = len / sz;
                for i in 0..nsec {
                    bad.push(start + i * sz);
                }
                return;
            }
        }
    }

    let nsec = len / sz;
    let k = EIO_SPLIT_FACTOR.min(nsec);
    let base = nsec / k;
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
            budget,
            good,
            bad,
        );
        off += piece_len;
    }
}

/// Verify the good regions of a run and report the bad (unreadable) sectors.
#[allow(clippy::too_many_arguments)]
fn process_isolated(
    good: Vec<(usize, Vec<u8>)>,
    bad: Vec<usize>,
    isolation_truncated: bool,
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

    // Budget exhausted: some sectors were never probed; report the run as
    // truncated so the summary explains the large read-error count.
    if isolation_truncated {
        stats.isolation_truncated += 1;
        if let Some(c) = counters {
            c.isolation_truncated.fetch_add(1, Ordering::Relaxed);
        }
        eprintln!(
            "note: EIO isolation budget exhausted for this run — remaining sectors \
             marked unreadable without further probing (device likely failing; \
             unreadable sectors are parity-recovered with verifier-gated writes)"
        );
    }

    // Verify each good region against its slice of the run's checksums.
    for (start, buf) in &good {
        let first = start / sz;
        let last = (start + buf.len()) / sz;

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

    // Unreadable sectors: count read errors; in batch mode also emit an
    // unreadable event so parity recovery can rebuild them.
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
        // Only sectors with a stored checksum can be parity-recovered.
        if batch && stored.len() == strategy.hash_len {
            on_sector(&SectorResult {
                logical: *sector_logical,
                devid: dext.devid,
                array_phys: phys,
                inode: 0,
                file_offset: 0,
                stored_csum: Some(stored.clone()),
                actual_csum: Vec::new(),
                unreadable: true,
                ok: false,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FaultyRead {
        data: Vec<u8>,
        fault: (usize, usize),
    }

    impl FaultyRead {
        fn read(&mut self, phys: u64, len: usize) -> std::io::Result<Vec<u8>> {
            let start = phys as usize;
            let end = start + len;
            let (fstart, flen) = self.fault;
            let fend = fstart.saturating_add(flen);
            if start < fend && end > fstart {
                return Err(std::io::Error::from_raw_os_error(5));
            }
            Ok(self.data[start..end].to_vec())
        }
    }

    fn sector(fill: u8) -> Vec<u8> {
        vec![fill; 4096]
    }

    #[test]
    fn isolate_bad_sector_keeps_neighbours() {
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
        let mut budget = IsolationBudget::new();
        isolate_run(
            &mut |p, l| r.read(p, l),
            0,
            0,
            8 * 4096,
            4096,
            &mut budget,
            &mut good,
            &mut bad,
        );

        assert_eq!(bad, vec![3 * 4096], "only the faulted sector is bad");

        let mut covered = std::collections::BTreeSet::new();
        for (s, b) in &good {
            let first = s / 4096;
            let last = (s + b.len()) / 4096;
            for idx in first..last {
                covered.insert(idx);
            }

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
        let data: Vec<u8> = vec![0u8; 4 * 4096];
        let mut r = FaultyRead {
            data,
            fault: (0, usize::MAX),
        };
        let mut good = Vec::new();
        let mut bad = Vec::new();
        let mut budget = IsolationBudget::new();
        isolate_run(
            &mut |p, l| r.read(p, l),
            0,
            0,
            4 * 4096,
            4096,
            &mut budget,
            &mut good,
            &mut bad,
        );
        assert!(good.is_empty(), "no good regions when everything faults");
        assert_eq!(bad, vec![0, 4096, 2 * 4096, 3 * 4096]);
    }

    #[test]
    fn isolate_uneven_split_covers_every_sector() {
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
        let mut budget = IsolationBudget::new();
        isolate_run(
            &mut |p, l| r.read(p, l),
            0,
            0,
            10 * 4096,
            4096,
            &mut budget,
            &mut good,
            &mut bad,
        );

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
        let mut budget = IsolationBudget::new();
        isolate_run(
            &mut |p, l| r.read(p, l),
            0,
            0,
            3 * 4096,
            4096,
            &mut budget,
            &mut good,
            &mut bad,
        );

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

    #[test]
    fn isolate_budget_exhaustion_marks_every_remaining_sector_bad_once() {
        let data: Vec<u8> = vec![0u8; 32 * 4096];
        let mut r = FaultyRead {
            data,
            fault: (0, usize::MAX),
        };
        let mut good: Vec<(usize, Vec<u8>)> = Vec::new();
        let mut bad: Vec<usize> = Vec::new();
        let mut budget = IsolationBudget {
            remaining: 4,
            exhausted: false,
        };
        isolate_run(
            &mut |p, l| r.read(p, l),
            0,
            0,
            32 * 4096,
            4096,
            &mut budget,
            &mut good,
            &mut bad,
        );

        assert!(budget.exhausted, "budget must be exhausted");
        assert!(good.is_empty(), "no good regions when everything faults");

        let mut seen = std::collections::BTreeSet::new();
        for &s in &bad {
            assert_eq!(s % 4096, 0, "bad start not sector-aligned: {s}");
            assert!(
                seen.insert(s / 4096),
                "sector {} marked bad twice",
                s / 4096
            );
        }
        assert_eq!(seen.len(), 32, "every sector must be covered exactly once");
    }

    #[test]
    fn isolate_budget_untouched_on_healthy_range() {
        let mut data = Vec::new();
        for i in 0..8u8 {
            data.extend_from_slice(&sector(i));
        }

        let mut r = FaultyRead {
            data,
            fault: (usize::MAX, 1),
        };
        let mut good: Vec<(usize, Vec<u8>)> = Vec::new();
        let mut bad: Vec<usize> = Vec::new();
        let mut budget = IsolationBudget {
            remaining: 4,
            exhausted: false,
        };
        isolate_run(
            &mut |p, l| r.read(p, l),
            0,
            0,
            8 * 4096,
            4096,
            &mut budget,
            &mut good,
            &mut bad,
        );

        assert!(!budget.exhausted, "healthy run must not exhaust the budget");
        assert_eq!(budget.remaining, 4, "no failing reads consumed");
        assert!(bad.is_empty(), "no bad sectors on a healthy run");

        let covered: usize = good.iter().map(|(_, b)| b.len()).sum();
        assert_eq!(covered, 8 * 4096, "all sectors covered as good");
    }
}
