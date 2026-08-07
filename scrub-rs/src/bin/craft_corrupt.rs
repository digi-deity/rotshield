//! `craft-corrupt` — convenience utility for crafting controlled data
//! corruptions on a NonRAID/Unraid array, for testing `scrub-rs` parity
//! recovery.
//!
//! # Usage
//!
//! ```text
//! craft-corrupt <array-partition-dev> <file-path> [options]
//!
//! Options:
//!   --sector <N>       Which 4 KiB sector of the file to corrupt (0-based).
//!                      Default: 0 (first sector).
//!   --byte <N>         Which byte within the sector to flip.  Default: 100.
//!   --flip <0xHH>      Byte value to XOR into the target byte.  Default: 0x5a.
//!   --targets <list>   What to corrupt / how to handle parity:
//!                        data   = corrupt the data block only (P & Q stay
//!                                 consistent with original — P-only recovery
//!                                 works, Q is also valid)
//!                        bake-p = corrupt data AND recompute P from the
//!                                 corrupt data so P-only recovery fails
//!                                 (P is "baked in"); Q left intact so Q
//!                                 recovery is the only path
//!                        bake-q = corrupt data AND recompute Q from the
//!                                 corrupt data so Q-only recovery fails;
//!                                 P left intact so P recovery is the only
//!                                 path
//!                        bake-both = corrupt data and recompute BOTH P and
//!                                 Q from the corrupt data — neither path
//!                                 can recover (sanity check for the
//!                                 unrecoverable case)
//!                      Default: bake-p (the most useful test: exercises
//!                      the Q fallback path).
//!   --backup <file>    Write the original 4 KiB block to this file before
//!                      corrupting, for `--restore` later.
//!   --restore <file>   Restore the original block from a backup file and
//!                      exit (no corruption applied).  Recomputes P and Q
//!                      from the restored data so the array is consistent
//!                      again.
//! ```
//!
//! All I/O is in **raw-rdev space** (opens `/dev/loopN` directly, adds the
//! per-disk `rdevOffset`), bypassing the array driver so the corruption is
//! visible to a raw-rdev scrub.  See
//! `memories/repo/scrub-rs-testing-recipes.md` for why scrubbing the array
//! partition would mask the corruption.
//!
//! Uses the `scrub_rs` library directly (btrfs chunk tree + FS tree walk)
//! to locate the file's extents — no subprocess or Python dependency.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use scrub_rs::array::config;
use scrub_rs::btrfs;

const BLOCK: usize = btrfs::superblock::BTRFS_SECTOR_SIZE;

/// What to corrupt and how to handle the parity disks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Targets {
    /// Corrupt the data block only; leave P and Q consistent with the
    /// original data (both recovery paths work).
    Data,
    /// Corrupt data and bake P from the corrupt data; Q left intact.
    /// P-only recovery fails (baked in), Q recovery is the only path.
    BakeP,
    /// Corrupt data and bake Q from the corrupt data; P left intact.
    /// Q-only recovery fails, P recovery is the only path.
    BakeQ,
    /// Corrupt data and bake BOTH P and Q from the corrupt data.
    /// Neither single-parity path can recover — sanity check for the
    /// unrecoverable case.
    BakeBoth,
    /// Corrupt the target file's data disk AND corrupt one other data disk
    /// (the partner) at the same array_phys offset, then bake BOTH P and Q
    /// from the corrupt data.  Single-parity P and Q both fail because they
    /// each include the *partner's* bad bytes; the PQ 2-disk solve (using
    /// P and Q simultaneously) is the only path that can recover the target
    /// disk.  `--partner <slot>` selects which other data disk to corrupt
    /// (default: the first non-failing data disk).
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
    /// For --targets two-disk: which other data slot to corrupt as the
    /// partner.  None = auto-pick the first non-failing data disk.
    partner_slot: Option<u64>,
    backup: Option<PathBuf>,
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

    // --restore: undo a prior corruption from a backup file.
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

// --- btrfs extent lookup ---------------------------------------------------

/// A btrfs filesystem opened for the corruption-crafting utility, wrapping
/// the shared [`btrfs::open`] result so the FS-tree walk (`find_file_sector`)
/// can keep borrowing `reader` and `chunk_map` mutably/immutably.
///
/// `csum_root` and `sb` are carried because [`btrfs::open`] locates them
/// anyway; they're unused by this binary today but cheaper to keep than to
/// re-open the device a second time.
struct FsContext {
    reader: btrfs::reader::FsReader,
    chunk_map: btrfs::chunk::ChunkMap,
    fs_root: u64,
    #[allow(dead_code)]
    csum_root: u64,
    #[allow(dead_code)]
    sb: btrfs::Superblock,
}

/// Open `dev` (an array partition or a raw image) and build the chunk map
/// and locate the FS/CSUM tree roots via the shared [`btrfs::open`].  The
/// chunk-map / root-tree walk is owned there now — this binary no longer
/// carries its own copy.
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

/// Find the physical location of the `sector`-th 4 KiB sector of the first
/// REGULAR data extent in the FS tree.  Returns `(devid, array_phys,
/// logical, inode)`.
///
/// For a freshly-created file on an otherwise-empty filesystem this is the
/// file's only extent.  For multi-extent files we walk the extents in
/// order, summing `num_bytes` until we reach the target sector.
fn find_file_sector(ctx: &mut FsContext, sector: usize) -> io::Result<(u64, u64, u64, u64)> {
    let mut extents: Vec<btrfs::extent::FileExtent> = Vec::new();
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
        // craft-corrupt only needs the file's extents; it doesn't count
        // metadata-header errors.
        |_logical| {},
        // Stale (freed/repurposed) nodes are normal churn, never errors —
        // not counted here either.
        |_logical| {},
        // Mirror-divergence reporting is not needed for extent enumeration.
        |_logical| {},
        // Read (EIO) errors are not counted for extent enumeration either.
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

// --- raw-rdev I/O ----------------------------------------------------------
// Reads/writes go through `array::stripe`; parity syndromes through
// `array::parity`.  This binary used to carry its own copies of all of
// these — the shared versions resolve `rdevOffset` from `cfg` internally,
// so call sites pass `(cfg, path, array_phys)` and stay out of the
// per-disk header business.  No local parity helpers remain — every XOR
// lives in `array::parity` now.

fn drop_caches() {
    if let Ok(mut f) = fs::OpenOptions::new()
        .write(true)
        .open("/proc/sys/vm/drop_caches")
    {
        let _ = f.write_all(b"1");
    }
}

// --- subcommands -----------------------------------------------------------

fn corrupt_cmd(dev: &str, file_path: &str, opts: &Opts) -> io::Result<()> {
    // 1. Open the btrfs filesystem and find the target file's extent.
    let mut ctx = open_fs(dev, 0)?;
    let (_fs_devid, array_phys, logical, inode) = find_file_sector(&mut ctx, opts.sector)?;
    let cfg = config::load()?;
    // Every NonRAID data disk hosts its own independent single-device
    // filesystem, so `_fs_devid` (the filesystem's own devid) is always 1
    // regardless of which slot this disk actually occupies — it must NOT
    // be used to look up the array config.  Derive the real slot from the
    // array-partition device name instead (e.g. `/dev/nmd2p1` -> slot 2).
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

    // 2. Read the original block and the parity disks (raw-rdev paths
    //    resolved by the array layer; `block_size` = the btrfs sector size).
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

    // 3. Sanity: P and Q must currently be consistent with the original data.
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

    // 4. Build the corrupt version.
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

    // 4b. For --targets two-disk, pick a partner disk and corrupt it too at
    //     the same array_phys offset.  Both P and Q get recomputed from
    //     the (now doubly-corrupt) data blocks below, so single-parity P
    //     and Q each inherit the *partner's* bad bytes and fail; the PQ
    //     2-disk solve (using P and Q together) is the only path that can
    //     recover the target disk.
    let mut partner: Option<(u64, PathBuf, Vec<u8>, Vec<u8>)> = None; // (slot, path, orig, corrupt)
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
        // Flip a different byte on the partner so the two corruptions are
        // genuinely independent.
        let mut p_corrupt = p_orig.clone();
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
        // Back up the partner's original block alongside the target's.
        if let Some(bp) = &opts.backup {
            let pp = bp.with_extension("partner");
            fs::write(&pp, &p_orig)?;
            println!("  partner backup: {}", pp.display());
        }
        partner = Some((chosen_slot, p_path, p_orig, p_corrupt));
    }

    // 5. Optionally back up the original (target) block.
    if let Some(bp) = &opts.backup {
        fs::write(bp, &orig)?;
        println!("  backup: {}", bp.display());
    }

    // 6. Compute the new P and/or Q depending on --targets.
    let (new_p, new_q): (Vec<u8>, Vec<u8>) = match opts.targets {
        Targets::Data => {
            println!(
                "  targets=data: corrupting data block only; P and Q left consistent with original"
            );
            (p_before.clone(), q_before.clone())
        }
        Targets::BakeP => {
            // P recomputed from the corrupt data block — the array-driver's
            // "I just saw a write and resynced" perspective.  Recovery's
            // P-only path then yields the corrupt bytes back (baked in).
            let p = scrub_rs::array::parity::compute_p_with_override(
                &cfg, devid, &corrupt, array_phys, BLOCK,
            )?;
            println!(
                "  targets=bake-p: P recomputed from corrupt data (P-only recovery will FAIL); Q intact"
            );
            (p, q_before.clone())
        }
        Targets::BakeQ => {
            let q = scrub_rs::array::parity::compute_q_with_override(
                &cfg, devid, &corrupt, array_phys, BLOCK,
            )?;
            println!(
                "  targets=bake-q: Q recomputed from corrupt data (Q-only recovery will FAIL); P intact"
            );
            (p_before.clone(), q)
        }
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
        Targets::TwoDisk => {
            // Both target + partner data disks are corrupt at this offset.
            // **P and Q are left INTACT** — they still reflect the ORIGINAL
            // data on both disks (the array is in sync up to the point of
            // corruption).  This is the scenario the PQ 2-disk solve path
            // is designed for: single-parity P-only recovery fails because
            // the partner's corrupt bytes are XORed in, single-parity
            // Q-only recovery fails for the same reason, but using P and
            // Q simultaneously gives two independent equations in the two
            // unknowns (the original Da, Db) — solvable.  Baking P and Q
            // from the corrupt data would destroy the original-syndrome
            // the solver relies on, so we deliberately do NOT do that.
            let (_p_slot, _pp, _p_orig, _p_corrupt) = partner.as_ref().unwrap();
            println!(
                "  targets=two-disk: target + partner both corrupt; P and Q LEFT INTACT (still reflect original data); single-parity paths FAIL; PQ 2-disk solve is the only path that can recover the target"
            );
            (p_before.clone(), q_before.clone())
        }
    };

    // 7. Write corrupt data + (optionally) recomputed P/Q + (two-disk)
    //    the corrupt partner block.
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

    // 8. Print the scrub-rs command to run next.
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
    // See corrupt_cmd: the filesystem's own devid is always 1 (each data
    // disk is an independent single-device filesystem) and must not be
    // used to look up the array config — derive the real slot from the
    // array-partition device name instead.
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
    // Write original data back, then recompute P and Q from the restored
    // data so the array is fully consistent again.  Also restore the
    // partner block if a partner backup exists (two-disk undo restore).
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
        // Write the partner's original block back.  We need to know which
        // partner slot — re-derive from the partner's first byte (or just
        // write to whichever non-failing degvid had the same array_phys).
        // For simplicity, walk data devs and write to the first whose
        // block at this offset differs from the backup (i.e. was the one
        // we corrupted).  This is best-effort; the test harness is the
        // only caller.
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
