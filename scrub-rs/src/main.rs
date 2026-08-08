//! scrub-rs CLI: btrfs scrub + unRAID parity-recovery driver.
//!
//! Exit codes reflect the state of the data, not which flags were passed
//! (the same disk yields the same code in dry-run and repair mode).

use scrub_rs::array;
use scrub_rs::btrfs;
use scrub_rs::fs;
use scrub_rs::fs::FilesystemScrub;

use std::env;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

// Exit-code contract: codes reflect the state of the data, not the flags
// passed. Callers (the unRAID plugin) branch on meaning.
const EXIT_RUNTIME_ERROR: u8 = 1;
const EXIT_USAGE_ERROR: u8 = 2;
const EXIT_ISSUES_FOUND: u8 = 3;
const EXIT_RECOVERED: u8 = 4;
const EXIT_RECOVER_FAILED: u8 = 5;

const EXIT_METADATA_FATAL: u8 = 6;

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let dev = match args.next() {
        Some(p) => p,
        None => {
            print_help();
            return ExitCode::SUCCESS;
        }
    };

    if dev == "--help" || dev == "-h" {
        print_help();
        return ExitCode::SUCCESS;
    }

    if dev == "--dump-array" {
        return dump_array();
    }

    if dev == "--resolve" {
        let device = match args.next() {
            Some(d) => d,
            None => {
                eprintln!("usage: scrub-rs --resolve <device> <logical>");
                return ExitCode::from(EXIT_USAGE_ERROR);
            }
        };
        let logical_str = match args.next() {
            Some(l) => l,
            None => {
                eprintln!("usage: scrub-rs --resolve <device> <logical>");
                return ExitCode::from(EXIT_USAGE_ERROR);
            }
        };
        let logical = match u64::from_str_radix(literal_hex(&logical_str), 16) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("invalid logical {logical_str:?}: {e}");
                return ExitCode::from(EXIT_USAGE_ERROR);
            }
        };
        return resolve_cmd(&device, logical);
    }

    run_scrub(dev, args)
}

fn run_scrub<I: Iterator<Item = String>>(dev: String, args: I) -> ExitCode {
    let mut args = args.peekable();
    if args.peek().map(|s| s.as_str()) == Some("--help")
        || args.peek().map(|s| s.as_str()) == Some("-h")
    {
        print_help();
        return ExitCode::SUCCESS;
    }

    let mut base_offset: u64 = 0;

    // Default is assess-only; --repair enables writes.
    let mut dry_run = true;

    let mut freeze_enabled = true;
    let mut freeze_mount: Option<String> = None;

    let mut batch_max: usize = 64;
    let mut batch_idle: f64 = 5.0;

    let mut freeze_max: f64 = 60.0;

    let mut status_port: u16 = 0;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--status-port" => {
                let v = match args.next() {
                    Some(s) => s,
                    None => {
                        eprintln!("error: --status-port requires a value");
                        return ExitCode::from(EXIT_USAGE_ERROR);
                    }
                };
                match v.parse::<u16>() {
                    Ok(p) => status_port = p,
                    Err(_) => {
                        eprintln!("error: --status-port must be a port number (0-65535)");
                        return ExitCode::from(EXIT_USAGE_ERROR);
                    }
                }
            }
            "--offset" => {
                let v = match args.next() {
                    Some(s) => s,
                    None => {
                        eprintln!("error: --offset requires a value");
                        return ExitCode::from(EXIT_USAGE_ERROR);
                    }
                };
                base_offset = match parse_offset(&v) {
                    Ok(o) => o,
                    Err(e) => {
                        eprintln!("error parsing --offset {v:?}: {e}");
                        return ExitCode::from(EXIT_USAGE_ERROR);
                    }
                };
            }
            "--repair" => {
                dry_run = false;
            }
            "--batch-max" => {
                let v = match args.next() {
                    Some(s) => s,
                    None => {
                        eprintln!("error: --batch-max requires a value");
                        return ExitCode::from(EXIT_USAGE_ERROR);
                    }
                };
                match v.parse::<usize>() {
                    Ok(n) if n > 0 => batch_max = n,
                    _ => {
                        eprintln!("error: --batch-max must be a positive integer");
                        return ExitCode::from(EXIT_USAGE_ERROR);
                    }
                };
            }
            "--batch-idle" => {
                let v = match args.next() {
                    Some(s) => s,
                    None => {
                        eprintln!("error: --batch-idle requires a value");
                        return ExitCode::from(EXIT_USAGE_ERROR);
                    }
                };
                match v.parse::<f64>() {
                    Ok(s) if s >= 0.0 => batch_idle = s,
                    _ => {
                        eprintln!("error: --batch-idle must be a non-negative number of seconds");
                        return ExitCode::from(EXIT_USAGE_ERROR);
                    }
                };
            }
            "--freeze-max" => {
                let v = match args.next() {
                    Some(s) => s,
                    None => {
                        eprintln!("error: --freeze-max requires a value");
                        return ExitCode::from(EXIT_USAGE_ERROR);
                    }
                };
                match v.parse::<f64>() {
                    Ok(s) if s.is_finite() && s > 0.0 => freeze_max = s,
                    _ => {
                        eprintln!("error: --freeze-max must be a positive number of seconds");
                        return ExitCode::from(EXIT_USAGE_ERROR);
                    }
                };
            }
            "--freeze-mount" => {
                let v = match args.next() {
                    Some(s) => s,
                    None => {
                        eprintln!("error: --freeze-mount requires a path");
                        return ExitCode::from(EXIT_USAGE_ERROR);
                    }
                };
                freeze_mount = Some(v);
            }
            "--no-freeze" => {
                freeze_enabled = false;
            }
            other => {
                eprintln!("unknown argument: {other}");
                eprintln!(
                    "usage: scrub-rs <device-or-image> [--offset <bytes>] \
                     [--repair] [--freeze-mount <path>] [--no-freeze] \
                     [--batch-max <n>] [--batch-idle <s>] [--freeze-max <s>] \
                     [--status-port <n>]"
                );
                return ExitCode::from(EXIT_USAGE_ERROR);
            }
        }
    }

    let mut scrub = match btrfs::BtrfsScrub::open(&dev, base_offset) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error opening btrfs filesystem: {e}");
            return ExitCode::from(EXIT_RUNTIME_ERROR);
        }
    };
    for line in scrub.describe() {
        println!("{line}");
    }

    let status = Arc::new(scrub_rs::status::StatusCounters::new());

    status.set_device(&dev);
    if status_port != 0 {
        status.set_state("starting");

        match scrub_rs::status::StatusServer::spawn(status_port, status.clone()) {
            Ok(_) => println!(
                "status         : serving live counters on 127.0.0.1:{status_port} (GET /status)"
            ),
            Err(e) => eprintln!(
                "note: could not start status server on 127.0.0.1:{status_port} ({e}); continuing without it"
            ),
        }
    }
    scrub.set_status(status.clone());

    // Resolve this device's array slot; run a plain scrub if it is not a
    // data disk or no array config is available.
    let (cfg, scrub_slot) = {
        match array::config::load() {
            Ok(loaded) => {
                let slot = array::config::slot_from_array_partition(&dev)
                    .or_else(|| loaded.slot_for_raw_dev(Path::new(&dev)));
                match slot {
                    Some(slot) => (Some(loaded), slot),
                    None => {
                        eprintln!(
                            "note: {dev:?} is not a data disk in the array config; \
                             running plain scrub (no parity reconstruction)"
                        );
                        (None, 0)
                    }
                }
            }
            Err(e) => {
                eprintln!(
                    "note: no array config available ({e}); running plain scrub \
                     (no parity reconstruction)"
                );
                (None, 0)
            }
        }
    };

    // Startup canary: reconstruct the target superblock from parity; a
    // missing magic or read failure means the array/parity is broken and
    // recovery would be unsafe, so abort.
    if let (Some(cfg), slot) = (cfg.as_ref(), scrub_slot)
        && slot != 0
    {
        const SB_BLOCK: usize = 4096;
        match scrub_rs::canary::reconstruct_block(cfg, slot, scrub.superblock_offset(), SB_BLOCK) {
            Ok(block) => {
                if scrub.block_has_magic(&block) {
                    println!(
                        "\ncanary         : OK — parity reconstructed the target \
                         superblock and it carries the filesystem magic (array config sound)"
                    );
                } else {
                    eprintln!(
                        "\n[CANARY FATAL] parity reconstructed the target superblock \
                         block but it does NOT carry the filesystem magic. The array config, \
                         slot, or parity is misconfigured (stale/out-of-sync parity, \
                         wrong rdevOffset, or wrong disk). Aborting: any recovery would \
                         be built on a broken array config."
                    );
                    status.set_state("error");
                    print_status_block(&status);
                    return ExitCode::from(EXIT_RUNTIME_ERROR);
                }
            }
            Err(e) => {
                eprintln!(
                    "\n[CANARY FATAL] could not reconstruct the target superblock from \
                     parity ({e}). The array config or parity is misconfigured. \
                     Aborting: any recovery would be built on a broken array config."
                );
                status.set_state("error");
                print_status_block(&status);
                return ExitCode::from(EXIT_RUNTIME_ERROR);
            }
        }
    }

    println!(
        "\nscan mode     : recovery assessment + {}",
        if dry_run {
            "dry-run (no writes)"
        } else {
            "REPAIR (writes enabled)"
        }
    );

    // Scrub callbacks: forward each mismatch event to the recovery
    // pipeline as a Candidate.
    struct Driver {
        cfg: Option<array::config::ArrayConfig>,

        tx: Option<std::sync::mpsc::SyncSender<scrub_rs::batch_recover::Msg>>,
        scrub_slot: u64,

        sent: Arc<AtomicU64>,
    }
    impl fs::ScrubCallbacks for Driver {
        fn wants_raw_candidates(&self) -> bool {
            self.tx.is_some()
        }

        fn on_log(&mut self, line: &str) {
            eprintln!("{line}");
        }

        fn on_event(&mut self, ev: &fs::ScrubEvent) {
            // No pipeline (plain scrub) or no verifier (no stored csum):
            // nothing to recover.
            let Some(tx) = self.tx.as_ref() else { return };
            let Some(cfg) = self.cfg.as_ref() else { return };
            // Sectors without a stored checksum cannot be verified and
            // are skipped.
            let Some(verifier) = ev.verify.as_ref() else {
                return;
            };
            let Some(failing_dev) = cfg.data_dev(self.scrub_slot) else {
                eprintln!(
                    "  [slot {}] not a data disk in array config",
                    self.scrub_slot
                );
                return;
            };
            let raw_phys = cfg.raw_phys(self.scrub_slot, ev.array_phys).expect(
                "failing_dev was just verified to be a data disk, so raw_phys must resolve",
            );
            let cand = scrub_rs::batch_recover::Candidate {
                array_phys: ev.array_phys,
                block_size: ev.block_size,
                verify: verifier.clone(),
                raw_phys,
                failing_dev: failing_dev.to_path_buf(),
                reconfirm: ev.reconfirm.clone(),
                unreadable: ev.unreadable,
            };

            if let Err(e) = tx.send(scrub_rs::batch_recover::Msg::Candidate(cand)) {
                eprintln!("error: recovery writer thread gone: {e}");
            } else {
                self.sent.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    let sent = Arc::new(AtomicU64::new(0));
    let mut driver = Driver {
        cfg: cfg.clone(),
        tx: None,
        scrub_slot,
        sent: sent.clone(),
    };

    // Freeze only matters for live repair writes; offline and dry-run
    // runs get a no-op controller.
    let mut freeze_controller = if !dry_run && freeze_enabled && freeze_mount.is_some() {
        scrub_rs::freeze::FreezeController::new(freeze_mount.as_ref().map(std::path::PathBuf::from))
    } else {
        scrub_rs::freeze::FreezeController::new(None)
    };
    if !dry_run && freeze_enabled && freeze_mount.is_some() {
        println!(
            "\nfreeze         : enabled for live mount {} (per-batch window)",
            freeze_mount.as_deref().unwrap_or("")
        );
    } else if !dry_run && freeze_mount.is_none() {
        println!(
            "\nfreeze         : disabled (no --freeze-mount; offline/unmounted image or not declared)"
        );
    }

    let mut writer_handle = None;
    let mut acc_handle = None;
    if let Some(cfg) = cfg.clone() {
        let reconfirmer = match scrub.reconfirmer() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("error building recovery re-confirm handle: {e}");
                status.set_state("error");
                print_status_block(&status);
                return ExitCode::from(EXIT_RUNTIME_ERROR);
            }
        };

        let fc = std::mem::replace(
            &mut freeze_controller,
            scrub_rs::freeze::FreezeController::new(None),
        );
        // Spawn the batched recovery pipeline (accumulator + writer
        // threads); its result is joined and reconciled after the scrub.
        let (tx, acc, handle) = match scrub_rs::batch_recover::spawn_pipeline(
            cfg,
            fc,
            reconfirmer,
            scrub_slot,
            dry_run,
            batch_max,
            std::time::Duration::from_secs_f64(batch_idle),
            std::time::Duration::from_secs_f64(freeze_max),
            Some(status.clone()),
        ) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("error spawning recovery pipeline: {e}");
                status.set_state("error");
                print_status_block(&status);
                return ExitCode::from(EXIT_RUNTIME_ERROR);
            }
        };
        driver.tx = Some(tx);
        acc_handle = Some(acc);
        writer_handle = Some(handle);
        status.set_recovery(true);
        println!(
            "recovery       : assessment pipeline (max {} candidates/batch, {}s idle flush){}",
            batch_max,
            batch_idle,
            if dry_run {
                " [dry-run: no writes]"
            } else {
                " [REPAIR enabled]"
            }
        );
    } else {
        println!("recovery       : disabled (no array config / not a data disk) — plain scrub");
    }

    status.set_state("running");
    let stats = match scrub.run(&mut driver) {
        Ok(s) => s,
        Err(e) => {
            status.set_state("error");
            eprintln!("error running scrub: {e}");
            print_status_block(&status);
            return ExitCode::from(EXIT_RUNTIME_ERROR);
        }
    };

    status.set_state("done");
    // Shut the pipeline down: send Done, drain the accumulator, join the
    // writer.
    let had_writer = writer_handle.is_some();
    let mut batch_stats = scrub_rs::batch_recover::BatchStats::default();
    if had_writer {
        if let Some(tx) = driver.tx.take() {
            let _ = tx.send(scrub_rs::batch_recover::Msg::Done);
        }

        if let Some(acc) = acc_handle.take()
            && acc.join().is_err()
        {
            eprintln!("error: recovery accumulator thread panicked — results are incomplete");
            status.set_state("error");
            print_status_block(&status);
            return ExitCode::from(EXIT_RUNTIME_ERROR);
        }
        match writer_handle.take().unwrap().join() {
            Ok(Ok(s)) => batch_stats = s,
            Ok(Err(e)) => {
                eprintln!("error: recovery pipeline aborted: {e}");
                status.set_state("error");
                print_status_block(&status);
                return ExitCode::from(EXIT_RUNTIME_ERROR);
            }
            Err(_) => {
                eprintln!("error: recovery writer thread panicked — results are incomplete");
                status.set_state("error");
                print_status_block(&status);
                return ExitCode::from(EXIT_RUNTIME_ERROR);
            }
        }

        // Every candidate handed to the pipeline must be classified into
        // exactly one bucket; a shortfall means a thread died mid-run.
        let classified =
            batch_stats.mismatch + batch_stats.stale + batch_stats.skipped + batch_stats.deduped;
        let sent = driver.sent.load(Ordering::Relaxed);
        if classified != sent {
            eprintln!(
                "error: verdict reconciliation failed: {sent} candidate(s) were handed to \
                 the recovery pipeline but only {classified} were classified (a pipeline \
                 thread likely died mid-run). Results are INCOMPLETE — this run cannot be \
                 reported as a complete scrub."
            );
            status.set_state("error");
            print_status_block(&status);
            return ExitCode::from(EXIT_RUNTIME_ERROR);
        }

        let sub_buckets = batch_stats.recovered
            + batch_stats.failed
            + batch_stats.not_corrupt
            + batch_stats.not_frozen
            + batch_stats.readback_failed;
        if sub_buckets != batch_stats.mismatch {
            eprintln!(
                "error: internal recovery accounting mismatch: mismatch={} but \
                 recovered+failed+not_corrupt+not_frozen+readback_failed={sub_buckets}",
                batch_stats.mismatch
            );
            status.set_state("error");
            print_status_block(&status);
            return ExitCode::from(EXIT_RUNTIME_ERROR);
        }
    }

    println!("\nscrub complete:");
    println!("  sectors checked    : {}", stats.sectors_checked);
    println!("  sectors ok         : {}", stats.sectors_ok);
    println!(
        "  sectors mismatch   : {}",
        stats.sectors_mismatch + batch_stats.mismatch
    );
    println!("  sectors no csum    : {}", stats.sectors_no_csum);
    println!(
        "  sectors stale      : {}",
        stats.sectors_stale + batch_stats.stale
    );
    println!("  sectors read error : {}", stats.sectors_read_error);
    println!("  csum branches stale: {}", stats.stale_csum_branches);
    println!("  isolation truncate : {}", stats.isolation_truncated);
    println!("  metadata hdr errs  : {}", stats.metadata_header_errors);
    println!("  metadata read err  : {}", stats.metadata_read_errors);
    println!("  metadata mirror   : {}", stats.metadata_mirror_mismatches);
    println!("  bytes checked      : {}", stats.bytes_checked);

    if had_writer {
        println!("\nrecovery summary:");
        println!("  recovered       : {}", batch_stats.recovered);
        println!("  failed          : {}", batch_stats.failed);
        println!("  skipped         : {}", batch_stats.skipped);
        println!("  not_corrupt     : {}", batch_stats.not_corrupt);
        println!("  not_frozen      : {}", batch_stats.not_frozen);
        println!("  readback_failed : {}", batch_stats.readback_failed);

        if status.repaired_while_mounted.load(Ordering::Relaxed) != 0 {
            eprintln!(
                "\n[PAGE-CACHE ADVISORY] {} repaired block(s) were written while the \
                 filesystem was mounted live. The raw device's buffer cache was flushed, \
                 but the mounted filesystem's FILE page cache may STILL serve the OLD \
                 (corrupt) bytes for ranges that were cached before the repair — \
                 indefinitely, until memory pressure evicts them. Reboot or remount the \
                 disk before trusting reads of the repaired files. The affected \
                 raw-rdev offsets are in the RECOVERED lines above.",
                batch_stats.recovered + batch_stats.readback_failed
            );
        }
    }

    // Any real issue blocks exit 0.
    let issues_found = stats.sectors_mismatch + batch_stats.mismatch > 0
        || stats.sectors_read_error > 0
        || stats.metadata_header_errors > 0
        || stats.metadata_read_errors > 0
        || stats.stale_csum_branches > 0
        || batch_stats.skipped > 0;

    if stats.metadata_read_errors > 0 {
        eprintln!(
            "\n[METADATA READ ERRORS] {} metadata node(s) failed with a READ \
             (EIO) error — the bytes could not be fetched from the device. Some \
             data may be UNVERIFIED. Check the disk hardware (SMART, cables, \
             controller), not `btrfs check`.",
            stats.metadata_read_errors
        );
    }

    if stats.stale_csum_branches > 0 {
        eprintln!(
            "\n[COVERAGE GAP] {} CSUM-tree branch(es) went stale mid-scrub \
             (freed/rewritten while the run was in progress); their sectors were NOT \
             verified this run. Rerun the scrub to cover them — exit 0 is refused \
             while this counter is non-zero.",
            stats.stale_csum_branches
        );
    }

    // Exit-code cascade: metadata fatal, clean, recovered, recover-failed,
    // issues found.
    let code = if stats.metadata_header_errors > 0 {
        eprintln!(
            "\n[METADATA FATAL] {} metadata node(s) had NO good copy (all mirrors \
             failed header checksum, no parity recovery attempted). The live \
             filesystem may be serving corrupt trees. UNMOUNT the filesystem \
             (if still mounted) and run `btrfs check --repair` offline.",
            stats.metadata_header_errors
        );
        ExitCode::from(EXIT_METADATA_FATAL)
    } else if !issues_found {
        ExitCode::SUCCESS
    // Coverage gap with no real corruption: the run is incomplete, so
    // refuse the clean/recovered codes.
    } else if stats.stale_csum_branches > 0
        && stats.sectors_mismatch == 0
        && stats.sectors_read_error == 0
        && stats.metadata_read_errors == 0
        && batch_stats.mismatch == 0
        && batch_stats.skipped == 0
    {
        ExitCode::from(EXIT_ISSUES_FOUND)
    // Every mismatch was recovered and nothing failed or was skipped.
    } else if had_writer
        && stats.metadata_read_errors == 0
        && batch_stats.failed == 0
        && batch_stats.skipped == 0
        && batch_stats.not_frozen == 0
        && batch_stats.readback_failed == 0
        && batch_stats.not_corrupt == 0
        && stats.stale_csum_branches == 0
    {
        ExitCode::from(EXIT_RECOVERED)
    // Some candidates could not be recovered.
    } else if had_writer {
        ExitCode::from(EXIT_RECOVER_FAILED)
    } else {
        ExitCode::from(EXIT_ISSUES_FOUND)
    };

    print_status_block(&status);
    code
}

fn print_status_block(status: &scrub_rs::status::StatusCounters) {
    println!("status:");
    print!("{}", status.snapshot());
}

fn print_help() {
    println!("usage: scrub-rs <device-or-image> [options]");
    println!("       scrub-rs --dump-array");
    println!("       scrub-rs --resolve <device> <logical>");
    println!();
    println!("options:");
    println!("  --offset <bytes>      byte offset of the btrfs partition in the backing file");
    println!("  --repair              write reconstructed blocks back to the failing disk");
    println!("                        (default: dry-run — assess + reconstruct, never mutate)");
    println!("  --freeze-mount <path> live mountpoint to FIFREEZE during repair writes");
    println!("  --no-freeze           disable the freeze (unsafe with --repair on a live FS)");
    println!("  --batch-max <N>       max candidates per recovery batch (default 64)");
    println!("  --batch-idle <X>      flush a batch after Xs of no new candidate (default 5.0)");
    println!(
        "  --freeze-max <s>      wall-clock bound for one batch's freeze window; on expiry the"
    );
    println!("                        batch thaws and defers the remainder to the next batch");
    println!("                        (default 60.0; guards a slow/dying disk in the gather)");
    println!("  --status-port <n>     serve live counters on 127.0.0.1:<n> (0 = off)");
    println!("  --help, -h            show this help and the recovery-counter glossary");
    println!();
    println!("recovery summary counters (always assessed when an array is present):");
    println!("  mismatch   real corruption found: live csum still disagrees with stored.");
    println!("  stale      benign churn: the live FS rewrote/freed the block (or nodatasum).");
    println!("             Re-confirm proved it was never genuine corruption — nothing written.");
    println!("  skipped    metadata for THIS sector was unreadable, so we could not safely");
    println!("             re-confirm it. Write skipped for just this candidate (per-sector,");
    println!("             not a global gate). Non-zero blocks exit code 4.");
    println!("  recovered  confirmed corruption rebuilt from parity and written (or would-be");
    println!("             in dry-run), with a read-back verify that reads the device (the");
    println!("             raw rdev's cache is invalidated BEFORE the read-back).");
    println!("  failed     confirmed corruption where the fix did not land: stripe read error,");
    println!("             parity gather error, write-back error, or parity reconstruction the");
    println!("             verifier rejected.");
    println!("  not_corrupt  reconstruction rejected by the verifier as not-the-original —");
    println!("             nothing to write (counted so verdict totals reconcile).");
    println!("  not_frozen   reconstruction succeeded but this batch's required filesystem");
    println!("             freeze (FIFREEZE) FAILED, so the write was deferred (assess-only).");
    println!("             Nothing is ever written unfrozen unless --no-freeze is explicit.");
    println!("             Non-zero blocks exit code 4.");
    println!("  readback_failed  the write landed but the post-write read-back disagreed with");
    println!("             the verifier or errored — a lying/failing-disk signal. Non-zero");
    println!("             blocks exit code 4.");
    println!();
    println!("coverage notes (what this scrub does NOT check — the honest gap vs `btrfs scrub`):");
    println!("  - data written nodatasum / nocow has no csum entries and is never checked;");
    println!("  - inline extent data lives in METADATA chunk ranges, which the DATA-only");
    println!("    dev-extent drive never visits — inline data is not checked;");
    println!("  - metadata trees are verified only where the scrub traverses them");
    println!("    (chunk/root/dev/csum trees at open/walk time); FS_TREE / EXTENT_TREE");
    println!("    subtrees not crossed by the walk are not part of this pass;");
    println!("  - CSUM-tree branches that go stale mid-scrub (freed/rewritten while the run");
    println!("    is in progress) are skipped and counted in `stale_csum_branches` — a");
    println!("    non-zero value refuses exit 0 (their sectors were not verified);");
    println!("  - runs whose EIO isolation budget is exhausted mark the remaining sectors");
    println!("    unreadable without further probing (counted `isolation_truncated`); they");
    println!("    still count as read errors, so exit 0 is already refused.");
    println!();
    println!("At the end of every run scrub-rs prints `status:` followed by the same");
    println!("key=value payload the --status-port server serves (state, device, all");
    println!("counters, progress), so the exact final numbers are always available from");
    println!("the run output — no flag needed.");
}

fn parse_offset(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (sign, rest) = match s.chars().next() {
        Some('+') => (1u64, &s[1..]),
        Some('-') => return Err(format!("negative byte offsets are not allowed: {s:?}")),
        _ => (1u64, s),
    };
    // A leading '+' means the value is in 512-byte sectors (md-style);
    // bare or 0x-prefixed values are bytes.
    let raw: u64 = if let Some(h) = rest.strip_prefix("0x") {
        u64::from_str_radix(h, 16).map_err(|e| format!("invalid hex: {e}"))?
    } else {
        rest.parse::<u64>()
            .map_err(|e| format!("invalid decimal: {e}"))?
    };

    let bytes = if s.starts_with('+') { raw * 512 } else { raw };
    Ok(sign * bytes)
}

fn literal_hex(s: &str) -> &str {
    s.strip_prefix("0x").unwrap_or(s)
}

fn dump_array() -> ExitCode {
    match array::config::load() {
        Ok(cfg) => {
            fn disp(o: &Option<PathBuf>) -> String {
                o.as_ref()
                    .map_or("None".to_string(), |p| p.display().to_string())
            }
            println!("parity_p : {}", disp(&cfg.parity_p));
            println!("parity_q : {}", disp(&cfg.parity_q));
            println!("data_devs:");
            for (slot, path) in &cfg.data_devs {
                let off = cfg.raw_offset_for(path);
                println!("  slot {slot}: {} (raw_offset=0x{off:x})", path.display());
            }
            println!("rdev_offsets:");
            for (path, off) in &cfg.rdev_offsets {
                println!("  {} -> 0x{off:x}", path.display());
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error loading array config: {e}");
            ExitCode::from(EXIT_RUNTIME_ERROR)
        }
    }
}

// Diagnostic: map a btrfs logical address to a physical array location.
fn resolve_cmd(device: &str, logical: u64) -> ExitCode {
    let chunk_map = match btrfs::open(device, 0) {
        Ok(ctx) => ctx.chunk_map,
        Err(e) => {
            eprintln!("error opening btrfs filesystem on {device}: {e}");
            return ExitCode::from(EXIT_RUNTIME_ERROR);
        }
    };

    let cfg = match array::config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error loading array config: {e}");
            return ExitCode::from(EXIT_RUNTIME_ERROR);
        }
    };

    let (devid, array_phys) = match chunk_map.lookup(logical) {
        Some(loc) => loc,
        None => {
            eprintln!("error: no chunk mapping for logical 0x{logical:x}");
            return ExitCode::from(EXIT_RUNTIME_ERROR);
        }
    };

    match array::resolve::resolve(&cfg, devid, array_phys) {
        Ok(loc) => {
            println!(
                "devid={} logical=0x{:x} array_phys=0x{:x} dev_path={} raw_phys=0x{:x}",
                loc.devid,
                logical,
                loc.array_phys,
                loc.dev_path.display(),
                loc.raw_phys
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error resolving logical 0x{logical:x}: {e}");
            ExitCode::from(EXIT_RUNTIME_ERROR)
        }
    }
}
