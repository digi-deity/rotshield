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

use std::env;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// Exit-code contract (kept small and stable so callers — e.g. the
/// btrfs-integrity-recovery unRAID plugin — can branch on meaning, not on
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
///   5  issues found AND some UNRECOVERABLE — at least one confirmed block
///      could not be rebuilt (parity/gather/write failure). Needs attention.
///   6  METADATA FATAL — at least one btrfs metadata node had NO good copy
///      (every DUP/RAID1 mirror failed its header checksum and no parity
///      read could recover it). The live filesystem may be serving corrupt
///      trees, so the data result above cannot be trusted. The operator
///      MUST unmount the filesystem (if still mounted) and run
///      `btrfs check --repair` offline. Highest-priority non-clean outcome:
///      it overrides the data-recovery codes because unreadable metadata
///      invalidates even a "recovered" data result.
const EXIT_OK: u8 = 0;
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
    while let Some(arg) = args.next() {
        match arg.as_str() {
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
                     [--repair] [--freeze-mount <path>] [--no-freeze]"
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
    let sb = scrub.superblock();

    {
        println!("device        : {dev}");
        println!("base offset   : 0x{:x} ({})", base_offset, base_offset);
        println!("magic         : {:?}", sb.magic);
        println!("fsid          : {}", btrfs::util::hex(&sb.fsid));
        println!("bytenr        : 0x{:x}", sb.bytenr);
        println!("generation   : {}", sb.generation);
        println!("root          : 0x{:x}", sb.root);
        println!("chunk_root    : 0x{:x}", sb.chunk_root);
        println!("total_bytes   : {}", sb.total_bytes);
        println!("bytes_used    : {}", sb.bytes_used);
        println!("num_devices   : {}", sb.num_devices);
        println!("sector_size   : {}", sb.sector_size);
        println!("node_size     : {}", sb.node_size);
        println!("stripesize    : {}", sb.stripesize);
        println!("csum_type     : {} ({})", sb.csum_type, scrub.csum_name());
        println!("csum sectors  : {} ({} bytes)", scrub.num_sectors(), scrub.csum_bytes());
        println!("dev extents   : {} (physical-order scrub)", scrub.num_dev_extents());
    }

    // Recovery glue: the contract routes two streams through one
    // `ScrubCallbacks` impl below — `on_log` for free-form diagnostic
    // text owned by the filesystem scrub, `on_event` for the narrow
    // recovery payload (array_phys + block_size + verify closure).  The
    // filesystem's checksum algorithm is fully encapsulated inside the
    // `verify` closure — main never imports crc32c and doesn't care what
    // bytes the csum is.
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
    println!(
        "\nscrubbing{}:",
        if dry_run { " (recovery assessment + dry-run)" } else { " (recovery assessment + REPAIR)" }
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
    }
    impl fs::ScrubCallbacks for Driver {
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
                eprintln!("  [slot {}] not a data disk in array config", self.scrub_slot);
                return;
            };
            let raw_phys = cfg.raw_phys(self.scrub_slot, ev.array_phys).expect(
                "failing_dev was just verified to be a data disk, so raw_phys must resolve",
            );
            let cand = scrub_rs::batch_recover::Candidate {
                array_phys: ev.array_phys,
                block_size: ev.block_size,
                logical: ev.logical,
                devid: ev.devid,
                stored_csum: ev.stored_csum.clone().unwrap_or_default(),
                verify: verifier.clone(),
                raw_phys,
                failing_dev: failing_dev.to_path_buf(),
            };
            // `send` blocks once two batches are buffered (depth-2 channel),
            // naturally pausing the scrub while the writer is frozen/writing.
            if let Err(e) = tx.send(scrub_rs::batch_recover::Msg::Candidate(cand)) {
                eprintln!("error: recovery writer thread gone: {e}");
            }
        }
    }
    let mut driver = Driver {
        cfg: cfg.clone(),
        tx: None,
        scrub_slot,
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
            freeze_mount.as_ref().unwrap()
        );
    } else if !dry_run && freeze_mount.is_none() {
        println!("\nfreeze         : disabled (no --freeze-mount; offline/unmounted image or not declared)");
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
        scrub.set_recover_batch(true);
        // Move the freeze controller into the writer thread.
        let fc =
            std::mem::replace(&mut freeze_controller, scrub_rs::freeze::FreezeController::new(None));
        let (tx, acc, handle) = match scrub_rs::batch_recover::spawn_pipeline(
            cfg,
            fc,
            dev.clone(),
            scrub.base_offset(),
            scrub.strategy(),
            scrub.devid(),
            scrub.fsid(),
            scrub.node_size(),
            scrub.chunk_map_clone(),
            scrub_slot,
            dry_run,
            batch_max,
            std::time::Duration::from_secs_f64(batch_idle),
        ) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("error spawning recovery pipeline: {e}");
                return ExitCode::from(EXIT_RUNTIME_ERROR);
            }
        };
        driver.tx = Some(tx);
        acc_handle = Some(acc);
        writer_handle = Some(handle);
        println!(
            "recovery       : assessment pipeline (max {} candidates/batch, {}s idle flush){}",
            batch_max, batch_idle, if dry_run { " [dry-run: no writes]" } else { " [REPAIR enabled]" }
        );
    } else {
        // No array: plain scrub with inline reconfirm.  `set_recover_batch`
        // stays false so the scrub loop owns mismatch/stale accounting.
        println!(
            "recovery       : disabled (no array config / not a data disk) — plain scrub"
        );
    }

    // The freeze lives on the writer thread in every mode, so the scrub loop
    // itself never freezes — pass None here.
    let stats = match scrub.run(&mut driver, None) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error running scrub: {e}");
            return ExitCode::from(EXIT_RUNTIME_ERROR);
        }
    };

    // If we spawned a writer thread, signal completion and collect its
    // stats.  The `Done` message flushes any pending batch; joining waits
    // for the final freeze/thaw to finish so the FS is never left frozen.
    let mut batch_stats = scrub_rs::batch_recover::BatchStats::default();
    if writer_handle.is_some() {
        if let Some(tx) = driver.tx.take() {
            let _ = tx.send(scrub_rs::batch_recover::Msg::Done);
        }
        // Join the accumulator first (it forwards Done to the writer), then
        // the writer (which flushes the final batch and thaws the FS).
        if let Some(acc) = acc_handle.take() {
            let _ = acc.join();
        }
        match writer_handle.take().unwrap().join() {
            Ok(s) => batch_stats = s,
            Err(_) => eprintln!("error: recovery writer thread panicked"),
        }
    }

    println!("\nscrub complete:");
    println!("  sectors checked    : {}", stats.sectors_checked);
    println!("  sectors ok         : {}", stats.sectors_ok);
    println!("  sectors mismatch   : {}", stats.sectors_mismatch + batch_stats.mismatch);
    println!("  sectors no csum    : {}", stats.sectors_no_csum);
    println!("  sectors stale      : {}", stats.sectors_stale + batch_stats.stale);
    println!("  sectors read error : {}", stats.sectors_read_error);
    println!("  metadata hdr errs  : {}", stats.metadata_header_errors);
    println!("  metadata mirror   : {}", stats.metadata_mirror_mismatches);
    println!("  bytes checked      : {}", stats.bytes_checked);

    // The recovery-possibility assessment runs whenever an array is
    // present, so print the summary then — it tells the operator whether
    // the corruption that was found is actually repairable.  When no array
    // was available the pipeline never ran and the counters are all zero,
    // so we skip the block to avoid implying a reconstruction happened.
    if writer_handle.is_some() {
        println!("\nrecovery summary:");
        println!("  recovered : {}", batch_stats.recovered);
        println!("  failed    : {}", batch_stats.failed);
        println!("  skipped   : {}", batch_stats.skipped);
    }

    let issues_found = stats.sectors_mismatch + batch_stats.mismatch > 0
        || stats.sectors_read_error > 0
        || stats.metadata_header_errors > 0;

    // METADATA FATAL takes top priority over everything else.  A metadata
    // node with NO good copy (every DUP/RAID1 mirror failed its header
    // checksum AND no parity read could recover it) means the live
    // filesystem may be serving corrupt trees — so even a "recovered" data
    // result cannot be trusted.  We never attempt parity recovery on btrfs
    // metadata by design, so the only safe action is: unmount (if mounted)
    // and run `btrfs check --repair` offline.  `metadata_mirror_mismatches`
    // is deliberately NOT fatal here — that is the self-heal-recoverable
    // case (a good copy still exists, data is intact).
    if stats.metadata_header_errors > 0 {
        eprintln!(
            "\n[METADATA FATAL] {} metadata node(s) had NO good copy (all mirrors \
             failed header checksum, no parity recovery attempted). The live \
             filesystem may be serving corrupt trees. UNMOUNT the filesystem \
             (if still mounted) and run `btrfs check --repair` offline.",
            stats.metadata_header_errors
        );
        return ExitCode::from(EXIT_METADATA_FATAL);
    }

    if !issues_found {
        ExitCode::SUCCESS
    } else if writer_handle.is_some() && batch_stats.failed == 0 {
        // Corruption found, but every confirmed block was rebuilt
        // successfully (or would-be-written in dry-run).  The expected good
        // outcome — the data is (or would be) intact.  Mode-independent:
        // plain scrub and --repair both report this the same way for the
        // same disk.
        ExitCode::from(EXIT_RECOVERED)
    } else if writer_handle.is_some() {
        // Corruption found AND at least one block could not be rebuilt
        // (parity/gather/write failure).  Needs operator attention.
        ExitCode::from(EXIT_RECOVER_FAILED)
    } else {
        // Plain scrub (no array): corruption detected but no reconstruction
        // was attempted or possible.  Distinct outcome from the recovered
        // cases above.
        ExitCode::from(EXIT_ISSUES_FOUND)
    }
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
    println!("  --help, -h            show this help and the recovery-counter glossary");
    println!();
    println!("recovery summary counters (always assessed when an array is present):");
    println!("  mismatch   real corruption found: live csum still disagrees with stored.");
    println!("  stale      benign churn: the live FS rewrote/freed the block (or nodatasum).");
    println!("             Re-confirm proved it was never genuine corruption — nothing written.");
    println!("  skipped    metadata for THIS sector was unreadable, so we could not safely");
    println!("             re-confirm it. Write skipped for just this candidate (per-sector,");
    println!("             not a global gate).");
    println!("  recovered  confirmed corruption rebuilt from parity and written (or would-be");
    println!("             in dry-run), with a passing read-back verify.");
    println!("  failed     confirmed corruption where the fix did not land: stripe read error,");
    println!("             parity gather error, write-back error, or parity reconstruction the");
    println!("             verifier rejected. (A successful write whose read-back verify");
    println!("             disagreed is logged as a WARNING but NOT counted here.)");
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
        u64::from_str_radix(h, 16)
            .map_err(|e| format!("invalid hex: {e}"))?
    } else {
        rest.parse::<u64>()
            .map_err(|e| format!("invalid decimal: {e}"))?
    };
    // Only a leading `+` indicates 512-byte-sector units (matching
    // /proc/nmdstat's rdevOffset units); a plain integer is in bytes.
    let bytes = if s.starts_with('+') {
        raw * 512
    } else {
        raw
    };
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
                o.as_ref().map_or("None".to_string(), |p| p.display().to_string())
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
                loc.devid, logical, loc.array_phys, loc.dev_path.display(), loc.raw_phys
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error resolving logical 0x{logical:x}: {e}");
            ExitCode::from(EXIT_RUNTIME_ERROR)
        }
    }
}