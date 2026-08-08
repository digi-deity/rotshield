//! Batched parity recovery: dedup → freeze → re-confirm → recover → write-back.

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

/// One mismatched sector queued for recovery.
#[derive(Clone)]
pub struct Candidate {
    pub array_phys: u64,

    pub block_size: usize,

    /// Verifies a candidate block against the sector's stored checksum.
    pub verify: SectorVerifier,

    pub raw_phys: u64,

    pub failing_dev: PathBuf,

    /// Opaque request to re-check the sector against live metadata at write time.
    pub reconfirm: Option<ReconfirmRequest>,

    /// The source read returned EIO; `verify` still carries the stored checksum.
    pub unreadable: bool,
}

/// Messages from the scrub thread to the accumulator.
pub enum Msg {
    /// One mismatched sector.
    Candidate(Candidate),

    /// The scrub finished; flush everything queued.
    /// No more batches will arrive; drain the pending remainder.
    Done,
}

/// Messages from the accumulator to the writer thread.
enum BatchMsg {
    /// A full batch of candidates.
    Batch(Vec<Candidate>),

    Done,
}

/// Recovery accounting; stage counters (dedup/stale/skipped/mismatch) and
/// outcome buckets are separate partitions of the same candidates.
#[derive(Debug, Default)]
pub struct BatchStats {
    /// Candidates that entered recovery (after dedup and re-confirm).
    pub mismatch: u64,

    /// Sectors no longer corrupt (rewritten or freed) — nothing to do.
    pub stale: u64,

    /// Reconstructed, written, and read-back verified (or dry-run).
    pub recovered: u64,

    /// Recovery attempted but not achieved.
    pub failed: u64,

    /// Skipped without recovery (e.g. re-confirm metadata unreadable).
    pub skipped: u64,

    /// Would have been written, but the freeze failed (assess-only batch).
    pub not_frozen: u64,

    /// Written but the read-back could not be verified.
    pub readback_failed: u64,

    /// Failed-disk read matched the stored checksum.
    pub not_corrupt: u64,

    /// Duplicate reports of the same sector collapsed in a batch.
    pub deduped: u64,
}

pub type WriterResult = io::Result<BatchStats>;

/// Start the accumulator and writer threads; returns the send handle for
/// candidates and join handles for both threads. Channels are synchronous
/// (capacity 1) so a slow writer back-pressures the scrub.
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

/// Collect candidates from the scrub and forward them to the writer in
/// batches of up to `batch_max`, flushing early when `batch_idle` elapses.
/// On Done/disconnect, flush whatever remains.
fn run_accumulator(
    rx: mpsc::Receiver<Msg>,
    tx_b: SyncSender<BatchMsg>,
    batch_max: usize,
    batch_idle: Duration,
) {
    let mut batch: Vec<Candidate> = Vec::with_capacity(batch_max);

    loop {
        // Wait for the first candidate; when the scrub finishes, send a
        // final batch plus Done.
        match rx.recv() {
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
            Err(_) => {
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
                // Batch filled the idle window — send what we have.
                Err(RecvTimeoutError::Timeout) => break,
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

        // Hand the batch to the writer and start the next one (full or idle timeout).
        if tx_b
            .send(BatchMsg::Batch(std::mem::take(&mut batch)))
            .is_err()
        {
            return;
        }
    }
}

/// Receive batches and flush each one; on Done, keep flushing the deferred
/// remainder until nothing is left.
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

    let mut pending: Vec<Candidate> = Vec::new();

    loop {
        match rx.recv() {
            Ok(BatchMsg::Batch(batch)) => {
                // Deferred candidates from freeze-timeouts are prepended to the next flush.
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

/// Flush the pending candidates, keeping the deferred remainder as pending.
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

/// Process one batch: sort and dedup by array offset, freeze the filesystem,
/// then per candidate re-confirm → read the failing disk → gather the stripe →
/// recover → write back → read-back verify. Returns the candidates deferred
/// by the freeze-timeout; aborts on a failed thaw.
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
    let mut batch = batch.to_vec();
    // Dedup: the scrub can report the same sector more than once.
    batch.sort_by_key(|a| a.array_phys);
    let pre_dedup = batch.len();
    batch.dedup_by(|a, b| a.array_phys == b.array_phys);
    let deduped = (pre_dedup - batch.len()) as u64;
    stats.deduped += deduped;
    if let Some(c) = counters {
        c.deduped.fetch_add(deduped, Ordering::Relaxed);
    }

    // One freeze covers the whole batch. A failed freeze makes the batch
    // assess-only: candidates are classified but never written.
    let has_live_mount = freeze.has_live_mount();
    let freeze_guard = match freeze.guard() {
        Ok(g) => g,
        // Freeze failed: the filesystem is not frozen, so this batch runs
        // assess-only — candidates are classified but never written.
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

    // Writes proceed only under an active freeze (or when there is no live
    // mount at all, e.g. an offline image).
    let writes_allowed = freeze_guard.is_some() || !has_live_mount;

    // Bound the freeze window: once exceeded, defer the rest of the batch
    // (still classified exactly once) to the next flush.
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

        // Re-confirm against live metadata: the sector may have been
        // rewritten or freed since the scan.
        let verdict = match &cand.reconfirm {
            Some(req) => reconfirmer.reconfirm(req),

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
            // Metadata unreadable — cannot safely write; skip this sector.
            Reconfirm::Unverifiable => {
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

        // Re-read the failing disk. If it now verifies, the sector was
        // rewritten since the scan — stale, nothing to recover.
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
                        stats.stale += 1;
                        if let Some(c) = counters {
                            c.stale.fetch_add(1, Ordering::Relaxed);
                        }
                        continue;
                    }
                    block
                }
                Err(e) => {
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

        // Gather the other data disks plus P/Q at this offset.
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
        // Try P, then Q, then the PQ 2-disk solve; the verifier picks the
        // first candidate that matches the stored checksum.
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

                // Dry-run or assess-only: report the would-be write, never issue it.
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

                // Write the recovered block straight to the raw rdev.
                match stripe::write_block(cfg, &cand.failing_dev, cand.array_phys, block) {
                    Ok(()) => {
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

                // Drop the device page cache so the read-back reads the platter,
                // not stale cache.
                match drop_cache(&cand.failing_dev) {
                    Ok(()) => {}
                    Err(e) => {
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
                // Read-back verification: only count the write as recovered when
                // the device returns bytes that verify.
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

                let _ = drop_cache(&cand.failing_dev);
            }
            // The on-disk block already matches its checksum (raced rewrite).
            RecoveryResult::NotCorrupt => {
                eprintln!(
                    "  [0x{:x}] not corrupt (matches stored csum)",
                    cand.raw_phys
                );
                stats.not_corrupt += 1;
                if let Some(c) = counters {
                    c.not_corrupt.fetch_add(1, Ordering::Relaxed);
                }
            }
            // All parity paths failed or the verifier rejected everything.
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

    // A failed thaw means the filesystem may still be frozen — abort the run
    // rather than risk unfrozen writes in later batches.
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

/// Invalidate the kernel's page cache for the device (BLKFLSBUF ioctl) so
/// subsequent reads come from the platter.
fn drop_cache(dev: &Path) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let f = std::fs::File::open(dev)?;

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
                cand(d1.clone(), 32, 2, false),
                cand(d1.clone(), 48, 3, false),
            ],
            VerdictStub {
                stale_tokens: vec![2],
                unverifiable_tokens: vec![3],
            },
        );
        assert_eq!(stats.recovered, 2);
        assert_eq!(stats.stale, 1);
        assert_eq!(stats.skipped, 1);

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

        let after = fs::read(&d1).unwrap();
        assert_eq!(&after[..16], &vec![0x44u8; 16]);
        assert_eq!(&after[16..32], &vec![0x44u8; 16]);
        assert_eq!(&after[32..48], &vec![0x11u8; 16], "stale: not written");
        assert_eq!(&after[48..64], &vec![0x11u8; 16], "skipped: not written");
    }

    #[test]
    fn array_phys_dedup_is_counted() {
        let dir = tempfile::tempdir().unwrap();
        let (cfg, d1) = cfg(&dir);
        let freeze = FreezeController::new(None);
        let stats = run_batch(
            &cfg,
            freeze,
            false,
            vec![cand(d1.clone(), 0, 0, false), cand(d1.clone(), 0, 1, false)],
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
        let dir = tempfile::tempdir().unwrap();
        let (cfg, d1) = cfg(&dir);
        let mut reconfirmer: Box<dyn Reconfirmer> = Box::new(VerdictStub {
            stale_tokens: vec![],
            unverifiable_tokens: vec![],
        });
        let mut stats = BatchStats::default();
        let mut freeze = FreezeController::new(None);

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

        assert_eq!(stats.recovered + stats2.recovered, 3);
    }
}
