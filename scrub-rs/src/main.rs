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

use std::env;
use std::fs::File;
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

    let mut fp = match File::open(&dev) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error opening {dev}: {e}");
            return ExitCode::from(1);
        }
    };

    match btrfs::Superblock::read(&mut fp, base_offset) {
        Ok(sb) => {
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
            println!("sys_chunk_arr : {} bytes", sb.sys_chunk_array_size);
            println!("csum_type     : {}", sb.csum_type);

            let sys_chunks = btrfs::chunk::parse_sys_chunks(&sb.sys_chunks);
            println!("sys chunks    : {}", sys_chunks.len());
            let mut chunk_map = btrfs::chunk::ChunkMap::default();
            for rec in &sys_chunks {
                chunk_map.insert(rec);
            }

            let mut reader = btrfs::reader::FsReader {
                fp: std::fs::File::open(&dev).expect("reopen"),
                node_size: sb.node_size as usize,
                base_offset,
            };
            // Collect chunk records by walking the chunk tree, then insert
            // them into the (separately-owned) chunk map.  The map is
            // immutable after this and shared by reference everywhere.
            let mut chunk_records: Vec<btrfs::chunk::ChunkRecord> = Vec::new();
            if let Err(e) = btrfs::tree::walk_leaves(&mut reader, &chunk_map, sb.chunk_root, |_r, leaf, _logical| {
                for i in 0..leaf.slots.len() {
                    let slot = leaf.slots[i];
                    if slot.key.ty == btrfs::key::key_type::CHUNK_ITEM {
                        let chunk = btrfs::chunk::ChunkItem::parse(leaf.item_data(i));
                        chunk_records.push(btrfs::chunk::ChunkRecord {
                            logical: slot.key.offset,
                            chunk,
                        });
                    }
                }
                Ok(())
            }) {
                eprintln!("error walking chunk tree: {e}");
                return ExitCode::from(1);
            }
            for rec in &chunk_records {
                chunk_map.insert(rec);
            }

            println!("chunk map ({} entries):", chunk_map.len());
            chunk_map.dump();

            // Walk the root tree to find the FS_TREE and CSUM_TREE roots.
            let mut fs_root: Option<u64> = None;
            let mut csum_root: Option<u64> = None;
            if let Err(e) = btrfs::tree::walk_leaves(&mut reader, &chunk_map, sb.root, |_r, leaf, _logical| {
                for i in 0..leaf.slots.len() {
                    let slot = leaf.slots[i];
                    if slot.key.ty == btrfs::key::key_type::ROOT_ITEM {
                        let ri = btrfs::root::RootItem::parse(leaf.item_data(i));
                        match slot.key.objectid {
                            btrfs::key::objectid::FS_TREE => fs_root = Some(ri.bytenr),
                            btrfs::key::objectid::CSUM_TREE => csum_root = Some(ri.bytenr),
                            _ => {}
                        }
                    }
                }
                Ok(())
            }) {
                eprintln!("error walking root tree: {e}");
                return ExitCode::from(1);
            }
            println!("fs_root   : 0x{:x}", fs_root.unwrap_or(0));
            println!("csum_root : 0x{:x}", csum_root.unwrap_or(0));

            // Build the checksum map from the CSUM tree.
            let mut csum_map = btrfs::csum::CsumMap::new();
            if let Some(csum_root) = csum_root {
                match btrfs::csum::build_csum_map(&mut reader, &chunk_map, csum_root, &mut csum_map) {
                    Ok(n) => println!("csum entries: {n}"),
                    Err(e) => {
                        eprintln!("error walking csum tree: {e}");
                        return ExitCode::from(1);
                    }
                }
            }

            // Walk the FS tree and collect all REGULAR data extents.
            let mut extents: Vec<btrfs::extent::FileExtent> = Vec::new();
            if let Some(fs_root) = fs_root {
                if let Err(e) = btrfs::tree::walk_leaves(&mut reader, &chunk_map, fs_root, |_r, leaf, _logical| {
                    for i in 0..leaf.slots.len() {
                        let slot = leaf.slots[i];
                        if slot.key.ty == btrfs::key::key_type::EXTENT_DATA {
                            if let Some(ext) = btrfs::extent::FileExtent::parse(
                                leaf.item_data(i),
                                slot.key.objectid,
                                slot.key.offset,
                            ) {
                                extents.push(ext);
                            }
                        }
                    }
                    Ok(())
                }) {
                    eprintln!("error walking fs tree: {e}");
                    return ExitCode::from(1);
                }
            }
            let total_bytes: u64 = extents.iter().map(|e| e.num_bytes).sum();
            println!("fs extents : {} ({} bytes)", extents.len(), total_bytes);

            // Scrub: read every data sector and verify its CRC32C.
            //
            // When --recover is set, recovery happens inline — the scrub
            // callback gets a filesystem-agnostic (devid, phys) physical
            // location in each SectorResult and attempts parity XOR
            // reconstruction immediately, rather than buffering all
            // mismatches first.  This keeps memory bounded even on a disk
            // with a huge number of errors.  The array module never imports
            // from btrfs; it only sees (devid, phys, block_size).
            let mut recovered_count: u64 = 0;
            let mut failed_count: u64 = 0;

            // Every NonRAID data disk hosts its own independent
            // single-device filesystem, so the filesystem's own devid
            // (always 1) is *not* the NonRAID slot number in general — it
            // only happens to match for the disk in slot 1.  Recovery
            // needs the real slot to know which raw rdev paths in the
            // array config are "other data disks" vs. "the failing disk",
            // so resolve it once up front from the device path we were
            // given: try the array-partition naming convention first
            // (`/dev/nmd2p1` → slot 2), then fall back to matching the
            // raw-rdev path against the parsed array config (for the
            // recommended raw-rdev invocation, e.g. `/dev/loop3`).
            let (cfg, opts, scrub_slot) = if recover {
                let cfg = match array::config::load() {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("error loading array config for recovery: {e}");
                        return ExitCode::from(1);
                    }
                };
                let slot = array::config::slot_from_array_partition(&dev)
                    .or_else(|| cfg.slot_for_raw_dev(Path::new(&dev)));
                let Some(slot) = slot else {
                    eprintln!(
                        "error: --recover requires a device this array config recognizes as a \
                         data disk (got {dev:?}); expected an array-partition path like \
                         /dev/nmd2p1 or a raw rdev path listed in /proc/nmdstat"
                    );
                    return ExitCode::from(1);
                };
                let opts = array::recover::RecoverOpts { dry_run };
                println!("\nscrubbing{}:", if dry_run { " (dry-run recovery)" } else { " (WRITE recovery)" });
                (Some(cfg), opts, slot)
            } else {
                println!("\nscrubbing:");
                (None, array::recover::RecoverOpts::default(), 0)
            };

            let stats = btrfs::scrub::scrub_extents(&mut reader, &chunk_map, &csum_map, &extents, |r| {
                match r.stored_csum {
                    Some(stored) => {
                        eprintln!(
                            "  MISMATCH logical=0x{:x} devid={} array_phys=0x{:x} ino={} off=0x{:x} \
                             stored=0x{:08x} actual=0x{:08x}",
                            r.logical, r.devid, r.array_phys, r.inode, r.file_offset, stored, r.actual_csum
                        );
                    }
                    None => {
                        eprintln!(
                            "  NO_CSUM  logical=0x{:x} devid={} array_phys=0x{:x} ino={} off=0x{:x} \
                             actual=0x{:08x}",
                            r.logical, r.devid, r.array_phys, r.inode, r.file_offset, r.actual_csum
                        );
                    }
                }

                // Inline recovery: only when --recover is set and the sector
                // has a stored csum to verify against.  Uses only the
                // filesystem-agnostic (devid, array_phys) — no btrfs imports.
                // resolve() adds rdevOffset to reach raw-rdev space.
                let Some(cfg) = cfg.as_ref() else { return };
                let Some(expected_csum) = r.stored_csum else {
                    eprintln!("  [0x{:x}] no stored csum — nothing to verify against", r.array_phys);
                    return;
                };

                let location = match array::resolve::resolve(cfg, scrub_slot, r.array_phys) {
                    Ok(l) => l,
                    Err(e) => {
                        eprintln!("  [slot {} array_phys=0x{:x}] resolve failed: {e}", scrub_slot, r.array_phys);
                        failed_count += 1;
                        return;
                    }
                };
                let result = array::recover::recover_sector(
                    cfg,
                    &location,
                    expected_csum,
                    btrfs::superblock::BTRFS_SECTOR_SIZE,
                    opts,
                );
                match &result {
                    array::recover::RecoveryResult::Recovered { location, via, recovered_csum, written, .. } => {
                        let via_str = match via {
                            array::recover::ParityPath::P => "P".to_string(),
                            array::recover::ParityPath::Q => "Q".to_string(),
                            array::recover::ParityPath::PQ { partner_slot } => {
                                format!("PQ(partner=slot {partner_slot})")
                            }
                        };
                        eprintln!(
                            "  [0x{:x}] RECOVERED via {} {} csum=0x{:08x} dev={}",
                            location.raw_phys,
                            via_str,
                            if *written { "(written)" } else { "(dry-run)" },
                            recovered_csum,
                            location.dev_path.display(),
                        );
                        recovered_count += 1;
                    }
                    array::recover::RecoveryResult::NotCorrupt { location, on_disk_csum, expected_csum } => {
                        eprintln!(
                            "  [0x{:x}] not corrupt (on-disk 0x{:08x} == metadata 0x{:08x})",
                            location.raw_phys, on_disk_csum, expected_csum,
                        );
                    }
                    array::recover::RecoveryResult::Failed { location, reason } => {
                        eprintln!(
                            "  [0x{:x}] FAILED: {:?} dev={}",
                            location.raw_phys, reason, location.dev_path.display(),
                        );
                        failed_count += 1;
                    }
                }
            });

            println!("\nscrub complete:");
            println!("  sectors checked    : {}", stats.sectors_checked);
            println!("  sectors ok         : {}", stats.sectors_ok);
            println!("  sectors mismatch   : {}", stats.sectors_mismatch);
            println!("  sectors no csum    : {}", stats.sectors_no_csum);
            println!("  sectors read error : {}", stats.sectors_read_error);
            println!("  bytes checked      : {}", stats.bytes_checked);

            if recover {
                println!("\nrecovery summary:");
                println!("  recovered : {}", recovered_count);
                println!("  failed    : {}", failed_count);
            }

            if stats.sectors_mismatch > 0 || stats.sectors_read_error > 0 {
                ExitCode::from(1)
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(e) => {
            eprintln!("error reading superblock: {e}");
            ExitCode::from(1)
        }
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
    let mut fp = match File::open(device) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error opening {device}: {e}");
            return ExitCode::from(1);
        }
    };
    let sb = match btrfs::Superblock::read(&mut fp, 0) {
        Ok(sb) => sb,
        Err(e) => {
            eprintln!("error reading superblock: {e}");
            return ExitCode::from(1);
        }
    };
    let sys_chunks = btrfs::chunk::parse_sys_chunks(&sb.sys_chunks);
    let mut chunk_map = btrfs::chunk::ChunkMap::default();
    for rec in &sys_chunks {
        chunk_map.insert(rec);
    }
    let mut reader = btrfs::reader::FsReader {
        fp: File::open(device).expect("reopen"),
        node_size: sb.node_size as usize,
        base_offset: 0,
    };
    let mut chunk_records: Vec<btrfs::chunk::ChunkRecord> = Vec::new();
    if let Err(e) = btrfs::tree::walk_leaves(&mut reader, &chunk_map, sb.chunk_root, |_r, leaf, _logical| {
        for i in 0..leaf.slots.len() {
            let slot = leaf.slots[i];
            if slot.key.ty == btrfs::key::key_type::CHUNK_ITEM {
                let chunk = btrfs::chunk::ChunkItem::parse(leaf.item_data(i));
                chunk_records.push(btrfs::chunk::ChunkRecord {
                    logical: slot.key.offset,
                    chunk,
                });
            }
        }
        Ok(())
    }) {
        eprintln!("error walking chunk tree: {e}");
        return ExitCode::from(1);
    }
    for rec in &chunk_records {
        chunk_map.insert(rec);
    }

    let cfg = match array::config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error loading array config: {e}");
            return ExitCode::from(1);
        }
    };

    // resolve_cmd: translate a btrfs logical address to a raw-rdev location.
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
