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
//!   `FreezeGuard` RAII drop at the end of the batch — or EARLIER, when the
//!   wall-clock guard fires (H4): once the freeze has been held longer than
//!   `freeze_timeout`, the batch finishes only its current candidate and
//!   defers the remainder to the next batch, so a slow/dying disk inside a
//!   stripe gather can never stall the live filesystem indefinitely.
//! * A failed thaw after a batch (H2) is a hard abort: the filesystem may
//!   still be frozen, so no further batches may start.
//! * A disk that degrades DURING the run (a sector that read fine at scan
//!   time but EIOs at the writer's fresh read) is degraded to the
//!   unreadable recovery path (H10) instead of being declared failed: the
//!   sector is still reconstructed from parity and verified against its
//!   stored checksum before any write.
//! * The writer owns an **independent** [`Reconfirmer`] (a filesystem
//!   handle with its own file handles + chunk map, built via
//!   [`crate::fs::FilesystemScrub::reconfirmer`]), so it never borrows
//!   the main scrub's reader — the two threads touch disjoint file
//!   handles.  The main thread only *reads* the raw rdev; only the writer
//!   *writes*.
//! * Read-back verification: after writing each recovered block the writer
//!   first invalidates the raw rdev's buffer cache (`BLKFLSBUF`) and then
//!   re-reads the block from the device and asserts the verifier accepts it
//!   — so the verify reads the platter, not the page cache the write just
//!   dirtied.  A read-back that disagrees (or errors) is counted
//!   `readback_failed` (never `recovered`) and blocks exit code 4: it is
//!   the clearest "this disk is lying/failing" signal.  A second
//!   `BLKFLSBUF` after the read-back drops the rdev's cached (possibly
//!   stale) view of the disk so the next read through the live mount sees
//!   the fresh bytes (note: this does NOT clear the mounted filesystem's
//!   *file* page cache — see C5 in SERIOUS_ISSUES.md; repairs under a live
//!   mount print a reboot/remount advisory).
//!
//! * A **required freeze that fails** (`FIFREEZE` ioctl error) never
//!   degrades to unfrozen writes: that batch runs assess-only (every
//!   candidate is still re-confirmed and reconstruction-tested so the
//!   operator learns whether the corruption is repairable), but nothing is
//!   written and would-be writes are counted `not_frozen`.  Only
//!   `Ok(None)` from [`crate::freeze::FreezeController::guard`] (no live
//!   mount declared: offline image, dry-run, explicit `--no-freeze`)
//!   permits unfrozen writes.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
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
/// lands in exactly one bucket.  **Accounting invariant** (enforced by the
/// verdict reconciliation in `main.rs`): every candidate handed to
/// [`flush_batch`] is counted exactly once, with
///
/// ```text
/// sent == stale + skipped + mismatch + deduped
/// mismatch == recovered + failed + not_corrupt + not_frozen + readback_failed
/// ```
///
/// (`sent` is tracked by `main` at the channel; `deduped` covers
/// candidates collapsed by the per-batch `array_phys` dedup.)  A candidate
/// that self-heals at the fresh read counts `stale` only; everything that
/// reaches the recovery stage counts `mismatch` plus exactly one
/// sub-bucket.
///
/// * `mismatch` → re-confirmed as genuine corruption (the live csum still
///   disagrees with the stored one).  This is the count of *real* problems
///   found; everything below is a sub-outcome of a mismatch.
/// * `stale` → re-confirmation proved it was **never** genuine corruption:
///   the extent is a hole / unallocated / `nodatasum`, or the live csum no
///   longer matches the stored one (the live FS legitimately rewrote the
///   block since our scan).  Benign churn — nothing to fix, no write.
/// * `skipped` → the metadata node covering **this specific sector**
///   (EXTENT_TREE or CSUM_TREE, or the live tree roots) could not be read,
///   so we cannot safely re-confirm it.  We decline to write *just this
///   candidate* (`Reconfirm::Unverifiable`) rather than risk a wrong
///   reconstruction.  This is per-sector, NOT a global gate — other
///   candidates in the same batch are unaffected.
/// * `recovered` → confirmed corruption that was successfully rebuilt from
///   parity and written (or would-be-written in dry-run), with a read-back
///   verify that the verifier accepted.
/// * `failed` → confirmed corruption where the fix did not land: the
///   failing disk's stripe could not be read, the parity stripe could not
///   be gathered, the write-back errored, or parity reconstruction produced
///   a block the verifier rejects (`RecoveryResult::Failed`).
/// * `not_corrupt` → re-confirm said corruption, but parity reconstruction
///   produced a block the verifier rejects as the original — the block
///   already matches / is not what the stored csum describes.  Nothing to
///   write.  (Counted so the verdict sums reconcile.)
/// * `not_frozen` → reconstruction succeeded but the batch's REQUIRED
///   freeze failed, so the write was deferred (assess-only batch).  Never
///   counted `recovered`; blocks exit code 4.
/// * `readback_failed` → the write landed but the post-write read-back
///   disagreed with the verifier or errored.  A lying/failing disk signal;
///   never counted `recovered`; blocks exit code 4.
/// * `deduped` → candidates collapsed by the per-batch `array_phys` dedup
///   (accidental duplicates of the same physical location).
#[derive(Debug, Default)]
pub struct BatchStats {
    /// Candidates confirmed (post re-confirm) as genuine corruption.
    pub mismatch: u64,
    /// Candidates re-confirmed as benign churn (freed/rewritten/nodatasum).
    pub stale: u64,
    /// Blocks successfully recovered and written (or would-be-written in
    /// dry-run), with a passing read-back verify.
    pub recovered: u64,
    /// Recovery attempts that failed (read/gather/write error or parity
    /// reconstruction the verifier rejected).
    pub failed: u64,
    /// Candidates whose re-confirmation could not read the metadata for
    /// *that specific sector* (`Reconfirm::Unverifiable`) — the write was
    /// skipped for just this candidate, not globally.
    pub skipped: u64,
    /// Reconstruction succeeded but the batch's required freeze failed —
    /// the write was deferred (assess-only).  Blocks exit code 4.
    pub not_frozen: u64,
    /// Write landed but the post-write read-back disagreed or errored.
    /// Blocks exit code 4.
    pub readback_failed: u64,
    /// Reconstructed block rejected by the verifier as "not the original"
    /// (`RecoveryResult::NotCorrupt`) — nothing to write.
    pub not_corrupt: u64,
    /// Candidates collapsed by the per-batch `array_phys` dedup.
    pub deduped: u64,
}

/// Writer-thread join result: `Ok(BatchStats)` on a clean finish, `Err` on
/// a HARD abort (H2: a thaw failed and the filesystem may still be frozen).
/// `main` turns the `Err` into a hard runtime error (exit 1, state=error).
pub type WriterResult = io::Result<BatchStats>;

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
    freeze_timeout: Duration,
    counters: Option<Arc<StatusCounters>>,
) -> io::Result<(
    SyncSender<Msg>,
    std::thread::JoinHandle<()>,
    std::thread::JoinHandle<WriterResult>,
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
                freeze_timeout,
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
                if !batch.is_empty()
                    && tx_b
                        .send(BatchMsg::Batch(std::mem::take(&mut batch)))
                        .is_err()
                {
                    return; // writer is gone — stop buffering, don't drain into a dead channel
                }
                let _ = tx_b.send(BatchMsg::Done);
                return;
            }
            Err(_) => {
                // Producer gone.  Flush + exit.
                if !batch.is_empty()
                    && tx_b
                        .send(BatchMsg::Batch(std::mem::take(&mut batch)))
                        .is_err()
                {
                    return;
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
                    if !batch.is_empty()
                        && tx_b
                            .send(BatchMsg::Batch(std::mem::take(&mut batch)))
                            .is_err()
                    {
                        return;
                    }
                    let _ = tx_b.send(BatchMsg::Done);
                    return;
                }
                Err(RecvTimeoutError::Timeout) => break, // idle => forward burst
                Err(RecvTimeoutError::Disconnected) => {
                    if !batch.is_empty()
                        && tx_b
                            .send(BatchMsg::Batch(std::mem::take(&mut batch)))
                            .is_err()
                    {
                        return;
                    }
                    let _ = tx_b.send(BatchMsg::Done);
                    return;
                }
            }
        }

        // Forward the filled batch.  Blocks if the writer is still busy with
        // the previous one (backpressure) — but we then immediately start
        // filling the next batch, so the two stages overlap.  If the writer
        // is dead the send fails: stop immediately rather than draining the
        // whole scrub into a dead channel (the writer's panic is surfaced
        // by `main`'s join check, which turns the whole run into a hard
        // error).
        if tx_b
            .send(BatchMsg::Batch(std::mem::take(&mut batch)))
            .is_err()
        {
            return;
        }
    }
}

/// Writer: receive one batch at a time, re-confirm + recover + write it
/// under a single freeze, then wait for the next batch.
///
/// Returns an `Err` on a hard abort: a thaw failed after a batch (H2).
/// `main` treats that as a hard runtime error — the verdict is incomplete
/// and must never be reported as a clean/partial run.
#[allow(clippy::too_many_arguments)]
fn run_writer(
    rx: mpsc::Receiver<BatchMsg>,
    cfg: ArrayConfig,
    mut freeze: FreezeController,
    mut reconfirmer: Box<dyn Reconfirmer>,
    scrub_slot: u64,
    dry_run: bool,
    freeze_timeout: Duration,
    counters: Option<&StatusCounters>,
) -> io::Result<BatchStats> {
    let mut stats = BatchStats::default();
    // Candidates deferred by a freeze-timeout in the previous flush (H4).
    // They were already counted `sent` by `main` and must still be
    // classified exactly once, so they head the next batch.  Flushing is
    // drained on `Done` too, so a deferral can never lose candidates.
    let mut pending: Vec<Candidate> = Vec::new();

    loop {
        match rx.recv() {
            Ok(BatchMsg::Batch(batch)) => {
                pending.extend(batch);
                if let Err(e) = flush_pending(
                    &mut pending,
                    &cfg,
                    &mut freeze,
                    &mut reconfirmer,
                    scrub_slot,
                    dry_run,
                    freeze_timeout,
                    counters,
                    &mut stats,
                ) {
                    eprintln!("error: recovery pipeline aborted: {e}");
                    return Err(e);
                }
            }
            Ok(BatchMsg::Done) | Err(_) => {
                // Producer done/gone: drain any freeze-deferred remainder.
                // Each flush classifies at least one candidate (the
                // freeze-timeout only defers from the second candidate
                // on), so this always terminates.
                while !pending.is_empty() {
                    if let Err(e) = flush_pending(
                        &mut pending,
                        &cfg,
                        &mut freeze,
                        &mut reconfirmer,
                        scrub_slot,
                        dry_run,
                        freeze_timeout,
                        counters,
                        &mut stats,
                    ) {
                        eprintln!("error: recovery pipeline aborted: {e}");
                        return Err(e);
                    }
                }
                return Ok(stats);
            }
        }
    }
}

/// Flush the writer's pending queue through [`flush_batch`], replacing it
/// with whatever the freeze-timeout deferred to the next flush.
#[allow(clippy::too_many_arguments)]
fn flush_pending(
    pending: &mut Vec<Candidate>,
    cfg: &ArrayConfig,
    freeze: &mut FreezeController,
    reconfirmer: &mut Box<dyn Reconfirmer>,
    scrub_slot: u64,
    dry_run: bool,
    freeze_timeout: Duration,
    counters: Option<&StatusCounters>,
    stats: &mut BatchStats,
) -> io::Result<()> {
    let deferred = flush_batch(
        pending,
        cfg,
        freeze,
        reconfirmer,
        scrub_slot,
        dry_run,
        freeze_timeout,
        counters,
        stats,
    )?;
    *pending = deferred;
    Ok(())
}

/// Re-confirm + recover + write one batch under a single freeze.
///
/// Returns `Ok(deferred)` with the candidates a freeze-timeout deferred to
/// the next batch (empty on a normal flush), or `Err` on a hard abort: a
/// thaw failed after the batch (H2) — the run must stop and never start
/// further batches while the filesystem may still be frozen.
#[allow(clippy::too_many_arguments)]
fn flush_batch(
    batch: &[Candidate],
    cfg: &ArrayConfig,
    freeze: &mut FreezeController,
    reconfirmer: &mut Box<dyn Reconfirmer>,
    scrub_slot: u64,
    dry_run: bool,
    freeze_timeout: Duration,
    counters: Option<&StatusCounters>,
    stats: &mut BatchStats,
) -> io::Result<Vec<Candidate>> {
    // Dedupe by array_phys: a DUP chunk yields two dev-extents with
    // *different* array_phys (two physical copies), which must BOTH be
    // recovered — so we keep distinct array_phys and only collapse a true
    // accidental duplicate of the same physical location.  Deduped
    // candidates are counted so the sent-vs-classified verdict
    // reconciliation in `main` still sums exactly (see the invariant on
    // [`BatchStats`]).
    let mut batch = batch.to_vec();
    batch.sort_by_key(|a| a.array_phys);
    let pre_dedup = batch.len();
    batch.dedup_by(|a, b| a.array_phys == b.array_phys);
    let deduped = (pre_dedup - batch.len()) as u64;
    stats.deduped += deduped;
    if let Some(c) = counters {
        c.deduped.fetch_add(deduped, Ordering::Relaxed);
    }

    // One freeze for the whole batch's reconfirm + write + read-back.
    // Deliberately NOT per-candidate: toggling FIFREEZE/FITHAW on and off
    // for every sector in the batch would flicker the live filesystem
    // frozen/thawed dozens of times per batch, which is its own source of
    // stalls/latency spikes for anything doing I/O against the mount
    // during recovery. A single freeze held for the whole batch is a
    // bounded, predictable window (at most `batch_max` candidates' worth
    // of reconfirm + write), preferred over frequent on/off flapping.
    //
    // A REQUIRED freeze that FAILS must never degrade to unfrozen writes.
    // `guard()` distinguishes three outcomes:
    //   Ok(Some(_)) — frozen; normal repair-write path.
    //   Ok(None)    — no live mount declared (offline image, dry-run,
    //                 explicit --no-freeze): unfrozen writes are the
    //                 caller's explicit choice.
    //   Err(e)      — a live mount WAS declared but FIFREEZE failed: run
    //                 this batch assess-only (classify every candidate,
    //                 write NOTHING, count would-be writes `not_frozen`).
    //                 Subsequent batches keep retrying the freeze.
    // Whether a live mount is declared — captured BEFORE the guard borrows
    // `freeze` mutably (a `FreezeGuard` holds `&mut FreezeController`, so
    // `freeze.has_live_mount()` cannot be called while it is alive).
    let has_live_mount = freeze.has_live_mount();
    let freeze_guard = match freeze.guard() {
        Ok(g) => g,
        Err(e) => {
            eprintln!(
                "ERROR: could not freeze the live filesystem for this batch ({e}). \
                 The filesystem is NOT frozen, so NO writes will be issued for this batch \
                 (assess-only; would-be writes counted as not_frozen). Later batches retry \
                 the freeze — nothing is ever written unfrozen unless --no-freeze is set \
                 explicitly."
            );
            None
        }
    };
    // Writes are allowed only when the freeze succeeded (guard present) or
    // was never required (no declared live mount).  A failed freeze means
    // `has_live_mount()` is true and the guard is None → writes blocked.
    let writes_allowed = freeze_guard.is_some() || !has_live_mount;

    // H4: wall-clock guard on the batch.  The freeze window spans every
    // candidate's reconfirm + fresh read + stripe gather across ALL other
    // disks + write + read-back; a slow/dying disk inside the gather must
    // not stall the live filesystem indefinitely.  Once the window exceeds
    // `freeze_timeout` we finish only the CURRENT candidate and defer the
    // rest to the next batch (the filesystem thaws here; `run_writer`
    // re-queues the deferred tail).  The first candidate is always
    // attempted so every flush makes progress — a batch can never be
    // deferred in a loop.
    let freeze_started = std::time::Instant::now();
    let mut deferred: Vec<Candidate> = Vec::new();

    for (i, cand) in batch.iter().enumerate() {
        if i > 0 && writes_allowed && freeze_started.elapsed() > freeze_timeout {
            deferred.extend_from_slice(&batch[i..]);
            eprintln!(
                "[freeze-timeout] batch window exceeded {}s — deferring {} candidate(s) to \
                 the next batch{}",
                freeze_timeout.as_secs(),
                batch.len() - i,
                if freeze_guard.is_some() {
                    " (filesystem thawed)"
                } else {
                    ""
                }
            );
            break;
        }
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
        let mut unreadable = cand.unreadable;
        let corrupt_block: Vec<u8> = if unreadable {
            vec![0u8; cand.block_size]
        } else {
            match stripe::read_block_or_zeros(
                cfg,
                &cand.failing_dev,
                cand.array_phys,
                cand.block_size,
            ) {
                Ok(block) => {
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
                }
                Err(e) => {
                    // H10: the disk degraded DURING the run — this sector read
                    // fine at scan time but the writer's fresh read now EIOs.
                    // Do NOT give up on the sector: degrade it to the
                    // unreadable path (zero placeholder + skip-self-heal +
                    // parity reconstruction verified against the stored csum),
                    // exactly what a scan-time-EIO candidate gets.  The engine
                    // knows `unreadable_source` and skips the checks that
                    // would compare against a bogus corrupt block.
                    eprintln!(
                        "  [0x{:x}] read failing disk: {e} — disk degraded mid-run; \
                         degrading to the unreadable recovery path",
                        cand.raw_phys
                    );
                    unreadable = true;
                    vec![0u8; cand.block_size]
                }
            }
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
                // Dry-run: the reconstruction is assessed but never
                // written — classified `recovered` (would-be-written).
                // Freeze-failed batch: the reconstruction is assessed but
                // the write is DEFERRED — classified `not_frozen`, never
                // `recovered` (exit 4 stays unreachable).
                if dry_run || !writes_allowed {
                    if writes_allowed {
                        eprintln!(
                            "  [0x{:x}] RECOVERED via {via_str} (dry-run) dev={}",
                            cand.raw_phys,
                            cand.failing_dev.display(),
                        );
                        stats.recovered += 1;
                        if let Some(c) = counters {
                            c.recovered.fetch_add(1, Ordering::Relaxed);
                        }
                    } else {
                        eprintln!(
                            "  [0x{:x}] RECOVERED via {via_str} but NOT WRITTEN (batch had no \
                             freeze — deferred; assess-only) dev={}",
                            cand.raw_phys,
                            cand.failing_dev.display(),
                        );
                        stats.not_frozen += 1;
                        if let Some(c) = counters {
                            c.not_frozen.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    continue;
                }
                // Real write path (freeze held, or no live mount declared).
                match stripe::write_block(cfg, &cand.failing_dev, cand.array_phys, block) {
                    Ok(()) => {
                        // C5: remember that a repair landed under a declared
                        // live mount, so `main` can print the page-cache
                        // staleness advisory (BLKFLSBUF on the raw rdev does
                        // NOT clear the mounted filesystem's FILE page
                        // cache).
                        if let Some(c) = counters
                            && has_live_mount
                        {
                            c.repaired_while_mounted.store(1, Ordering::Relaxed);
                        }
                    }
                    Err(e) => {
                        eprintln!("  [0x{:x}] write back failed: {e}", cand.raw_phys);
                        stats.failed += 1;
                        if let Some(c) = counters {
                            c.failed.fetch_add(1, Ordering::Relaxed);
                        }
                        continue;
                    }
                }
                // C4: invalidate the raw rdev's buffer cache BEFORE the
                // read-back, so the verify reads the device — not the page
                // cache the write just dirtied.  A read-back served from
                // cache proves nothing about the platter.
                match drop_cache(&cand.failing_dev) {
                    Ok(()) => {}
                    Err(e) => {
                        // If the invalidation failed on a real BLOCK
                        // device, the upcoming read-back could be served
                        // from the page cache the write just dirtied — the
                        // verify would prove nothing about the platter, so
                        // it must NOT be trusted as "recovered" (C4 review:
                        // a verify we cannot trust is a failed verify).
                        // On a regular-FILE image ENOTTY is expected and
                        // harmless: the file's page cache is the ground
                        // truth there (the write was fsynced), so proceed.
                        use std::os::unix::fs::FileTypeExt;
                        let is_block = std::fs::metadata(&cand.failing_dev)
                            .map(|m| m.file_type().is_block_device())
                            .unwrap_or(false);
                        if is_block {
                            eprintln!(
                                "  [0x{:x}] READ-BACK VERIFY NOT TRUSTED: could not \
                                 invalidate the raw rdev cache before read-back ({e}) — the \
                                 verify would read the page cache, not the platter. Counted \
                                 FAILED (readback_failed) instead of verified.",
                                cand.raw_phys
                            );
                            stats.readback_failed += 1;
                            if let Some(c) = counters {
                                c.readback_failed.fetch_add(1, Ordering::Relaxed);
                            }
                            // Best-effort post-write invalidation for the
                            // live mount (C5), then move on.
                            let _ = drop_cache(&cand.failing_dev);
                            continue;
                        }
                        eprintln!(
                            "  [0x{:x}] note: BLKFLSBUF unavailable on this image ({e}); \
                             read-back reads the image's page cache, which is authoritative \
                             after fsync",
                            cand.raw_phys
                        );
                    }
                }
                let reread = stripe::read_block_or_zeros(
                    cfg,
                    &cand.failing_dev,
                    cand.array_phys,
                    cand.block_size,
                );
                match reread {
                    Ok(b) if (cand.verify)(&b) => {
                        eprintln!(
                            "  [0x{:x}] RECOVERED via {via_str} (written + read-back verified) dev={}",
                            cand.raw_phys,
                            cand.failing_dev.display(),
                        );
                        stats.recovered += 1;
                        if let Some(c) = counters {
                            c.recovered.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Ok(_) => {
                        // The write landed but the device returns bytes that
                        // fail the verifier — the clearest "this disk is
                        // lying / failing" signal.  Counted readback_failed
                        // (never recovered), blocks exit code 4.
                        eprintln!(
                            "  [0x{:x}] READ-BACK MISMATCH: device returned bytes that fail \
                             the verifier after a successful write (lying/failing disk?) — \
                             counted FAILED (readback_failed)",
                            cand.raw_phys
                        );
                        stats.readback_failed += 1;
                        if let Some(c) = counters {
                            c.readback_failed.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Err(e) => {
                        eprintln!(
                            "  [0x{:x}] read-back error after write: {e} — counted FAILED \
                             (readback_failed)",
                            cand.raw_phys
                        );
                        stats.readback_failed += 1;
                        if let Some(c) = counters {
                            c.readback_failed.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                // C4/C5: invalidate again after the read-back so a
                // subsequent read through the live mount sees the fresh
                // bytes (this only clears the raw-rdev buffer cache — the
                // mounted filesystem's FILE page cache is C5's problem).
                // Best-effort: a failure here cannot invalidate the verify
                // (already done) and only affects cache freshness.
                let _ = drop_cache(&cand.failing_dev);
            }
            RecoveryResult::NotCorrupt => {
                // Re-confirm said corruption, but parity reconstruction
                // produced a block the verifier rejects as the original —
                // or the block already matches.  Nothing to write.
                eprintln!(
                    "  [0x{:x}] not corrupt (matches stored csum)",
                    cand.raw_phys
                );
                stats.not_corrupt += 1;
                if let Some(c) = counters {
                    c.not_corrupt.fetch_add(1, Ordering::Relaxed);
                }
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

    // Explicitly drop the freeze guard here so the thaw completes before
    // we check whether it FAILED (H2): a failed thaw means the filesystem
    // may STILL be frozen.  That is strictly worse than the corruption
    // being repaired — every writer on the mount stalls — so the run must
    // not start further batches: abort, and surface the manual recovery
    // command (`fsfreeze -u <mountpoint>`).
    drop(freeze_guard);
    if let Some(e) = freeze.take_thaw_error() {
        let mnt = freeze
            .mountpoint()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<--freeze-mount path>".to_string());
        return Err(io::Error::other(format!(
            "failed to thaw the live filesystem after this batch ({e}) — it may STILL \
             BE FROZEN. Thaw it manually with `fsfreeze -u {mnt}`, then re-run. Aborting \
             the run: no further batches will start while the filesystem may be frozen."
        )));
    }
    Ok(deferred)
}

/// Invalidate the kernel's cached view of a block device via `BLKFLSBUF`.
/// Called TWICE around a repair write: once BEFORE the read-back so the
/// verify reads the device rather than the page cache the write just
/// dirtied (C4), and once AFTER so a subsequent read through the live
/// mount sees the fresh bytes rather than a stale page-cache entry.
///
/// Returns `Err` when the device cannot be opened or the ioctl fails
/// (e.g. `ENOTTY` on a regular-file image).  The caller decides whether a
/// failure is acceptable: for a block device the pre-read-back
/// invalidation is REQUIRED for the verify to mean anything; for a file
/// image `ENOTTY` is expected and harmless.
fn drop_cache(dev: &Path) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let f = std::fs::File::open(dev)?;
    // BLKFLSBUF = _IO(0x12, 97) = 0x0000_1261.
    const BLKFLSBUF: std::os::raw::c_ulong = 0x0000_1261;
    let rc = unsafe { crate::freeze::libc_ioctl(f.as_raw_fd(), BLKFLSBUF, 0) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::array::config::ArrayConfig;
    use crate::fs::{Reconfirm, ReconfirmRequest};
    use std::collections::BTreeMap;
    use std::fs;
    use std::io::Write;
    use std::path::PathBuf;

    const BLOCK: usize = 16;

    /// Reconfirmer stub: returns `Reconfirm::Corruption` for every token
    /// except the ones mapped to `Stale` / `Unverifiable` (so one batch can
    /// exercise every verdict in a single run).
    struct VerdictStub {
        stale_tokens: Vec<u64>,
        unverifiable_tokens: Vec<u64>,
    }
    impl Reconfirmer for VerdictStub {
        fn reconfirm(&mut self, req: &ReconfirmRequest) -> Reconfirm {
            if self.stale_tokens.contains(&req.token) {
                Reconfirm::Stale
            } else if self.unverifiable_tokens.contains(&req.token) {
                Reconfirm::Unverifiable
            } else {
                Reconfirm::Corruption
            }
        }
    }

    fn make_disk(dir: &tempfile::TempDir, name: &str, fill: u8) -> PathBuf {
        let path = dir.path().join(name);
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(&[fill; BLOCK * 8]).unwrap();
        f.sync_all().unwrap();
        path
    }

    /// Three-file "array": the failing disk d1 (slot 1) carries 0x11 on
    /// disk, but the ORIGINAL data — what the verifier accepts — is 0x44.
    /// d2 (slot 2) is 0x22; P is 0x44 ^ 0x22 = 0x66, so P-recovery
    /// reconstructs 0x44 and the verifier accepts it.
    fn cfg(dir: &tempfile::TempDir) -> (ArrayConfig, PathBuf) {
        let d1 = make_disk(dir, "d1", 0x11);
        let d2 = make_disk(dir, "d2", 0x22);
        let p = make_disk(dir, "p", 0x66);
        let cfg = ArrayConfig {
            data_devs: BTreeMap::from([(1, d1.clone()), (2, d2)]),
            parity_p: Some(p),
            parity_q: None,
            rdev_offsets: BTreeMap::new(),
        };
        (cfg, d1)
    }

    fn cand(failing_dev: PathBuf, array_phys: u64, token: u64, unreadable: bool) -> Candidate {
        let verify: SectorVerifier = Arc::new(|b: &[u8]| b == &vec![0x44u8; BLOCK][..]);
        Candidate {
            array_phys,
            block_size: BLOCK,
            verify,
            raw_phys: array_phys,
            failing_dev,
            reconfirm: Some(ReconfirmRequest {
                token,
                stored_csum: vec![],
            }),
            unreadable,
        }
    }

    fn run_batch(
        cfg: &ArrayConfig,
        freeze: FreezeController,
        dry_run: bool,
        cands: Vec<Candidate>,
        stub: VerdictStub,
    ) -> BatchStats {
        let mut reconfirmer: Box<dyn Reconfirmer> = Box::new(stub);
        let mut stats = BatchStats::default();
        let mut freeze = freeze;
        let deferred = flush_batch(
            &cands,
            cfg,
            &mut freeze,
            &mut reconfirmer,
            1,
            dry_run,
            Duration::from_secs(60),
            None,
            &mut stats,
        )
        .expect("batch must not abort");
        assert!(
            deferred.is_empty(),
            "no freeze-timeout deferral expected in this test"
        );
        stats
    }

    #[test]
    fn freeze_failure_defers_writes_into_not_frozen() {
        // C2: a REQUIRED freeze that fails must never degrade to unfrozen
        // writes — the batch runs assess-only; would-be writes land in
        // `not_frozen` (never `recovered`) and the disk stays untouched.
        let dir = tempfile::tempdir().unwrap();
        let (cfg, d1) = cfg(&dir);
        let before = fs::read(&d1).unwrap();
        let freeze = FreezeController::new(Some(PathBuf::from("/nonexistent/mountpoint")));
        let stats = run_batch(
            &cfg,
            freeze,
            false,
            vec![cand(d1.clone(), 0, 0, false)],
            VerdictStub {
                stale_tokens: vec![],
                unverifiable_tokens: vec![],
            },
        );
        assert_eq!(stats.not_frozen, 1, "write must be deferred");
        assert_eq!(stats.recovered, 0, "deferred writes are never recovered");
        assert_eq!(stats.failed, 0);
        assert_eq!(stats.mismatch, 1);
        assert_eq!(
            fs::read(&d1).unwrap(),
            before,
            "assess-only batch must not touch the disk"
        );
    }

    #[test]
    fn no_declared_mount_writes_normally() {
        // C2 Ok(None): no live mount declared (offline image / --no-freeze)
        // — unfrozen writes are the caller's explicit choice, unchanged.
        let dir = tempfile::tempdir().unwrap();
        let (cfg, d1) = cfg(&dir);
        let freeze = FreezeController::new(None);
        let stats = run_batch(
            &cfg,
            freeze,
            false,
            vec![cand(d1.clone(), 0, 0, false)],
            VerdictStub {
                stale_tokens: vec![],
                unverifiable_tokens: vec![],
            },
        );
        assert_eq!(stats.recovered, 1);
        assert_eq!(stats.not_frozen, 0);
        let after = fs::read(&d1).unwrap();
        assert_eq!(
            &after[..BLOCK],
            &vec![0x44u8; BLOCK],
            "block must be repaired on the failing disk"
        );
    }

    #[test]
    fn dry_run_counts_recovered_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let (cfg, d1) = cfg(&dir);
        let before = fs::read(&d1).unwrap();
        let freeze = FreezeController::new(None);
        let stats = run_batch(
            &cfg,
            freeze,
            true,
            vec![cand(d1.clone(), 0, 0, false)],
            VerdictStub {
                stale_tokens: vec![],
                unverifiable_tokens: vec![],
            },
        );
        assert_eq!(stats.recovered, 1, "dry-run reports would-be recovery");
        assert_eq!(
            fs::read(&d1).unwrap(),
            before,
            "dry-run must not touch the disk"
        );
    }

    #[test]
    fn unreadable_candidate_recovers_from_parity() {
        // An EIO-unreadable candidate (zero placeholder) is still recovered
        // from parity and written when the freeze is fine.
        let dir = tempfile::tempdir().unwrap();
        let (cfg, d1) = cfg(&dir);
        let freeze = FreezeController::new(None);
        let stats = run_batch(
            &cfg,
            freeze,
            false,
            vec![cand(d1.clone(), 0, 0, true)],
            VerdictStub {
                stale_tokens: vec![],
                unverifiable_tokens: vec![],
            },
        );
        assert_eq!(stats.recovered, 1);
        assert_eq!(
            &fs::read(&d1).unwrap()[..BLOCK],
            &vec![0x44u8; BLOCK],
            "unreadable sector must be rebuilt from parity"
        );
    }

    #[test]
    fn verdicts_stale_and_unverifiable_are_counted_and_never_written() {
        // Mixed batch: two recoverable, one stale (self-healed/freed), one
        // unverifiable (metadata unreadable for that sector) — the C7
        // accounting invariant must hold and neither stale nor skipped may
        // produce a write.
        let dir = tempfile::tempdir().unwrap();
        let (cfg, d1) = cfg(&dir);
        let freeze = FreezeController::new(None);
        let stats = run_batch(
            &cfg,
            freeze,
            false,
            vec![
                cand(d1.clone(), 0, 0, false),
                cand(d1.clone(), 16, 1, false),
                cand(d1.clone(), 32, 2, false), // token 2 -> Stale
                cand(d1.clone(), 48, 3, false), // token 3 -> Unverifiable
            ],
            VerdictStub {
                stale_tokens: vec![2],
                unverifiable_tokens: vec![3],
            },
        );
        assert_eq!(stats.recovered, 2);
        assert_eq!(stats.stale, 1);
        assert_eq!(stats.skipped, 1);
        // Invariant: sent(4) == stale + skipped + mismatch + deduped,
        // and mismatch == recovered + failed + not_corrupt + not_frozen +
        // readback_failed.
        assert_eq!(stats.mismatch, 2);
        assert_eq!(
            stats.mismatch,
            stats.recovered
                + stats.failed
                + stats.not_corrupt
                + stats.not_frozen
                + stats.readback_failed
        );
        assert_eq!(
            stats.stale + stats.skipped + stats.mismatch + stats.deduped,
            4
        );
        // Only the two corruption candidates were written; the stale one
        // (offset 32) must still hold its original corrupt bytes.
        let after = fs::read(&d1).unwrap();
        assert_eq!(&after[..16], &vec![0x44u8; 16]);
        assert_eq!(&after[16..32], &vec![0x44u8; 16]);
        assert_eq!(&after[32..48], &vec![0x11u8; 16], "stale: not written");
        assert_eq!(&after[48..64], &vec![0x11u8; 16], "skipped: not written");
    }

    #[test]
    fn array_phys_dedup_is_counted() {
        // Two candidates at the same physical offset collapse to one; the
        // collapsed candidate is counted `deduped` so the sent-vs-classified
        // reconciliation still sums exactly.
        let dir = tempfile::tempdir().unwrap();
        let (cfg, d1) = cfg(&dir);
        let freeze = FreezeController::new(None);
        let stats = run_batch(
            &cfg,
            freeze,
            false,
            vec![
                cand(d1.clone(), 0, 0, false),
                cand(d1.clone(), 0, 1, false), // accidental duplicate
            ],
            VerdictStub {
                stale_tokens: vec![],
                unverifiable_tokens: vec![],
            },
        );
        assert_eq!(stats.deduped, 1);
        assert_eq!(stats.mismatch, 1);
        assert_eq!(stats.recovered, 1);
        assert_eq!(stats.deduped + stats.mismatch, 2, "sent must reconcile");
    }

    #[test]
    fn fresh_read_failure_degrades_to_unreadable_recovery_path() {
        // H10: a sector that read fine at scan time but EIOs at the
        // writer's fresh read must NOT be auto-failed — it degrades to
        // the unreadable path (zero placeholder + skip-self-heal + parity
        // reconstruction verified against the stored csum).  We delete the
        // failing disk after building the config so the fresh read fails;
        // in dry-run the reconstruction is still classified `recovered`
        // (would-be), proving the candidate was not given up on.
        let dir = tempfile::tempdir().unwrap();
        let (cfg, d1) = cfg(&dir);
        fs::remove_file(&d1).unwrap();
        let freeze = FreezeController::new(None);
        let stats = run_batch(
            &cfg,
            freeze,
            true,
            vec![cand(d1.clone(), 0, 0, false)],
            VerdictStub {
                stale_tokens: vec![],
                unverifiable_tokens: vec![],
            },
        );
        assert_eq!(
            stats.recovered, 1,
            "degraded candidate must be recovered from parity"
        );
        assert_eq!(
            stats.failed, 0,
            "fresh-read failure must not auto-fail the candidate"
        );
        assert_eq!(stats.mismatch, 1);
    }

    #[test]
    fn freeze_timeout_defers_the_rest_of_the_batch() {
        // H4: once the batch window exceeds the freeze timeout, the rest
        // of the batch is deferred to the next flush instead of keeping
        // the freeze (and the live filesystem) held.  The first candidate
        // is always attempted (progress guarantee — a batch can never be
        // deferred in a loop), the remainder is returned for re-queueing,
        // and the deferred candidates are still classifiable by later
        // flushes (so the sent-vs-classified reconciliation stays exact).
        let dir = tempfile::tempdir().unwrap();
        let (cfg, d1) = cfg(&dir);
        let mut reconfirmer: Box<dyn Reconfirmer> = Box::new(VerdictStub {
            stale_tokens: vec![],
            unverifiable_tokens: vec![],
        });
        let mut stats = BatchStats::default();
        let mut freeze = FreezeController::new(None);
        // Timeout of zero: every candidate after the first is deferred.
        let deferred = flush_batch(
            &[
                cand(d1.clone(), 0, 0, false),
                cand(d1.clone(), 16, 1, false),
                cand(d1.clone(), 32, 2, false),
            ],
            &cfg,
            &mut freeze,
            &mut reconfirmer,
            1,
            false,
            Duration::from_millis(0),
            None,
            &mut stats,
        )
        .expect("deferral is not an abort");
        assert_eq!(deferred.len(), 2, "candidates 2 and 3 deferred");
        assert_eq!(stats.recovered, 1, "first candidate still classified");
        assert_eq!(stats.mismatch, 1);
        assert_eq!(
            &fs::read(&d1).unwrap()[..16],
            &vec![0x44u8; 16],
            "first candidate was written before the timeout"
        );
        assert_eq!(
            &fs::read(&d1).unwrap()[16..48],
            &vec![0x11u8; 32],
            "deferred candidates not written yet"
        );
        // Drain the deferred candidates with subsequent flushes; each
        // flush must classify at least one.
        let mut stats2 = BatchStats::default();
        let deferred2 = flush_batch(
            &deferred,
            &cfg,
            &mut freeze,
            &mut reconfirmer,
            1,
            false,
            Duration::from_millis(0),
            None,
            &mut stats2,
        )
        .expect("second flush must not abort");
        assert_eq!(deferred2.len(), 1);
        assert_eq!(stats2.recovered, 1);
        let deferred3 = flush_batch(
            &deferred2,
            &cfg,
            &mut freeze,
            &mut reconfirmer,
            1,
            false,
            Duration::from_millis(0),
            None,
            &mut stats2,
        )
        .expect("third flush must not abort");
        assert!(deferred3.is_empty());
        assert_eq!(stats2.recovered, 2);
        // Full drain: every candidate classified exactly once across the
        // three flushes (reconciliation: 3 sent == 3 recovered).
        assert_eq!(stats.recovered + stats2.recovered, 3);
    }
}
