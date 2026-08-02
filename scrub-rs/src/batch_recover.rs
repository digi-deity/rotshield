//! Batched, two-stage recovery pipeline.
//!
//! When scrub-rs runs with `--repair`, recovery is split into two
//! cooperating threads connected by single-depth channels, so the two
//! stages overlap and each provides backpressure to the one before it:
//!
//! ```text
//!   scrub thread ──(A, depth 1)──▶ accumulator ──(B, depth 1)──▶ writer
//!        reads disk          batches candidates        re-confirms +
//!        emits Candidate     (max N / idle X s)        recovers + writes
//! ```
//!
//! * **Accumulator** — receives raw csum-mismatch `Candidate`s from the
//!   scrub, buffers them into a batch, and forwards the whole batch to the
//!   writer.  It immediately starts filling the *next* batch.  Channel A is
//!   depth 1, so the scrub blocks (pauses) once one batch is buffered.
//! * **Writer** — receives a full batch, then re-confirms + recovers +
//!   writes every candidate in it under **one** filesystem freeze.  Channel
//!   B is depth 1, so the accumulator blocks (pauses) while the writer is
//!   busy — but the accumulator can be filling batch N+1 while the writer
//!   writes batch N.  That overlap is the whole point: the scrub and the
//!   write proceed concurrently instead of strictly alternating.
//!
//! ## Why batch + thread
//!
//! * **Fewer freeze cycles.** `FIFREEZE`/`FITHAW` flushes dirty pages and
//!   quiesces the mount — not free.  Batching does one freeze/thaw per
//!   burst instead of one per corruption.
//! * **Stricter reconfirmation.** Re-confirmation is *deferred* to write
//!   time, so it answers "is this *still* corrupt right before I write?"
//!   rather than "was it corrupt when I first scanned it?".  A block the
//!   live FS legitimately rewrote in the meantime is classified `Stale`
//!   and skipped.
//! * **Per-sector metadata trust.** Re-confirmation returns
//!   [`Reconfirm::Unverifiable`] only when the metadata node covering
//!   *that specific sector* could not be read.  We skip the write for just
//!   that candidate — we do NOT block unrelated writes because some other
//!   part of the tree was unreadable.  (An earlier design gated globally on
//!   `metadata_header_errors`; that was the wrong kind of gate and is gone.)
//!
//! ## Safety
//!
//! * The freeze wraps the *entire* batch's reconfirm + write + read-back
//!   verify + cache-invalidate window, so the live FS cannot mutate a block
//!   between our reconfirm and our write.  It is released by the
//!   `FreezeGuard` RAII drop at the end of the batch.
//! * The writer owns an **independent** [`Reconfirmer`] (a filesystem
//!   handle with its own file handles + chunk map, built via
//!   [`crate::fs::FilesystemScrub::reconfirmer`]), so it never borrows
//!   the main scrub's reader — the two threads touch disjoint file
//!   handles.  The main thread only *reads* the raw rdev; only the writer
//!   *writes*.
//! * Read-back verification: after writing each recovered block the writer
//!   re-reads it from the raw rdev and asserts the verifier accepts it; a
//!   mismatch is logged as a warning.  It then issues `BLKFLSBUF` on the
//!   raw rdev to drop the kernel's cached (possibly stale) view of that
//!   disk, so the next read through the live mount sees the fresh bytes.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::sync::Arc;
use std::time::Duration;

use crate::array::config::ArrayConfig;
use crate::array::stripe;
use crate::freeze::FreezeController;
use crate::fs::{Reconfirm, ReconfirmRequest, Reconfirmer, SectorVerifier};
use crate::recovery::{RecoveryInput, RecoveryResult, recover_block};
use crate::status::StatusCounters;

/// A single corruption candidate handed from the scrub thread to the
/// writer thread.  Carries everything the writer needs to re-confirm and
/// recover it without touching the scrub's reader.
#[derive(Clone)]
pub struct Candidate {
    /// Byte offset on the failing disk's array partition (`/dev/nmd1p1`
    /// space).  The array layer adds `rdevOffset` to reach raw-rdev space.
    pub array_phys: u64,
    /// Sector size in bytes.
    pub block_size: usize,
    /// Verifier closure: `true` iff a candidate block is the correct
    /// original data.  Used both for recovery verification and read-back.
    pub verify: SectorVerifier,
    /// Raw-rdev byte offset, for log lines only.
    pub raw_phys: u64,
    /// Raw-rdev path of the failing disk, for the write-back.
    pub failing_dev: PathBuf,
    /// Filesystem-opaque deferred re-confirmation request, handed back to
    /// the writer's [`Reconfirmer`] at write time to decide stale-vs-
    /// corrupt right before the write.
    pub reconfirm: Option<ReconfirmRequest>,
    /// The candidate's source bytes were **unreadable** (device `EIO`) —
    /// the scrub could not read the failing disk at all, so the self-heal
    /// fresh-read pre-check must be skipped (it would just `EIO` again)
    /// and recovery must use a zero placeholder for the corrupt block.
    pub unreadable: bool,
}

/// Producer→accumulator protocol (channel A, depth 1).
pub enum Msg {
    /// One corruption candidate.
    Candidate(Candidate),
    /// Producer is done; accumulator should flush any pending batch and exit.
    Done,
}

/// Accumulator→writer protocol (channel B, depth 1).
enum BatchMsg {
    /// One fully-buffered batch, ready to re-confirm + write.
    Batch(Vec<Candidate>),
    /// Accumulator is done; writer should exit after the current batch.
    Done,
}

/// Aggregate counters produced by the writer thread, merged into the
/// scrub's printed summary by `main`.
///
/// Per candidate the writer re-confirms against the live trees and then
/// lands in exactly one bucket:
///
/// * `mismatch` → re-confirmed as genuine corruption (the live csum still
///   disagrees with the stored one).  This is the count of *real* problems
///   found; everything below is a sub-outcome of a mismatch that did not
///   result in a successful write.
/// * `stale` → re-confirmation proved it was **never** genuine corruption:
///   the extent is a hole / unallocated / `nodatasum`, or the live csum no
///   longer matches the stored one (the live FS legitimately rewrote the
///   block since our scan).  Benign churn — nothing to fix, no write.
/// * `skipped` → the metadata node covering **this specific sector**
///   (EXTENT_TREE or CSUM_TREE) could not be read, so we cannot safely
///   re-confirm it.  We decline to write *just this candidate*
///   (`Reconfirm::Unverifiable`) rather than risk a wrong reconstruction.
///   This is per-sector, NOT a global gate — other candidates in the same
///   batch are unaffected.
/// * `recovered` → confirmed corruption that was successfully rebuilt from
///   parity and written (or would-be-written in dry-run), with a read-back
///   verify that the verifier accepted.
/// * `failed` → confirmed corruption where the fix did not land: the
///   failing disk's stripe could not be read, the parity stripe could not
///   be gathered, the write-back errored, or parity reconstruction produced
///   a block the verifier rejects (`RecoveryResult::Failed`).  Note: a
///   successful write whose post-write read-back *verify* disagreed is
///   logged as a WARNING but is NOT counted here (the write itself
///   succeeded).  `RecoveryResult::NotCorrupt` (parity already matches the
///   stored csum) also writes nothing and is counted nowhere.
#[derive(Debug, Default)]
pub struct BatchStats {
    /// Candidates confirmed (post re-confirm) as genuine corruption.
    pub mismatch: u64,
    /// Candidates re-confirmed as benign churn (freed/rewritten/nodatasum).
    pub stale: u64,
    /// Blocks successfully recovered and written (or would-be-written in
    /// dry-run).
    pub recovered: u64,
    /// Recovery attempts that failed (read/gather/write error or parity
    /// reconstruction the verifier rejected).
    pub failed: u64,
    /// Candidates whose re-confirmation could not read the metadata for
    /// *that specific sector* (`Reconfirm::Unverifiable`) — the write was
    /// skipped for just this candidate, not globally.
    pub skipped: u64,
}

/// Spawn the two-stage pipeline: an accumulator and a writer thread,
/// connected by single-depth channels.
///
/// Returns the sending half of channel A (for the scrub to push
/// candidates into) plus the two join handles.  The writer handle's `join`
/// yields the [`BatchStats`].  The writer takes ownership of `freeze` (so
/// the freeze lives entirely on its thread), a filesystem-owned
/// [`Reconfirmer`] (independent of the scrub's reader) for write-time
/// re-confirmation, and the array config.
#[allow(clippy::too_many_arguments)]
pub fn spawn_pipeline(
    cfg: ArrayConfig,
    freeze: FreezeController,
    reconfirmer: Box<dyn Reconfirmer>,
    scrub_slot: u64,
    dry_run: bool,
    batch_max: usize,
    batch_idle: Duration,
    counters: Option<Arc<StatusCounters>>,
) -> io::Result<(
    SyncSender<Msg>,
    std::thread::JoinHandle<()>,
    std::thread::JoinHandle<BatchStats>,
)> {
    // Channel A: scrub -> accumulator (depth 1 => scrub pauses when one
    // batch is buffered).  Channel B: accumulator -> writer (depth 1 =>
    // accumulator pauses while the writer is busy, but can fill the next
    // batch concurrently with the writer draining the current one).
    let (tx_a, rx_a) = mpsc::sync_channel(1);
    let (tx_b, rx_b) = mpsc::sync_channel(1);

    let acc_handle = std::thread::Builder::new()
        .name("scrub-accum".into())
        .spawn(move || run_accumulator(rx_a, tx_b, batch_max, batch_idle))
        .expect("spawn scrub-accumulator thread");

    let writer_handle = std::thread::Builder::new()
        .name("scrub-writer".into())
        .spawn(move || {
            run_writer(
                rx_b,
                cfg,
                freeze,
                reconfirmer,
                scrub_slot,
                dry_run,
                counters.as_deref(),
            )
        })
        .expect("spawn scrub-writer thread");

    Ok((tx_a, acc_handle, writer_handle))
}

/// Accumulator: buffer candidates into batches and forward each batch to
/// the writer.  Starts filling the next batch immediately after forwarding,
/// so batch N+1 accumulates while batch N is being written.
fn run_accumulator(
    rx: mpsc::Receiver<Msg>,
    tx_b: SyncSender<BatchMsg>,
    batch_max: usize,
    batch_idle: Duration,
) {
    let mut batch: Vec<Candidate> = Vec::with_capacity(batch_max);

    loop {
        // First candidate of a batch: block until one arrives (or Done).
        match rx.recv() {
            Ok(Msg::Candidate(c)) => batch.push(c),
            Ok(Msg::Done) => {
                if !batch.is_empty() {
                    let _ = tx_b.send(BatchMsg::Batch(std::mem::take(&mut batch)));
                }
                let _ = tx_b.send(BatchMsg::Done);
                return;
            }
            Err(_) => {
                // Producer gone.  Flush + exit.
                if !batch.is_empty() {
                    let _ = tx_b.send(BatchMsg::Batch(std::mem::take(&mut batch)));
                }
                let _ = tx_b.send(BatchMsg::Done);
                return;
            }
        }

        // Fill the batch up to batch_max, or until batch_idle elapses with
        // no new candidate (idle => forward the burst now).
        while batch.len() < batch_max {
            match rx.recv_timeout(batch_idle) {
                Ok(Msg::Candidate(c)) => batch.push(c),
                Ok(Msg::Done) => {
                    let _ = tx_b.send(BatchMsg::Batch(std::mem::take(&mut batch)));
                    let _ = tx_b.send(BatchMsg::Done);
                    return;
                }
                Err(RecvTimeoutError::Timeout) => break, // idle => forward burst
                Err(RecvTimeoutError::Disconnected) => {
                    let _ = tx_b.send(BatchMsg::Batch(std::mem::take(&mut batch)));
                    let _ = tx_b.send(BatchMsg::Done);
                    return;
                }
            }
        }

        // Forward the filled batch.  Blocks if the writer is still busy with
        // the previous one (backpressure) — but we then immediately start
        // filling the next batch, so the two stages overlap.
        let _ = tx_b.send(BatchMsg::Batch(std::mem::take(&mut batch)));
    }
}

/// Writer: receive one batch at a time, re-confirm + recover + write it
/// under a single freeze, then wait for the next batch.
#[allow(clippy::too_many_arguments)]
fn run_writer(
    rx: mpsc::Receiver<BatchMsg>,
    cfg: ArrayConfig,
    mut freeze: FreezeController,
    mut reconfirmer: Box<dyn Reconfirmer>,
    scrub_slot: u64,
    dry_run: bool,
    counters: Option<&StatusCounters>,
) -> BatchStats {
    let mut stats = BatchStats::default();

    while let Ok(BatchMsg::Batch(batch)) = rx.recv() {
        flush_batch(
            &batch,
            &cfg,
            &mut freeze,
            &mut reconfirmer,
            scrub_slot,
            dry_run,
            counters,
            &mut stats,
        );
    }

    stats
}

/// Re-confirm + recover + write one batch under a single freeze.
#[allow(clippy::too_many_arguments)]
fn flush_batch(
    batch: &[Candidate],
    cfg: &ArrayConfig,
    freeze: &mut FreezeController,
    reconfirmer: &mut Box<dyn Reconfirmer>,
    scrub_slot: u64,
    dry_run: bool,
    counters: Option<&StatusCounters>,
    stats: &mut BatchStats,
) {
    // Dedupe by array_phys: a DUP chunk yields two dev-extents with
    // *different* array_phys (two physical copies), which must BOTH be
    // recovered — so we keep distinct array_phys and only collapse a true
    // accidental duplicate of the same physical location.
    let mut batch = batch.to_vec();
    batch.sort_by_key(|a| a.array_phys);
    batch.dedup_by(|a, b| a.array_phys == b.array_phys);

    // One freeze for the whole batch's reconfirm + write + read-back.
    // Deliberately NOT per-candidate: toggling FIFREEZE/FITHAW on and off
    // for every sector in the batch would flicker the live filesystem
    // frozen/thawed dozens of times per batch, which is its own source of
    // stalls/latency spikes for anything doing I/O against the mount
    // during recovery. A single freeze held for the whole batch is a
    // bounded, predictable window (at most `batch_max` candidates' worth
    // of reconfirm + write), preferred over frequent on/off flapping.
    let _freeze_guard = freeze.guard();

    for cand in batch.iter() {
        // Re-confirm against the LIVE trees (deferred from scan time).
        // Per-sector metadata trust: if the metadata covering *this*
        // sector could not be read, we skip just this candidate — we do
        // NOT block the other candidates in the batch.
        let verdict = match &cand.reconfirm {
            Some(req) => reconfirmer.reconfirm(req),
            // No re-confirm request (no stored csum) — be conservative and
            // treat as real corruption (never hide a possible mismatch).
            None => Reconfirm::Corruption,
        };

        match verdict {
            Reconfirm::Stale => {
                stats.stale += 1;
                if let Some(c) = counters {
                    c.stale.fetch_add(1, Ordering::Relaxed);
                }
                continue;
            }
            Reconfirm::Unverifiable => {
                // Metadata for THIS sector unreadable — skip only this one.
                stats.skipped += 1;
                if let Some(c) = counters {
                    c.skipped.fetch_add(1, Ordering::Relaxed);
                }
                eprintln!(
                    "  [0x{:x}] SKIPPED (re-confirm metadata unreadable for this sector)",
                    cand.raw_phys
                );
                continue;
            }
            Reconfirm::Corruption => {}
        }

        // Obtain the "current bytes on the failing disk" for the recovery
        // engine.  For a NORMAL candidate this is a fresh read, used both
        // to close the transient-rewrite gap (if the live bytes now pass
        // the verifier, the sector self-healed — treat as stale, no write)
        // and as the `corrupt_block` input.  For an UNREADABLE candidate
        // (EIO) the disk cannot return bytes — skip the fresh read entirely
        // (it would just EIO again, see `docs/EIO-robustness-design.md`
        // §5.2) and use a zero placeholder, exactly as the canary does for
        // its P-only reconstruction.
        let unreadable = cand.unreadable;
        let corrupt_block: Vec<u8> = if unreadable {
            vec![0u8; cand.block_size]
        } else {
            let block = match stripe::read_block_or_zeros(
                cfg,
                &cand.failing_dev,
                cand.array_phys,
                cand.block_size,
            ) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("  [0x{:x}] read failing disk: {e}", cand.raw_phys);
                    stats.mismatch += 1;
                    stats.failed += 1;
                    if let Some(c) = counters {
                        c.mismatch.fetch_add(1, Ordering::Relaxed);
                        c.failed.fetch_add(1, Ordering::Relaxed);
                    }
                    continue;
                }
            };
            if (cand.verify)(&block) {
                // Self-healed since the scan/reconfirm read: the live bytes
                // now match the stored csum. Benign churn, not corruption.
                stats.stale += 1;
                if let Some(c) = counters {
                    c.stale.fetch_add(1, Ordering::Relaxed);
                }
                continue;
            }
            block
        };
        stats.mismatch += 1;
        if let Some(c) = counters {
            c.mismatch.fetch_add(1, Ordering::Relaxed);
        }

        let stripe_chunks =
            match stripe::gather_stripe(cfg, scrub_slot, cand.array_phys, cand.block_size) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("  [0x{:x}] gather stripe failed: {e}", cand.raw_phys);
                    stats.failed += 1;
                    if let Some(c) = counters {
                        c.failed.fetch_add(1, Ordering::Relaxed);
                    }
                    continue;
                }
            };
        let input = RecoveryInput {
            failing_slot: scrub_slot,
            corrupt_block: &corrupt_block,
            unreadable_source: unreadable,
            other_blocks: &stripe_chunks.other_data,
            p_block: stripe_chunks.p_block.as_deref(),
            q_block: stripe_chunks.q_block.as_deref(),
            verifier: &*cand.verify,
        };
        let result = recover_block(&input, cand.block_size);
        match &result {
            RecoveryResult::Recovered { via, block } => {
                let via_str = match via {
                    crate::recovery::ParityPath::P => "P".to_string(),
                    crate::recovery::ParityPath::Q => "Q".to_string(),
                    crate::recovery::ParityPath::PQ { partner_slot } => {
                        format!("PQ(partner=slot {partner_slot})")
                    }
                };
                let mut written = false;
                if !dry_run {
                    match stripe::write_block(cfg, &cand.failing_dev, cand.array_phys, block) {
                        Ok(()) => written = true,
                        Err(e) => {
                            eprintln!("  [0x{:x}] write back failed: {e}", cand.raw_phys);
                            stats.failed += 1;
                            if let Some(c) = counters {
                                c.failed.fetch_add(1, Ordering::Relaxed);
                            }
                            continue;
                        }
                    }
                    // Read-back verify: re-read and assert the verifier.
                    let reread = stripe::read_block_or_zeros(
                        cfg,
                        &cand.failing_dev,
                        cand.array_phys,
                        cand.block_size,
                    );
                    match reread {
                        Ok(b) if (cand.verify)(&b) => {}
                        Ok(_) => {
                            eprintln!(
                                "  [0x{:x}] WARNING: write read-back mismatch (verifier rejected)",
                                cand.raw_phys
                            );
                        }
                        Err(e) => {
                            eprintln!("  [0x{:x}] write read-back error: {e}", cand.raw_phys);
                        }
                    }
                    // Drop the kernel's cached view of this disk so the live
                    // mount reads fresh bytes (not the pre-write cache).
                    drop_cache(&cand.failing_dev);
                }
                eprintln!(
                    "  [0x{:x}] RECOVERED via {via_str} {} dev={}",
                    cand.raw_phys,
                    if written { "(written)" } else { "(dry-run)" },
                    cand.failing_dev.display(),
                );
                stats.recovered += 1;
                if let Some(c) = counters {
                    c.recovered.fetch_add(1, Ordering::Relaxed);
                }
            }
            RecoveryResult::NotCorrupt => {
                // Re-confirm said corruption, but parity reconstruction
                // produced a block the verifier rejects as the original —
                // or the block already matches.  Nothing to write.
                eprintln!(
                    "  [0x{:x}] not corrupt (matches stored csum)",
                    cand.raw_phys
                );
            }
            RecoveryResult::Failed { reason } => {
                eprintln!(
                    "  [0x{:x}] FAILED: {reason:?} dev={}",
                    cand.raw_phys,
                    cand.failing_dev.display(),
                );
                stats.failed += 1;
                if let Some(c) = counters {
                    c.failed.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    // `_freeze_guard` dropped here -> filesystem thawed.
}

/// Invalidate the kernel's cached view of a block device via `BLKFLSBUF`,
/// so a subsequent read through the live mount sees the bytes we just
/// wrote rather than a stale page-cache entry.  Best-effort: failures are
/// logged but non-fatal (the freeze already flushed dirty pages down).
fn drop_cache(dev: &Path) {
    use std::os::unix::io::AsRawFd;
    let f = match std::fs::File::open(dev) {
        Ok(f) => f,
        Err(_) => return,
    };
    // BLKFLSBUF = _IO(0x12, 97) = 0x0000_1261.
    const BLKFLSBUF: std::os::raw::c_ulong = 0x0000_1261;
    let _ = unsafe { crate::freeze::libc_ioctl(f.as_raw_fd(), BLKFLSBUF, 0) };
}
