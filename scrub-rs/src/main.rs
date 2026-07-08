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
use scrub_rs::recovery;

use std::env;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let dev = match args.next() {
        Some(p) => p,
        None => {
            eprintln!("usage: scrub-rs <device-or-image> [--offset <bytes>] [--recover] [--write]");
            eprintln!("       scrub-rs --dump-array");
            eprintln!("       scrub-rs --resolve <device> <logical>");
            return ExitCode::from(2);
        }
    };

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
                return ExitCode::from(2);
            }
        };
        let logical_str = match args.next() {
            Some(l) => l,
            None => {
                eprintln!("usage: scrub-rs --resolve <device> <logical>");
                return ExitCode::from(2);
            }
        };
        let logical = match u64::from_str_radix(literal_hex(&logical_str), 16) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("invalid logical {logical_str:?}: {e}");
                return ExitCode::from(2);
            }
        };
        return resolve_cmd(&device, logical);
    }

    run_scrub(dev, args)
}

fn run_scrub<I: Iterator<Item = String>>(dev: String, mut args: I) -> ExitCode {

    // Optional byte offset of the btrfs partition inside the backing file.
    // 0 for a bare btrfs image or an array partition (/dev/nmd1p1); the
    // partition start (e.g. rdevOffset*512) for a whole-disk image or a raw
    // rdev.  No autodetection — wrong values fail loudly at the superblock
    // magic check.
    let mut base_offset: u64 = 0;
    // Recovery flags.  --recover enables parity-XOR recovery for scrub
    // mismatches.  Recovery defaults to dry-run (logs what would be
    // written without touching any disk, so corruption stays in place
    // for repeatable testing); --write disables dry-run and actually
    // writes recovered blocks back to the failing disk.
    let mut recover = false;
    let mut dry_run = true;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--offset" => {
                let v = match args.next() {
                    Some(s) => s,
                    None => {
                        eprintln!("error: --offset requires a value");
                        return ExitCode::from(2);
                    }
                };
                base_offset = match parse_offset(&v) {
                    Ok(o) => o,
                    Err(e) => {
                        eprintln!("error parsing --offset {v:?}: {e}");
                        return ExitCode::from(2);
                    }
                };
            }
            "--recover" => {
                recover = true;
            }
            "--write" => {
                dry_run = false;
            }
            other => {
                eprintln!("unknown argument: {other}");
                eprintln!(
                    "usage: scrub-rs <device-or-image> [--offset <bytes>] \
                     [--recover] [--write]"
                );
                return ExitCode::from(2);
            }
        }
    }

    // `BtrfsScrub::open` opens the device itself; we don't need a separate
    // File handle here anymore (the old code peeked the superblock first).
    let mut scrub = match btrfs::BtrfsScrub::open(&dev, base_offset) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error opening btrfs filesystem: {e}");
            return ExitCode::from(1);
        }
    };
    let sb = scrub.superblock();

    {
        println!("device        : {dev}");
        println!("base offset   : 0x{:x} ({})", base_offset, base_offset);
        println!("magic         : {:?}", sb.magic);
        println!("fsid          : {}", hex(&sb.fsid));
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
        println!("csum_type     : {}", sb.csum_type);
        println!("fs extents : {} ({} bytes)", scrub.num_extents(), scrub.extent_bytes());
    }

    // Recovery glue: the contract routes two streams through one
    // `ScrubCallbacks` impl below — `on_log` for free-form diagnostic
    // text owned by the filesystem scrub, `on_event` for the narrow
    // recovery payload (array_phys + block_size + verify closure).  The
    // filesystem's checksum algorithm is fully encapsulated inside the
    // `verify` closure — main never imports crc32c and doesn't care what
    // bytes the csum is.
    let (cfg, opts, scrub_slot) = if recover {
        let loaded = match array::config::load() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("error loading array config for recovery: {e}");
                return ExitCode::from(1);
            }
        };
        let slot = array::config::slot_from_array_partition(&dev)
            .or_else(|| loaded.slot_for_raw_dev(Path::new(&dev)));
        let Some(slot) = slot else {
            eprintln!(
                "error: --recover requires a device this array config recognizes as a \
                 data disk (got {dev:?}); expected an array-partition path like \
                 /dev/nmd2p1 or a raw rdev path listed in /proc/nmdstat"
            );
            return ExitCode::from(1);
        };
        println!("\nscrubbing{}:", if dry_run { " (dry-run recovery)" } else { " (WRITE recovery)" });
        (Some(loaded), recovery::RecoverOpts { dry_run }, slot)
    } else {
        println!("\nscrubbing:");
        (None, recovery::RecoverOpts::default(), 0)
    };

    // The two contract streams route through a single `ScrubCallbacks`
    // impl.  `on_log` just `eprintln!`s whatever the filesystem scrub
    // formatted — `main` has no log fields to decode, so a future ZFS
    // implementation can use a completely different diagnostic vocabulary
    // without changing this call site.  `on_event` does the recovery glue
    // using the narrow `ScrubEvent` (array_phys + block_size + verify
    // closure); no checksum bytes, no algorithm names ever reach here.
    struct Driver {
        cfg: Option<array::config::ArrayConfig>,
        opts: recovery::RecoverOpts,
        scrub_slot: u64,
        recovered_count: u64,
        failed_count: u64,
    }
    impl fs::ScrubCallbacks for Driver {
        fn on_log(&mut self, line: &str) {
            eprintln!("{line}");
        }

        fn on_event(&mut self, ev: &fs::ScrubEvent) {
            let Some(cfg) = self.cfg.as_ref() else { return };
            let Some(verifier) = ev.verify.as_ref() else {
                // No stored csum → nothing to verify against; the scrub
                // implementation already logged the mismatch in its own
                // format via on_log, so we just skip recovery.
                return;
            };

            let block_size = ev.block_size;
            let Some(failing_dev) = cfg.data_dev(self.scrub_slot) else {
                eprintln!("  [slot {}] not a data disk in array config", self.scrub_slot);
                self.failed_count += 1;
                return;
            };
            // rdevOffset stays internal to the array layer: read_block_or_zeros
            // resolves it from `cfg` itself.  We only compute `raw_phys` here
            // for log-line display; the recovery I/O functions take `array_phys`.
            let raw_phys = ev.array_phys + cfg.raw_offset_for(failing_dev);
            let corrupt_block =
                match array::stripe::read_block_or_zeros(cfg, failing_dev, ev.array_phys, block_size) {
                    Ok(b) => b,
                    Err(e) => {
                        eprintln!("  [0x{raw_phys:x}] read failing disk: {e}");
                        self.failed_count += 1;
                        return;
                    }
                };
            let stripe_chunks =
                match array::stripe::gather_stripe(cfg, self.scrub_slot, ev.array_phys, block_size) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("  [0x{raw_phys:x}] gather stripe failed: {e}");
                        self.failed_count += 1;
                        return;
                    }
                };
            let input = recovery::RecoveryInput {
                failing_slot: self.scrub_slot,
                corrupt_block: &corrupt_block,
                other_blocks: &stripe_chunks.other_data,
                p_block: stripe_chunks.p_block.as_deref(),
                q_block: stripe_chunks.q_block.as_deref(),
                verifier: verifier.as_ref(),
            };
            let result = recovery::recover_block(&input, block_size, self.opts);
            match &result {
                recovery::RecoveryResult::Recovered { via, block } => {
                    let via_str = match via {
                        recovery::ParityPath::P => "P".to_string(),
                        recovery::ParityPath::Q => "Q".to_string(),
                        recovery::ParityPath::PQ { partner_slot } => {
                            format!("PQ(partner=slot {partner_slot})")
                        }
                    };
                    let mut written = false;
                    if !self.opts.dry_run {
                        match array::stripe::write_block(
                            cfg,
                            failing_dev,
                            ev.array_phys,
                            block,
                        ) {
                            Ok(()) => written = true,
                            Err(e) => {
                                eprintln!("  [0x{raw_phys:x}] write back failed: {e}");
                                self.failed_count += 1;
                                return;
                            }
                        }
                    }
                    eprintln!(
                        "  [0x{raw_phys:x}] RECOVERED via {via_str} {} dev={}",
                        if written { "(written)" } else { "(dry-run)" },
                        failing_dev.display(),
                    );
                    self.recovered_count += 1;
                }
                recovery::RecoveryResult::NotCorrupt => {
                    eprintln!("  [0x{raw_phys:x}] not corrupt (matches stored csum)");
                }
                recovery::RecoveryResult::Failed { reason } => {
                    eprintln!(
                        "  [0x{raw_phys:x}] FAILED: {reason:?} dev={}",
                        failing_dev.display(),
                    );
                    self.failed_count += 1;
                }
            }
        }
    }
    let mut driver = Driver {
        cfg,
        opts,
        scrub_slot,
        recovered_count: 0,
        failed_count: 0,
    };

    let stats = match scrub.run(&mut driver) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error running scrub: {e}");
            return ExitCode::from(1);
        }
    };

    println!("\nscrub complete:");
    println!("  sectors checked    : {}", stats.sectors_checked);
    println!("  sectors ok         : {}", stats.sectors_ok);
    println!("  sectors mismatch   : {}", stats.sectors_mismatch);
    println!("  sectors no csum    : {}", stats.sectors_no_csum);
    println!("  sectors read error : {}", stats.sectors_read_error);
    println!("  bytes checked      : {}", stats.bytes_checked);

    if recover {
        println!("\nrecovery summary:");
        println!("  recovered : {}", driver.recovered_count);
        println!("  failed    : {}", driver.failed_count);
    }

    if stats.sectors_mismatch > 0 || stats.sectors_read_error > 0 {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{:02x}", x)).collect()
}

/// Parse a byte offset from a string.  Accepts decimal, 0x-prefixed hex,
/// and a `+`/`-` prefix for sector multiples (e.g. `+64` means 64 sectors
/// of 512 bytes, matching how rdevOffset is reported in /proc/nmdstat).
fn parse_offset(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (sign, rest) = match s.chars().next() {
        Some('+') => (1u64, &s[1..]),
        Some('-') => (0u64, &s[1..]), // negative makes no sense here
        _ => (1u64, s),
    };
    let raw: u64 = if let Some(h) = rest.strip_prefix("0x") {
        u64::from_str_radix(h, 16)
            .map_err(|e| format!("invalid hex: {e}"))?
    } else {
        rest.parse::<u64>()
            .map_err(|e| format!("invalid decimal: {e}"))?
    };
    // A leading `+`/`-` indicates the value is in 512-byte sectors
    // (matching /proc/nmdstat's rdevOffset units).
    let bytes = if s.starts_with('+') || s.starts_with('-') {
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
            ExitCode::from(1)
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
            return ExitCode::from(1);
        }
    };

    let cfg = match array::config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error loading array config: {e}");
            return ExitCode::from(1);
        }
    };

    // The btrfs chunk map does logical→(devid, array_phys); the array config
    // does (devid, array_phys)→(raw_rdev_path, raw_phys).  Two clean steps.
    let (devid, array_phys) = match chunk_map.lookup(logical) {
        Some(loc) => loc,
        None => {
            eprintln!("error: no chunk mapping for logical 0x{logical:x}");
            return ExitCode::from(1);
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
            ExitCode::from(1)
        }
    }
}
