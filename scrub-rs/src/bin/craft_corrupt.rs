//! Corruption-crafting tool for parity-recovery tests: flips a byte in a
//! chosen sector of a file on the array and rewrites P/Q parity per the
//! requested scenario (data-only, baked parity, two-disk corruption).
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use scrub_rs::array::config;
use scrub_rs::btrfs;

// 4 KiB btrfs sector — the corruption and parity granularity.
const BLOCK: usize = btrfs::superblock::BTRFS_SECTOR_SIZE;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// Corruption scenario: which blocks are corrupted and whether parity is
// recomputed from the corrupt data ("baked in").
enum Targets {
    // Data block only; P and Q stay consistent with the original.
    Data,

    // P recomputed from the corrupt data — P-only recovery cannot succeed.
    BakeP,

    // Q recomputed from the corrupt data — Q-only recovery cannot succeed.
    BakeQ,

    // Both P and Q recomputed from the corrupt data.
    BakeBoth,

    // Data and one partner disk corrupted; P and Q left intact (two-disk solve).
    TwoDisk,
}

impl Targets {
    fn parse(s: &str) -> Result<Self, String> {
        match s {
            "data" => Ok(Self::Data),
            "bake-p" => Ok(Self::BakeP),
            "bake-q" => Ok(Self::BakeQ),
            "bake-both" => Ok(Self::BakeBoth),
            "two-disk" => Ok(Self::TwoDisk),
            other => Err(format!(
                "unknown --targets value: {other} (expected: data, bake-p, bake-q, bake-both, two-disk)"
            )),
        }
    }
}

struct Opts {
    sector: usize,
    byte_offset: usize,
    flip: u8,
    targets: Targets,

    // Explicit partner slot for TwoDisk (default: first other data disk).
    partner_slot: Option<u64>,
    backup: Option<PathBuf>,
    // When set, restore from this backup and exit instead of corrupting.
    restore: Option<PathBuf>,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            sector: 0,
            byte_offset: 100,
            flip: 0x5A,
            targets: Targets::BakeP,
            partner_slot: None,
            backup: None,
            restore: None,
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        usage();
        return ExitCode::from(2);
    }
    let mut opts = Opts::default();
    let mut positional: Vec<String> = Vec::new();
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--sector" => {
                opts.sector = iter
                    .next()
                    .expect("--sector requires a value")
                    .parse()
                    .expect("invalid --sector");
            }
            "--byte" => {
                opts.byte_offset = iter
                    .next()
                    .expect("--byte requires a value")
                    .parse()
                    .expect("invalid --byte");
            }
            "--flip" => {
                let v = iter.next().expect("--flip requires a value");
                opts.flip = u64::from_str_radix(v.trim_start_matches("0x"), 16)
                    .expect("invalid --flip") as u8;
            }
            "--targets" => {
                opts.targets = Targets::parse(iter.next().expect("--targets requires a value"))
                    .unwrap_or_else(|e| {
                        eprintln!("{e}");
                        std::process::exit(2);
                    });
            }
            "--partner" => {
                opts.partner_slot = Some(
                    iter.next()
                        .expect("--partner requires a value")
                        .parse()
                        .expect("invalid --partner slot"),
                );
            }
            "--backup" => {
                opts.backup = Some(PathBuf::from(
                    iter.next().expect("--backup requires a value"),
                ));
            }
            "--restore" => {
                opts.restore = Some(PathBuf::from(
                    iter.next().expect("--restore requires a value"),
                ));
            }
            "-h" | "--help" => {
                usage();
                return ExitCode::SUCCESS;
            }
            other if other.starts_with("--") => {
                eprintln!("unknown option: {other}");
                usage();
                return ExitCode::from(2);
            }
            _ => positional.push(a.clone()),
        }
    }

    if positional.len() != 2 {
        eprintln!("error: expected exactly two positional args: <array-partition-dev> <file-path>");
        usage();
        return ExitCode::from(2);
    }
    let dev = positional[0].clone();
    let file_path = positional[1].clone();

    // --restore takes precedence: undo a previous corruption and exit.
    if let Some(backup) = opts.restore.clone() {
        return restore_cmd(&dev, &file_path, &backup, &opts);
    }

    match corrupt_cmd(&dev, &file_path, &opts) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}

fn usage() {
    eprintln!(
        "usage: craft-corrupt <array-partition-dev> <file-path> [options]\n\n  options:\n    --sector <N>       which 4 KiB sector of the file (0-based, default 0)\n    --byte <N>         byte within sector to flip (default 100)\n    --flip <0xHH>      XOR value (default 0x5a)\n    --targets <list>   data | bake-p | bake-q | bake-both | two-disk (default: bake-p)\n    --partner <slot>   partner slot for two-disk (default: first non-failing data disk)\n    --backup <file>    save original block before corrupting\n    --restore <file>   restore from a backup file and exit"
    );
}

struct FsContext {
    reader: btrfs::reader::FsReader,
    chunk_map: btrfs::chunk::ChunkMap,
    fs_root: u64,
    #[allow(dead_code)]
    csum_root: u64,
    #[allow(dead_code)]
    sb: btrfs::Superblock,
}

fn open_fs(dev: &str, base_offset: u64) -> io::Result<FsContext> {
    let ctx = btrfs::open(dev, base_offset)?;
    Ok(FsContext {
        reader: ctx.reader,
        chunk_map: ctx.chunk_map,
        fs_root: ctx.roots.fs_root,
        csum_root: ctx.roots.csum_root,
        sb: ctx.superblock,
    })
}

/// Locate the on-disk position of the file's Nth data sector: walk the FS tree
/// for data extents, pick the extent containing sector `sector`, then map its
/// logical address through the chunk map.
/// Returns (btrfs devid, array_phys, logical, inode).
fn find_file_sector(ctx: &mut FsContext, sector: usize) -> io::Result<(u64, u64, u64, u64)> {
    let mut extents: Vec<btrfs::extent::FileExtent> = Vec::new();
    // Only EXTENT_DATA items matter; the metadata-error callbacks are
    // irrelevant to a corruption tool.
    btrfs::tree::walk_leaves(
        &mut ctx.reader,
        &ctx.chunk_map,
        ctx.fs_root,
        |_r, leaf, _logical| {
            for i in 0..leaf.slots.len() {
                let slot = leaf.slots[i];
                if slot.key.ty == btrfs::key::key_type::EXTENT_DATA
                    && let Some(ext) = btrfs::extent::FileExtent::parse(
                        leaf.item_data(i),
                        slot.key.objectid,
                        slot.key.offset,
                    )
                {
                    extents.push(ext);
                }
            }
            Ok(())
        },
        |_logical| {},
        |_logical| {},
        |_logical| {},
        |_logical| {},
    )?;
    if extents.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no data extents in FS tree",
        ));
    }
    let mut remaining_sector = sector;
    let mut chosen: Option<&btrfs::extent::FileExtent> = None;
    for ext in &extents {
        let n_sectors = (ext.num_bytes / BLOCK as u64) as usize;
        if remaining_sector < n_sectors {
            chosen = Some(ext);
            break;
        }
        remaining_sector -= n_sectors;
    }
    let ext = chosen.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("sector {sector} is past the end of all extents"),
        )
    })?;
    let logical = ext.disk_start() + (remaining_sector as u64) * BLOCK as u64;
    let (devid, array_phys) = ctx.chunk_map.lookup(logical).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("no chunk mapping for logical 0x{logical:x}"),
        )
    })?;
    Ok((devid, array_phys, logical, ext.inode))
}

/// Drop the kernel page cache so subsequent reads see the just-written bytes.
fn drop_caches() {
    if let Ok(mut f) = fs::OpenOptions::new()
        .write(true)
        .open("/proc/sys/vm/drop_caches")
    {
        let _ = f.write_all(b"1");
    }
}

fn corrupt_cmd(dev: &str, file_path: &str, opts: &Opts) -> io::Result<()> {
    let mut ctx = open_fs(dev, 0)?;
    let (_fs_devid, array_phys, logical, inode) = find_file_sector(&mut ctx, opts.sector)?;
    let cfg = config::load()?;

    // The chunk map's devid is the btrfs device id (always 1 for a
    // single-device filesystem), not the NonRAID slot — take the slot
    // from the array-partition path instead.
    let devid = config::slot_from_array_partition(dev).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("cannot determine NonRAID slot from device path {dev:?} (expected an array-partition path like /dev/nmd2p1)"),
        )
    })?;
    let Some(dev_path) = cfg.data_dev(devid) else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("devid {devid} not in array config"),
        ));
    };
    let dev_path = dev_path.to_path_buf();
    println!("file: {file_path}  inode={inode}  sector={}", opts.sector);
    println!(
        "  devid={devid}  logical=0x{logical:x}  array_phys=0x{array_phys:x}  dev={}",
        dev_path.display()
    );

    // Snapshot the data block and both parity blocks before touching anything.
    let orig = scrub_rs::array::stripe::read_block_or_zeros(&cfg, &dev_path, array_phys, BLOCK)?;
    let orig_csum = crc32c::crc32c(&orig);
    let p_path = cfg
        .parity_p
        .as_ref()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no P disk"))?;
    let q_path = cfg
        .parity_q
        .as_ref()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no Q disk"))?;
    let p_before = scrub_rs::array::stripe::read_block_or_zeros(&cfg, p_path, array_phys, BLOCK)?;
    let q_before = scrub_rs::array::stripe::read_block_or_zeros(&cfg, q_path, array_phys, BLOCK)?;

    // Warn (don't fail) when live parity already disagrees with the data —
    // a stale array would make the test meaningless.
    let p_expected = scrub_rs::array::parity::compute_p(&cfg, array_phys, BLOCK)?;
    let q_expected = scrub_rs::array::parity::compute_q(&cfg, array_phys, BLOCK)?;
    if p_before != p_expected {
        eprintln!(
            "warning: P does not match XOR(data) — array P not in sync, test may be meaningless"
        );
    }
    if q_before != q_expected {
        eprintln!(
            "warning: Q does not match GF syndrome — array Q not in sync, test may be meaningless"
        );
    }

    // Flip the chosen byte; reject the flip if it leaves the checksum
    // unchanged (the scrub would not notice the corruption).
    let mut corrupt = orig.clone();
    corrupt[opts.byte_offset] ^= opts.flip;
    let corrupt_csum = crc32c::crc32c(&corrupt);
    if corrupt_csum == orig_csum {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "flip produced identical checksum — choose a different --byte/--flip",
        ));
    }
    println!("  original csum:  0x{orig_csum:08x}");
    println!(
        "  corrupt csum:   0x{corrupt_csum:08x}  (byte {} ^= 0x{:02x})",
        opts.byte_offset, opts.flip
    );

    // TwoDisk: also corrupt a partner disk at the same offset and remember
    // its identity so --restore can undo it.
    let mut partner: Option<(u64, PathBuf, Vec<u8>, Vec<u8>)> = None;
    if opts.targets == Targets::TwoDisk {
        let chosen_slot =
            match opts.partner_slot {
                Some(s) => s,
                None => *cfg.data_devs.keys().find(|s| **s != devid).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "no partner data disk")
                })?,
            };
        let Some(p_path) = cfg.data_dev(chosen_slot) else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("partner slot {chosen_slot} not in array config"),
            ));
        };
        let p_path = p_path.to_path_buf();
        let p_orig =
            scrub_rs::array::stripe::read_block_or_zeros(&cfg, &p_path, array_phys, BLOCK)?;

        let mut p_corrupt = p_orig.clone();
        // Flip a different byte with a different mask so the partner's
        // corruption is distinct from the target's.
        p_corrupt[opts.byte_offset.wrapping_add(1) % BLOCK] ^= opts.flip ^ 0x1;
        if crc32c::crc32c(&p_corrupt) == crc32c::crc32c(&p_orig) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "partner flip produced identical checksum — pick another --flip",
            ));
        }
        println!(
            "  partner: devid={chosen_slot} array_phys=0x{array_phys:x} dev={}",
            p_path.display()
        );

        if let Some(bp) = &opts.backup {
            let pp = bp.with_extension("partner");
            fs::write(&pp, &p_orig)?;
            println!("  partner backup: {}", pp.display());
        }
        partner = Some((chosen_slot, p_path, p_orig, p_corrupt));
    }

    if let Some(bp) = &opts.backup {
        fs::write(bp, &orig)?;
        println!("  backup: {}", bp.display());
    }

    let (new_p, new_q): (Vec<u8>, Vec<u8>) = match opts.targets {
        // Corrupt data only; parity keeps matching the original data.
        Targets::Data => {
            println!(
                "  targets=data: corrupting data block only; P and Q left consistent with original"
            );
            (p_before.clone(), q_before.clone())
        }
        // Baked-in P: recompute P from the corrupt bytes.
        Targets::BakeP => {
            let p = scrub_rs::array::parity::compute_p_with_override(
                &cfg, devid, &corrupt, array_phys, BLOCK,
            )?;
            println!(
                "  targets=bake-p: P recomputed from corrupt data (P-only recovery will FAIL); Q intact"
            );
            (p, q_before.clone())
        }
        // Baked-in Q: recompute Q from the corrupt bytes.
        Targets::BakeQ => {
            let q = scrub_rs::array::parity::compute_q_with_override(
                &cfg, devid, &corrupt, array_phys, BLOCK,
            )?;
            println!(
                "  targets=bake-q: Q recomputed from corrupt data (Q-only recovery will FAIL); P intact"
            );
            (p_before.clone(), q)
        }
        // Both parity disks baked.
        Targets::BakeBoth => {
            let p = scrub_rs::array::parity::compute_p_with_override(
                &cfg, devid, &corrupt, array_phys, BLOCK,
            )?;
            let q = scrub_rs::array::parity::compute_q_with_override(
                &cfg, devid, &corrupt, array_phys, BLOCK,
            )?;
            println!(
                "  targets=bake-both: P and Q both recomputed from corrupt data (NEITHER single-parity path can recover)"
            );
            (p, q)
        }
        // Leave P and Q untouched — they still reflect the original data.
        Targets::TwoDisk => {
            // Prove a partner was selected (unwrap panics otherwise).
            let (_p_slot, _pp, _p_orig, _p_corrupt) = partner.as_ref().unwrap();
            println!(
                "  targets=two-disk: target + partner both corrupt; P and Q LEFT INTACT (still reflect original data); single-parity paths FAIL; PQ 2-disk solve is the only path that can recover the target"
            );
            (p_before.clone(), q_before.clone())
        }
    };

    // Write the corrupt data, then rewrite only the parity that changed.
    scrub_rs::array::stripe::write_block(&cfg, &dev_path, array_phys, &corrupt)?;
    if new_p != p_before {
        scrub_rs::array::stripe::write_block(&cfg, p_path, array_phys, &new_p)?;
    }
    if new_q != q_before {
        scrub_rs::array::stripe::write_block(&cfg, q_path, array_phys, &new_q)?;
    }
    if let Some((p_slot, p_path, _p_orig, p_corrupt)) = &partner {
        scrub_rs::array::stripe::write_block(&cfg, p_path, array_phys, p_corrupt)?;
        println!(
            "  wrote corrupt block to partner slot {p_slot} ({})",
            p_path.display()
        );
    }

    drop_caches();

    let dev_name = dev_path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    println!("\nnow run:");
    println!("  scrub-rs /dev/{dev_name} --offset +64");
    let expected = match opts.targets {
        Targets::Data => "RECOVERED via P (or Q — both work)",
        Targets::BakeP => "FAILED via P (ParityBakedIn), RECOVERED via Q",
        Targets::BakeQ => "RECOVERED via P (Q is baked in, fallback not needed but P works)",
        Targets::BakeBoth => "FAILED: AllPathsFailed (P baked in, Q baked in)",
        Targets::TwoDisk => {
            "MISMATCH, both single-parity paths FAIL, RECOVERED via PQ(partner=slot N)"
        }
    };
    println!("expected: MISMATCH at 0x{array_phys:x}, {expected}");
    Ok(())
}

fn restore_cmd(dev: &str, _file_path: &str, backup: &Path, opts: &Opts) -> ExitCode {
    let cfg = match config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error loading array config: {e}");
            return ExitCode::from(1);
        }
    };
    let mut ctx = match open_fs(dev, 0) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error opening filesystem: {e}");
            return ExitCode::from(1);
        }
    };
    let (_fs_devid, array_phys, _logical, _inode) = match find_file_sector(&mut ctx, opts.sector) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error finding file sector: {e}");
            return ExitCode::from(1);
        }
    };

    // Slot from the array-partition path, as in corrupt_cmd.
    let Some(devid) = config::slot_from_array_partition(dev) else {
        eprintln!(
            "cannot determine NonRAID slot from device path {dev:?} (expected an array-partition path like /dev/nmd2p1)"
        );
        return ExitCode::from(1);
    };
    let Some(dev_path) = cfg.data_dev(devid) else {
        eprintln!("devid {devid} not in array config");
        return ExitCode::from(1);
    };
    let orig = match fs::read(backup) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error reading backup {}: {e}", backup.display());
            return ExitCode::from(1);
        }
    };
    if orig.len() != BLOCK {
        eprintln!("backup file is {} bytes, expected {BLOCK}", orig.len());
        return ExitCode::from(1);
    }

    if let Err(e) = scrub_rs::array::stripe::write_block(&cfg, dev_path, array_phys, &orig) {
        eprintln!("error writing restored block: {e}");
        return ExitCode::from(1);
    }
    let partner_backup = backup.with_extension("partner");
    if partner_backup.exists() {
        let p_orig = match fs::read(&partner_backup) {
            Ok(d) => d,
            Err(e) => {
                eprintln!(
                    "error reading partner backup {}: {e}",
                    partner_backup.display()
                );
                return ExitCode::from(1);
            }
        };
        if p_orig.len() != BLOCK {
            eprintln!("partner backup is {} bytes, expected {BLOCK}", p_orig.len());
            return ExitCode::from(1);
        }

        // Find the corrupted partner disk: scan data disks for one whose
        // block no longer matches the saved original, then restore it.
        for (slot, p_path) in &cfg.data_devs {
            if *slot == devid {
                continue;
            }
            let cur =
                match scrub_rs::array::stripe::read_block_or_zeros(&cfg, p_path, array_phys, BLOCK)
                {
                    Ok(b) => b,
                    Err(_) => continue,
                };
            if cur.len() == BLOCK && cur != p_orig {
                if let Err(e) =
                    scrub_rs::array::stripe::write_block(&cfg, p_path, array_phys, &p_orig)
                {
                    eprintln!("error restoring partner block on slot {slot}: {e}");
                    return ExitCode::from(1);
                }
                println!("restored partner block on slot {slot}");
                break;
            }
        }
    }
    // Recompute P and Q from the restored original data and rewrite them.
    let p = match scrub_rs::array::parity::compute_p(&cfg, array_phys, BLOCK) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error computing P: {e}");
            return ExitCode::from(1);
        }
    };
    let q = match scrub_rs::array::parity::compute_q(&cfg, array_phys, BLOCK) {
        Ok(q) => q,
        Err(e) => {
            eprintln!("error computing Q: {e}");
            return ExitCode::from(1);
        }
    };
    if let Some(p_path) = cfg.parity_p.as_ref()
        && let Err(e) = scrub_rs::array::stripe::write_block(&cfg, p_path, array_phys, &p)
    {
        eprintln!("error writing P: {e}");
        return ExitCode::from(1);
    }
    if let Some(q_path) = cfg.parity_q.as_ref()
        && let Err(e) = scrub_rs::array::stripe::write_block(&cfg, q_path, array_phys, &q)
    {
        eprintln!("error writing Q: {e}");
        return ExitCode::from(1);
    }
    drop_caches();
    println!("restored block at array_phys=0x{array_phys:x} and recomputed P+Q from original data");
    ExitCode::SUCCESS
}
