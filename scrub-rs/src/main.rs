//! scrub-rs — a minimal, standalone btrfs scrub tool.
//!
//! Reads the btrfs on-disk metadata directly from a backing store (a block
//! device *or* a regular file image — both are just seekable byte streams)
//! and verifies the CRC32C of every data sector that has a stored checksum,
//! mirroring what `btrfs scrub` does but without involving the kernel.
//!
//! This is the CLI binary; the reusable modules live in the `scrub_rs`
//! library crate (`src/lib.rs`) so utility binaries can share them.

use scrub_rs::array;
use scrub_rs::btrfs;
use scrub_rs::fs;
use scrub_rs::fs::FilesystemScrub;

use std::env;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Exit-code contract (kept small and stable so callers — e.g. the
/// Rotshield unRAID plugin — can branch on meaning, not on
/// guessing whether a non-zero code means "found problems" or "crashed").
/// The code reflects the *state of the data*, NOT which flags were passed:
/// the recovery-possibility assessment runs in every mode (plain scrub,
/// dry-run assessment, --repair), so the same disk always yields
/// the same code regardless of flags.
///   0  success — ran to completion, filesystem clean
///   1  runtime/setup error — the tool could not do its job (unopenable
///      device, missing array config, scrub pipeline failed, resolve
///      failed, etc.). Distinct from "found issues".
///   2  usage/argument error — bad CLI flags or missing required args.
///   3  issues found (plain scrub, no array) — corruption detected but no
///      reconstruction was attempted or possible. Distinct from the
///      recovered/unrecovered data-recovery outcomes below.
///   4  issues found AND all recoverable — corruption detected, but every
///      confirmed block was rebuilt successfully (or would-be-written in
///      dry-run). The expected good outcome; data is (or would be) intact.
///      Reachable ONLY when nothing was skipped (unverifiable metadata),
///      deferred (a batch whose required freeze failed — `not_frozen`), or
///      read-back-failed — any of those escalates to 5, because "all
///      recoverable" must mean "everything was actually verified and
///      written", not "we gave up on some of it silently".
///   5  issues found AND some UNRECOVERABLE — at least one confirmed block
///      could not be rebuilt (parity/gather/write failure), OR metadata
///      coverage was lost to READ (EIO) errors (which are not recovered by
///      parity, so some data may be unverified). Needs attention.
///   6  METADATA FATAL — at least one btrfs metadata node had NO good copy
///      (every DUP/RAID1 mirror failed its header checksum and no parity
///      read could recover it). The live filesystem may be serving corrupt
///      trees, so the data result above cannot be trusted. The operator
///      MUST unmount the filesystem (if still mounted) and run
///      `btrfs check --repair` offline. Highest-priority non-clean outcome:
///      it overrides the data-recovery codes because unreadable metadata
///      invalidates even a "recovered" data result.
const EXIT_RUNTIME_ERROR: u8 = 1;
const EXIT_USAGE_ERROR: u8 = 2;
const EXIT_ISSUES_FOUND: u8 = 3;
const EXIT_RECOVERED: u8 = 4;
const EXIT_RECOVER_FAILED: u8 = 5;
/// See code-6 description in the contract comment above.
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

    // --help / -h may appear as the first token (before any device).
    if dev == "--help" || dev == "-h" {
        print_help();
        return ExitCode::SUCCESS;
    }

    // Debug subcommand: dump the parsed array config and exit.  Useful while
    // developing the array/ module — cross-check against the Python
    // `get_array_config()` output.
    if dev == "--dump-array" {
        return dump_array();
    }

    // Debug subcommand: resolve a logical address to a raw-rdev location.
    // Usage: scrub-rs --resolve <device> <logical-hex>
    // Cross-check against Python's find_physical_offset + raw_offset_for.
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
    // --help / -h: print usage and the recovery-counter glossary, then exit.
    let mut args = args.peekable();
    if args.peek().map(|s| s.as_str()) == Some("--help")
        || args.peek().map(|s| s.as_str()) == Some("-h")
    {
        print_help();
        return ExitCode::SUCCESS;
    }

    // Optional byte offset of the btrfs partition inside the backing file.
    // 0 for a bare btrfs image or an array partition (/dev/nmd1p1); the
    // partition start (e.g. rdevOffset*512) for a whole-disk image or a raw
    // rdev.  No autodetection — wrong values fail loudly at the superblock
    // magic check.
    let mut base_offset: u64 = 0;
    // Recovery is ALWAYS on: every csum mismatch is reconstructed from
    // parity (read-only, free) so the operator learns whether the
    // corruption is repairable.  The only remaining choice is whether to
    // WRITE the reconstruction back to the failing disk.  We default to
    // the SAFE mode (dry-run: report + reconstruct, but never mutate the
    // disk); `--repair` opts in to actually writing the recovered blocks.
    // No `--recover` flag — there is nothing to opt out of, and hiding a
    // free read-only assessment behind a flag would only hide useful info.
    let mut dry_run = true;
    // Live-filesystem freeze for safe recovery writes.  `freeze` is ON by
    // default; it only engages when BOTH `--repair` is given (we are
    // actually writing) AND `--freeze-mount <PATH>` names the live
    // mountpoint (so we know what to freeze).  Offline/unmounted images
    // pass no mountpoint and are never frozen.  `--no-freeze` disables it
    // explicitly.
    let mut freeze_enabled = true;
    let mut freeze_mount: Option<String> = None;
    // Batched recovery tuning.  Candidates accumulate and are re-confirmed
    // + written as one batch (under a single freeze) once the batch is
    // full (`--batch-max`) or no new candidate has arrived for
    // `--batch-idle` seconds.  Defaults chosen so a single corruption
    // still flushes promptly (idle timer) while bursts are coalesced.
    let mut batch_max: usize = 64;
    let mut batch_idle: f64 = 5.0;
    // H4: wall-clock guard for the per-batch freeze window.  The freeze
    // spans every candidate's reconfirm + stripe gather across ALL other
    // disks + write + read-back; a slow/dying disk inside the gather must
    // not stall the live filesystem indefinitely.  Once the window exceeds
    // this many seconds the batch thaws and defers the remainder to the
    // next batch.
    let mut freeze_max: f64 = 60.0;
    // Optional localhost HTTP status server for the plugin: when a non-zero
    // `--status-port <n>` is given, scrub-rs serves the live error/progress
    // counters on 127.0.0.1:<n> from a background thread (see `status.rs`).
    // 0 (default) = no server, so standalone behaviour is unchanged.
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
                // Opt in to writing reconstructed blocks back to the
                // failing disk.  Default (no flag) is dry-run: assess +
                // reconstruct only, never mutate the disk.
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

    // `BtrfsScrub::open` opens the device itself; we don't need a separate
    // File handle here anymore (the old code peeked the superblock first).
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

    // Optional localhost status server (plugin integration).  The shared
    // counters are handed to both the scrub loop and the recovery writer so
    // `GET /status` reports live numbers while the run is in flight.  The
    // server runs on its own background thread; dropping its handle here
    // detaches it — the thread lives until the process exits.
    let status = Arc::new(scrub_rs::status::StatusCounters::new());
    // `device` is tracked unconditionally (also with the server off): the
    // final `status:` block printed at the end of the run carries it, so
    // the plugin can associate the log's final counters with the right disk.
    status.set_device(&dev);
    if status_port != 0 {
        status.set_state("starting");
        // A busy port (e.g. a previous run still up) is logged and skipped —
        // the scrub must never fail just because the status port is taken.
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

    // Recovery glue: the contract routes two streams through one
    // `ScrubCallbacks` impl below — `on_log` for free-form diagnostic
    // text owned by the filesystem scrub, `on_event` for the narrow
    // recovery payload (array_phys + block_size + verify closure +
    // opaque re-confirm request).  The filesystem's checksum algorithm is
    // fully encapsulated inside the `verify` closure — main never imports
    // crc32c and doesn't care what bytes the csum is.
    //
    // Recovery-possibility assessment runs in EVERY mode.  We always try
    // to load the array config and spawn the assessment pipeline so that
    // `BatchStats` (recovered / failed) is populated — this lets the exit
    // code report the *state of the data* (corruption found, and whether
    // it is recoverable) instead of reflecting which flags were passed.
    // Only the actual disk WRITE is gated by `dry_run` (i.e. `--repair`);
    // the re-confirm + parity-rebuild classification always runs.
    //
    // Graceful fallback: if there is no array config, or this device is
    // not a data disk the array recognizes, we simply skip reconstruction
    // and behave like a plain read-only scrub (the mismatch is still
    // reported and counted).  Recovery is best-effort — never a hard error.
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
    // Early canary: before committing to a full scrub + recovery pass, prove
    // the array config is actually sound.  We reconstruct the target disk's
    // superblock block from the *other* data disks + primary parity (the
    // exact machinery recovery depends on) and check whether the result
    // carries the filesystem's magic.  A match means the slot, offsets, and
    // parity are all consistent — if they weren't, every later recovery
    // attempt would be built on a misconfigured array.  This is a cheap,
    // early smoke test of the environment/config, not a correctness
    // guarantee of the data itself (the real scrub still runs in full when
    // the canary passes).  It is FATAL: a failure means the array config,
    // slot, or parity is misconfigured, so any recovery we attempt later
    // would be built on sand — we abort with EXIT_RUNTIME_ERROR rather than
    // produce misleading results.  Only runs when we have an array config
    // and a recognized data-disk slot (otherwise there's nothing to
    // reconstruct against, and a single-disk/offline run is allowed to
    // proceed).
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

    // NOTE: this line must NOT start with the word "scrubbing" — scrub.sh
    // and status.php find the run's active device by scanning the log for
    // "scrubbing <device>" markers, and a bare "scrubbing (...)" banner
    // (no device path) used to be misread as a marker, producing a phantom
    // "dry-run):cancelled" column and dropping the interrupted disk's
    // counters on manual stop.
    println!(
        "\nscan mode     : recovery assessment + {}",
        if dry_run {
            "dry-run (no writes)"
        } else {
            "REPAIR (writes enabled)"
        }
    );

    // The two contract streams route through a single `ScrubCallbacks`
    // impl.  `on_log` just `eprintln!`s whatever the filesystem scrub
    // formatted — `main` has no log fields to decode, so a future ZFS
    // implementation can use a completely different diagnostic vocabulary
    // without changing this call site.  `on_event` forwards each raw
    // candidate to the writer thread, which re-confirms + classifies it
    // (recovered / failed) in every mode.  No checksum bytes, no algorithm
    // names ever reach here.
    struct Driver {
        cfg: Option<array::config::ArrayConfig>,
        /// Always `Some` once the assessment pipeline is spawned (every
        /// mode); `on_event` forwards each raw candidate to the writer
        /// thread, which re-confirms + classifies it under a single freeze.
        tx: Option<std::sync::mpsc::SyncSender<scrub_rs::batch_recover::Msg>>,
        scrub_slot: u64,
        /// Count of candidates successfully handed to the recovery pipeline
        /// (incremented in `on_event` only on a successful send).  Compared
        /// against the writer's classified totals after the joins — the C7
        /// verdict reconciliation: a shortfall means a pipeline thread died
        /// and the verdict is silently incomplete.
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
            // Forward the raw candidate to the writer thread (runs in every
            // mode).  The writer owns re-confirmation + the freeze + the
            // (optional) write, so we only need to package the candidate here.
            let Some(tx) = self.tx.as_ref() else { return };
            let Some(cfg) = self.cfg.as_ref() else { return };
            let Some(verifier) = ev.verify.as_ref() else {
                // No stored csum → nothing to verify against; skip.
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
            // `send` blocks once one batch is buffered (depth-1 channel),
            // naturally pausing the scrub while the writer is frozen/writing.
            if let Err(e) = tx.send(scrub_rs::batch_recover::Msg::Candidate(cand)) {
                // The pipeline is gone (writer/accumulator died): the
                // candidate was NOT handed over.  The send-vs-classified
                // reconciliation after the joins turns this into a hard
                // error (the join checks catch the thread death itself).
                eprintln!("error: recovery writer thread gone: {e}");
            } else {
                self.sent.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    // `sent` counts candidates successfully handed to the recovery
    // pipeline: the shared `Arc` is cloned into the `Driver` (incremented
    // per successful send in `on_event`) and read back here after the
    // joins for the C7 verdict reconciliation.
    let sent = Arc::new(AtomicU64::new(0));
    let mut driver = Driver {
        cfg: cfg.clone(),
        tx: None,
        scrub_slot,
        sent: sent.clone(),
    };

    // Build the live-filesystem freeze controller.  It only engages when we
    // are actually writing recovered blocks (not dry-run) AND a live mount
    // was declared AND freeze is enabled.  Otherwise it is a no-op controller
    // (mountpoint = None) and `guard()` returns None, so the scrub proceeds
    // exactly as before.
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

    // Recovery-possibility assessment runs in EVERY mode.  When an array
    // config + data-disk slot is available we hand candidates to a writer
    // thread that re-confirms + classifies them (recovered / failed) in
    // batches under a single freeze.  The freeze controller is moved INTO
    // that thread; the scrub loop itself does not freeze (it just emits raw
    // candidates).  Only the actual disk WRITE is gated by `dry_run`
    // (--repair), so the recovered/failed classification is always
    // populated and the exit code reflects data state, not the flags passed.
    //
    // When there is no array (cfg == None) we fall back to the inline
    // reconfirm path: the scrub loop re-confirms each mismatch itself and
    // counts it directly.  No writer thread, no freeze, no reconstruction.
    let mut writer_handle = None;
    let mut acc_handle = None;
    if let Some(cfg) = cfg.clone() {
        // The writer thread needs its own re-confirmation handle (an
        // independent filesystem handle, not a share of the scrub's
        // reader).  The filesystem implementation builds it for us.
        let reconfirmer = match scrub.reconfirmer() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("error building recovery re-confirm handle: {e}");
                status.set_state("error");
                print_status_block(&status);
                return ExitCode::from(EXIT_RUNTIME_ERROR);
            }
        };
        // Move the freeze controller into the writer thread.
        let fc = std::mem::replace(
            &mut freeze_controller,
            scrub_rs::freeze::FreezeController::new(None),
        );
        let (tx, acc, handle) = match scrub_rs::batch_recover::spawn_pipeline(
            cfg,
            fc,
            reconfirmer,
            scrub_slot,
            dry_run,
            batch_max,
            std::time::Duration::from_secs_f64(batch_idle),
            // H4: wall-clock guard for the per-batch freeze window — a
            // slow/dying disk inside a batch's stripe gather must not
            // stall the live filesystem indefinitely.
            std::time::Duration::from_secs_f64(freeze_max),
            // Always mirror the writer's counters into the shared bank —
            // NOT only when the live status server is on: the final
            // `status:` block printed at the end of the run must carry the
            // exact recovered/failed/skipped numbers even with
            // STATUS_PORT=0 (the server is just a live view; the block is
            // the durable record).  The atomics are cheap and idle.
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
        // No array: plain scrub with inline reconfirm.  `wants_raw_candidates`
        // is false (no writer attached), so the scrub loop owns
        // mismatch/stale accounting.
        println!("recovery       : disabled (no array config / not a data disk) — plain scrub");
    }

    // The freeze lives on the writer thread in every mode, so the scrub loop
    // itself never freezes.  Batch vs inline mismatch handling is decided by
    // the driver's `wants_raw_candidates` (true when a writer is attached).
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

    // If we spawned a writer thread, signal completion and collect its
    // stats.  The `Done` message flushes any pending batch; joining waits
    // for the final freeze/thaw to finish so the FS is never left frozen.
    // `had_writer` is captured BEFORE the join below consumes the handle:
    // the recovery-summary block and the exit-code branches at the end of
    // this function must know whether the assessment pipeline ran (only
    // then are `batch_stats` and the recovered/failed classification
    // meaningful).  Regression fix: the handle was previously `take()`n
    // here, leaving the later `is_some()` checks permanently false — the
    // recovery summary never printed and exit codes 4/5 were unreachable.
    status.set_state("done");
    let had_writer = writer_handle.is_some();
    let mut batch_stats = scrub_rs::batch_recover::BatchStats::default();
    if had_writer {
        if let Some(tx) = driver.tx.take() {
            let _ = tx.send(scrub_rs::batch_recover::Msg::Done);
        }
        // Join the accumulator first (it forwards Done to the writer), then
        // the writer (which flushes the final batch and thaws the FS).  An
        // ABNORMAL join (thread panicked) is a hard runtime error (C7): the
        // verdict is incomplete and must never be reported as a clean scrub
        // or a partial-looking one.  The early return happens BEFORE
        // "scrub complete:" is printed, so the plugin's completion-marker
        // gate reports ERROR rather than trusting a misleading rc.
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
                // A HARD abort from the writer (H2: a thaw failed and the
                // filesystem may still be frozen): the verdict is
                // incomplete and must be reported as an ERROR — never as a
                // clean or partial scrub.
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

        // C7 verdict reconciliation: every candidate handed to the pipeline
        // must have been classified by the writer into exactly one bucket
        // (see the invariant documented on `BatchStats`).  A shortfall
        // means candidates vanished (dead thread, lost batch) and the
        // verdict is silently incomplete — that must be a hard error, not
        // a clean run.
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
        // Internal consistency of the mismatch sub-buckets (guards against
        // future accounting drift in `flush_batch`).
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

    // The recovery-possibility assessment runs whenever an array is
    // present, so print the summary then — it tells the operator whether
    // the corruption that was found is actually repairable.  When no array
    // was available the pipeline never ran and the counters are all zero,
    // so we skip the block to avoid implying a reconstruction happened.
    if had_writer {
        println!("\nrecovery summary:");
        println!("  recovered       : {}", batch_stats.recovered);
        println!("  failed          : {}", batch_stats.failed);
        println!("  skipped         : {}", batch_stats.skipped);
        println!("  not_corrupt     : {}", batch_stats.not_corrupt);
        println!("  not_frozen      : {}", batch_stats.not_frozen);
        println!("  readback_failed : {}", batch_stats.readback_failed);
        // C5: repairs written under a live mount leave the mounted
        // filesystem's FILE page cache potentially serving the OLD corrupt
        // bytes for ranges cached before the repair (BLKFLSBUF on the raw
        // rdev only clears the block-device buffer cache, not file pages).
        // Surface the advisory prominently so the operator knows to reboot
        // / remount before trusting reads of repaired files.  The affected
        // raw-rdev offsets are logged in the RECOVERED lines above.
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

    // `batch_stats.skipped` is part of the verdict: a candidate whose
    // re-confirmation metadata was unreadable was NEVER verified or
    // recovered — a run whose only outcomes were skipped must not report
    // clean (C3 + C7 review: the C3 fix downgrades unreadable-live-trees
    // to Unverifiable/skipped, so without this term such a run would exit
    // 0 "OK (clean)" with confirmed mismatches left unexamined).
    //
    // `stats.stale_csum_branches` is a *coverage* term (H8): CSUM_TREE
    // branches that went stale mid-scrub (normal churn, NOT a metadata
    // error) had their sectors never verified — "clean" must mean "fully
    // checked", so a non-zero value refuses exit 0 too.
    let issues_found = stats.sectors_mismatch + batch_stats.mismatch > 0
        || stats.sectors_read_error > 0
        || stats.metadata_header_errors > 0
        || stats.metadata_read_errors > 0
        || stats.stale_csum_branches > 0
        || batch_stats.skipped > 0;

    // Metadata READ errors (device EIO) are hardware, NOT checksum
    // corruption — the operator response differs, so we do NOT trigger the
    // "btrfs check --repair" advice below (that is written for checksum
    // corruption).  Print a hardware-focused message instead; the exit
    // code still reflects the coverage gap (a scrub that lost metadata
    // coverage can never report clean / fully-verified).
    if stats.metadata_read_errors > 0 {
        eprintln!(
            "\n[METADATA READ ERRORS] {} metadata node(s) failed with a READ \
             (EIO) error — the bytes could not be fetched from the device. Some \
             data may be UNVERIFIED. Check the disk hardware (SMART, cables, \
             controller), not `btrfs check`.",
            stats.metadata_read_errors
        );
    }

    // H8: CSUM-tree branches that went stale mid-scrub (freed/rewritten by
    // a live transaction while the run was in progress) are normal churn
    // but a coverage gap — their sectors were never verified this run.
    // Non-zero refuses exit 0 via `issues_found` above; explain why here.
    // Deliberately NOT a metadata error: no METADATA FATAL / `btrfs check`
    // advice, just a rerun.
    if stats.stale_csum_branches > 0 {
        eprintln!(
            "\n[COVERAGE GAP] {} CSUM-tree branch(es) went stale mid-scrub \
             (freed/rewritten while the run was in progress); their sectors were NOT \
             verified this run. Rerun the scrub to cover them — exit 0 is refused \
             while this counter is non-zero.",
            stats.stale_csum_branches
        );
    }

    // METADATA FATAL takes top priority over everything else.  A metadata
    // node with NO good copy (every DUP/RAID1 mirror failed its header
    // checksum AND no parity read could recover it) means the live
    // filesystem may be serving corrupt trees — so even a "recovered" data
    // result cannot be trusted.  We never attempt parity recovery on btrfs
    // metadata by design, so the only safe action is: unmount (if mounted)
    // and run `btrfs check --repair` offline.  `metadata_mirror_mismatches`
    // is deliberately NOT fatal here — that is the self-heal-recoverable
    // case (a good copy still exists, data is intact).
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
    } else if stats.stale_csum_branches > 0
        && stats.sectors_mismatch == 0
        && stats.sectors_read_error == 0
        && stats.metadata_read_errors == 0
        && batch_stats.mismatch == 0
        && batch_stats.skipped == 0
    {
        // Coverage-only gap (H8): the run never finished verifying
        // (CSUM-tree branches went stale mid-scrub — normal churn, not
        // corruption) and found no corruption, no read errors, no
        // candidates.  Not "all recoverable" (nothing was recovered) and
        // not "some unrecoverable" (nothing was lost): plain ISSUES FOUND
        // (warning severity) — rerun in a quieter window.  The [COVERAGE
        // GAP] message above explains.
        ExitCode::from(EXIT_ISSUES_FOUND)
    } else if had_writer
        && stats.metadata_read_errors == 0
        && batch_stats.failed == 0
        && batch_stats.skipped == 0
        && batch_stats.not_frozen == 0
        && batch_stats.readback_failed == 0
        && batch_stats.not_corrupt == 0
        && stats.stale_csum_branches == 0
    {
        // Corruption found, but every confirmed block was rebuilt
        // successfully (or would-be-written in dry-run).  The expected good
        // outcome — the data is (or would be) intact.  Mode-independent:
        // plain scrub and --repair both report this the same way for the
        // same disk.  Metadata read errors are deliberately excluded from
        // this branch: lost metadata coverage means the data result cannot
        // imply full verification (see the METADATA READ ERRORS message
        // above), so it escalates to exit 5 (some unrecoverable) instead.
        // The new gates (`skipped`/`not_frozen`/`readback_failed` == 0) are
        // the same principle applied to the recovery path: "all recoverable"
        // is only true when every confirmed block was actually verified and
        // written — a skipped/deferred/read-back-failed candidate means we
        // gave up on it, which is not "all recovered" (C2/C4/C7 + M1).
        // `stale_csum_branches == 0` extends the same honesty to coverage
        // (H8): a run whose CSUM-tree branches went stale mid-scrub never
        // verified those sectors, so "all recoverable" would be a lie — it
        // falls through to exit 5 with the [COVERAGE GAP] explanation.
        ExitCode::from(EXIT_RECOVERED)
    } else if had_writer {
        // Corruption found AND at least one block could not be rebuilt
        // (parity/gather/write failure).  Needs operator attention.
        ExitCode::from(EXIT_RECOVER_FAILED)
    } else {
        // Plain scrub (no array): corruption detected but no reconstruction
        // was attempted or possible.  Distinct outcome from the recovered
        // cases above.
        ExitCode::from(EXIT_ISSUES_FOUND)
    };

    // Final status block — always printed, no flag needed.  `status:`
    // followed by the same key=value payload the live status server serves,
    // so the plugin reads a device's exact final counters from the run log
    // with the same parser it uses for the live endpoint.
    print_status_block(&status);
    code
}

/// Print the final status block to stdout: a `status:` marker line followed
/// by the exact `key=value` payload the live status server serves (see
/// `status.rs`).  Emitted unconditionally at the end of every run, so the
/// final counters are always available from the run output — the plugin's
/// progress table fills a finished disk's column from this block.
fn print_status_block(status: &scrub_rs::status::StatusCounters) {
    println!("status:");
    print!("{}", status.snapshot());
}

/// Parse a byte offset from a string.  Accepts decimal, 0x-prefixed hex,
/// and a `+` prefix for sector multiples (e.g. `+64` means 64 sectors of
/// 512 bytes, matching how rdevOffset is reported in /proc/nmdstat).
///
/// Print usage plus the recovery-counter glossary shown by `--help`/`-h`.
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

/// A leading `-` is rejected loudly — a negative byte offset makes no
/// sense for a partition start, and silently turning it into `0` (the
/// previous behaviour) hid typos instead of failing fast.
fn parse_offset(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (sign, rest) = match s.chars().next() {
        Some('+') => (1u64, &s[1..]),
        Some('-') => return Err(format!("negative byte offsets are not allowed: {s:?}")),
        _ => (1u64, s),
    };
    let raw: u64 = if let Some(h) = rest.strip_prefix("0x") {
        u64::from_str_radix(h, 16).map_err(|e| format!("invalid hex: {e}"))?
    } else {
        rest.parse::<u64>()
            .map_err(|e| format!("invalid decimal: {e}"))?
    };
    // Only a leading `+` indicates 512-byte-sector units (matching
    // /proc/nmdstat's rdevOffset units); a plain integer is in bytes.
    let bytes = if s.starts_with('+') { raw * 512 } else { raw };
    Ok(sign * bytes)
}

/// Strip an optional `0x` prefix from a hex string argument.
fn literal_hex(s: &str) -> &str {
    s.strip_prefix("0x").unwrap_or(s)
}

/// `--dump-array` subcommand: print the parsed array config.
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

/// `--resolve <device> <logical>` subcommand: resolve a logical address to a
/// raw-rdev location via the chunk map + array config.  Cross-check against
/// the Python `find_physical_offset` + `raw_offset_for` reference.
fn resolve_cmd(device: &str, logical: u64) -> ExitCode {
    // The chunk-map / root-tree preamble is owned by `btrfs::open`; this
    // debug subcommand reuses it rather than carrying its own copy with
    // an `expect("reopen")` band-aid.
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

    // The btrfs chunk map does logical→(devid, array_phys); the array config
    // does (devid, array_phys)→(raw_rdev_path, raw_phys).  Two clean steps.
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
