//! Assemble the aligned chunks of one stripe across all array disks.
//!
//! Given an array-space byte offset and a block size, gather the
//! `block_size`-byte chunk sitting at that offset on every data disk (the
//! failing one excluded, since recovery reconstructs it) plus the P and Q
//! parity chunks — all in raw-rdev space (each disk's `rdevOffset` is
//! added internally).  This produces exactly the input the
//! [`recovery`] engine needs, with no filesystem knowledge involved.
//!
//! # Asymmetric arrays
//!
//! NonRAID/Unraid arrays commonly have asymmetric data disks: a smaller
//! data disk has fewer usable bytes than the largest one.  The parity
//! relationship treats the missing region of a smaller disk as zeros
//! (verified experimentally — see `memories/repo/nonraid-asymmetric-parity.md`),
//! so a read that falls past a smaller disk's end is substituted with an
//! all-zero block here, rather than propagated as an error.  This keeps
//! the recovery engine pure — it just sees a `(slot, block)` pair and
//! has no idea whether the bytes came from a real disk or were zero-padded.
//!
//! [`recovery`]: crate::recovery

use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;

use nix::libc;

use crate::array::config::ArrayConfig;

/// `BLKGETSIZE64` ioctl request code — device capacity in bytes.  Expansion
/// of `_IOR(0x12, 114, u64)`: READ direction (0x8000_0000) |
/// (size_of::<u64>() << 16) | (0x12 << 8) | 114.  (Same derivation style as
/// the FIFREEZE/FITHAW constants in `freeze.rs`.)
const BLKGETSIZE64: std::os::raw::c_ulong = 0x8008_1272;

/// Capacity of the open file/device `f` in bytes.
///
/// Regular files: `st_size` via fstat.  **Block devices: `fstat()` reports
/// `st_size` = 0 on Linux** — block-device inodes carry no byte size — so
/// the capacity comes from the `BLKGETSIZE64` ioctl instead.  This
/// distinction is load-bearing: every "past the device end" decision in the
/// array layer (zero-padding of asymmetric disks, straddle reads) is derived
/// from this number, and a zero capacity turns *every* read into an
/// all-zero block — which silently breaks parity reconstruction (the
/// canary/recovery reads return zeros, XOR produces garbage, and the
/// startup canary false-fails on any real-hardware array).
pub fn device_size(f: &fs::File) -> io::Result<u64> {
    let md = f.metadata()?;
    if !md.file_type().is_block_device() {
        return Ok(md.len());
    }
    let mut size: u64 = 0;
    // SAFETY: BLKGETSIZE64 writes exactly one u64 into the provided pointer.
    let rc = unsafe { libc::ioctl(f.as_raw_fd(), BLKGETSIZE64, &mut size as *mut u64) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(size)
}

/// The aligned chunks of one stripe, ready to hand to the recovery engine.
///
/// `other_data` is `(slot, block)` for every *other* data disk (the
/// failing slot excluded), with all-zero substitution for chunks past a
/// smaller disk's end.  `p_block` / `q_block` are the parity chunks at
/// the same offset (or `None` if the array lacks that parity disk).
#[derive(Debug)]
pub struct StripeChunks {
    /// `(slot, block)` for every data disk except the failing one.  All
    /// blocks are exactly `block_size` bytes.
    pub other_data: Vec<(u64, Vec<u8>)>,
    /// Primary parity chunk, or `None` if no P disk.
    pub p_block: Option<Vec<u8>>,
    /// Secondary parity chunk, or `None` if no Q disk.
    pub q_block: Option<Vec<u8>>,
}

/// Gather the stripe at `array_phys` across the array, excluding `failing_slot`.
///
/// `array_phys` is in **array-partition space** (the byte offset on
/// `/dev/nmdNp1`).  Each disk's `rdevOffset` is added internally to
/// reach raw-rdev space.  Reads past a smaller disk's end yield an
/// all-zero block (the array-level parity convention).
pub fn gather_stripe(
    config: &ArrayConfig,
    failing_slot: u64,
    array_phys: u64,
    block_size: usize,
) -> io::Result<StripeChunks> {
    let mut other_data: Vec<(u64, Vec<u8>)> = Vec::new();
    for (slot, path) in &config.data_devs {
        if *slot == failing_slot {
            continue;
        }
        let block = read_block_or_zeros(config, path, array_phys, block_size)?;
        other_data.push((*slot, block));
    }
    let p_block = match config.parity_p.as_ref() {
        None => None,
        Some(p) => Some(read_block_or_zeros(config, p, array_phys, block_size)?),
    };
    let q_block = match config.parity_q.as_ref() {
        None => None,
        Some(q) => Some(read_block_or_zeros(config, q, array_phys, block_size)?),
    };
    Ok(StripeChunks {
        other_data,
        p_block,
        q_block,
    })
}

/// Read one `block_size`-byte block from `dev_path` at `array_phys` on
/// the array partition (the disk's `rdevOffset` is looked up via `config`
/// and added internally), or return a zero block if the read falls past
/// the device end.
///
/// The zero-substitution is ONLY for the asymmetric-array convention:
/// NonRAID arrays with asymmetric data disks treat the missing region of a
/// smaller disk as zeros, so an offset past a smaller disk's *declared
/// size* contributes zeros to the parity relationship.  The device capacity
/// comes from [`device_size`] — fstat's `st_size` for regular files,
/// `BLKGETSIZE64` for block devices (whose `st_size` is 0 on Linux) — so
/// "past the end" is decided by geometry, never by interpreting error
/// codes.
///
/// Everything else is a HARD error, never zeros:
///
/// * open failures (device node vanished, `PermissionDenied`, …),
/// * short reads that are NOT at the device end (a dying disk returning
///   fewer bytes than requested is a hardware-error signal, not padding),
/// * any seek/read I/O error.
///
/// A block that straddles the device end (part of it inside, part past)
/// reads the available bytes and zero-fills the tail — the same convention
/// the parity relationship uses for the missing region.
///
/// The signature deliberately mirrors [`write_block`]: the caller passes
/// the same `(config, dev_path, array_phys)` triple for reads and writes
/// — the array layer owns the `rdevOffset` translation in both directions,
/// so the integration glue never has to know the per-disk header size.
pub fn read_block_or_zeros(
    config: &ArrayConfig,
    dev_path: &Path,
    array_phys: u64,
    block_size: usize,
) -> io::Result<Vec<u8>> {
    let raw_phys = array_phys + config.raw_offset_for(dev_path);
    // Any open failure is a hard error (missing disk, permissions, node
    // vanished mid-run) — never silently substitute zeros.
    let mut f = fs::File::open(dev_path)?;
    // The device/file capacity decides the zero-pad boundary.  NOTE: this
    // must be [`device_size`], NOT `f.metadata().len()` — on Linux,
    // `fstat()` reports `st_size` = 0 for block devices, which would make
    // every offset "past the end" and turn every read into zeros (that
    // silently breaks parity reconstruction on real arrays).
    let size = device_size(&f)?;
    if raw_phys >= size {
        // Past the device end: the asymmetric-array zero-pad convention.
        return Ok(vec![0u8; block_size]);
    }
    // Block straddling the device end: read the available bytes, zero-fill
    // the remainder (same convention as the pad case).
    let available = (size - raw_phys).min(block_size as u64) as usize;
    let mut buf = vec![0u8; block_size];
    f.seek(SeekFrom::Start(raw_phys))?;
    f.read_exact(&mut buf[..available])?;
    if available < block_size {
        // Straddle: `buf[available..]` stays zero-filled.
        return Ok(buf);
    }
    // A full block inside the device: a short read here is a hardware
    // error signal (dying disk), NOT padding — `read_exact` surfaces it as
    // `UnexpectedEof` and the caller counts the candidate `failed`.
    Ok(buf)
}

/// Write one `data` block to the raw rdev `dev_path` at `array_phys` (the
/// disk's `rdevOffset` is looked up via `config` and added internally,
/// mirroring [`read_block_or_zeros`]), fsyncing before returning.  Used by
/// the integration glue to write back a recovered block to the failing
/// disk **in raw-rdev space** so the array driver is bypassed and parity
/// is left holding the original relationship — see the "Why recovery
/// writes to raw-rdev space" doc in [`crate::array`] for the rationale.
///
/// The signature deliberately mirrors [`read_block_or_zeros`]: the caller
/// passes the same `(dev_path, array_phys)` pair for reads and writes —
/// the array layer owns the `rdevOffset` translation in both directions,
/// so the integration glue never has to memoise per-disk offsets.
pub fn write_block(
    config: &ArrayConfig,
    dev_path: &Path,
    array_phys: u64,
    data: &[u8],
) -> io::Result<()> {
    let raw_phys = array_phys + config.raw_offset_for(dev_path);
    let mut f = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(dev_path)?;
    f.seek(SeekFrom::Start(raw_phys))?;
    f.write_all(data)?;
    f.flush()?;
    f.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::array::config::ArrayConfig;
    use std::collections::BTreeMap;
    use std::io::Write;
    use std::path::PathBuf;

    /// Build a tiny temp file of exactly `size` bytes filled with `fill`.
    fn make_image(dir: &tempfile::TempDir, name: &str, size: u64, fill: u8) -> PathBuf {
        let path = dir.path().join(name);
        let mut f = fs::File::create(&path).unwrap();
        let chunk = [fill; 4096];
        let mut remaining = size;
        while remaining > 0 {
            let n = chunk.len().min(remaining as usize) as u64;
            f.write_all(&chunk[..n as usize]).unwrap();
            remaining -= n;
        }
        f.sync_all().unwrap();
        path
    }

    /// A minimal `ArrayConfig` pointing at three regular files instead of
    /// block devices.  We bypass `is_block_device` checks by inserting
    /// paths directly into the maps.
    fn config_from_files(
        data: Vec<(u64, PathBuf)>,
        p: Option<PathBuf>,
        q: Option<PathBuf>,
        rdev_offsets: Vec<(PathBuf, u64)>,
    ) -> ArrayConfig {
        let mut data_devs = BTreeMap::new();
        for (s, path) in data {
            data_devs.insert(s, path);
        }
        let mut offsets = BTreeMap::new();
        for (path, off) in rdev_offsets {
            offsets.insert(path, off);
        }
        ArrayConfig {
            data_devs,
            parity_p: p,
            parity_q: q,
            rdev_offsets: offsets,
        }
    }

    #[test]
    fn gather_reads_aligned_chunks_across_disks() {
        // 3 data disks of equal size, distinct fill bytes; rdevOffset 0.
        // gather at offset 0 → first 4 bytes of each disk.
        let dir = tempfile::tempdir().unwrap();
        let d1 = make_image(&dir, "d1", 4096, 0x11);
        let d2 = make_image(&dir, "d2", 4096, 0x22);
        let d3 = make_image(&dir, "d3", 4096, 0x33);
        let p = make_image(&dir, "p", 4096, 0x44);
        let cfg = config_from_files(
            vec![(1, d1.clone()), (2, d2.clone()), (3, d3.clone())],
            Some(p.clone()),
            None,
            vec![],
        );
        let chunks = gather_stripe(&cfg, 1, 0, 4).unwrap();
        // other_data has slots 2 and 3 (failing=1 excluded).
        assert_eq!(chunks.other_data.len(), 2);
        let (s2, b2) = &chunks.other_data[0];
        let (s3, b3) = &chunks.other_data[1];
        assert_eq!(*s2, 2);
        assert_eq!(b2, &[0x22; 4]);
        assert_eq!(*s3, 3);
        assert_eq!(b3, &[0x33; 4]);
        assert_eq!(chunks.p_block.as_deref(), Some(&[0x44; 4][..]));
        assert!(chunks.q_block.is_none());
    }

    #[test]
    fn gather_zero_pads_past_smaller_disk_end() {
        // Asymmetric: d1 big (8192), d2 small (4096).  Reading a block at
        // offset 8192 (past both ends) yields zeros for everyone.  More
        // importantly, reading a block at offset 4096..8192 on d2 must
        // zero-pad (the read seeks past d2's end).
        let dir = tempfile::tempdir().unwrap();
        let d1 = make_image(&dir, "d1", 8192, 0x11);
        let d2 = make_image(&dir, "d2", 4096, 0x22);
        let cfg = config_from_files(vec![(1, d1.clone()), (2, d2.clone())], None, None, vec![]);
        // Offset 4096 (block index 1): d1 has real data; d2's file is only
        // 4096 bytes long so seeking to 4096 reads EOF → zero block.
        let chunks = gather_stripe(&cfg, 1, 4096, 4096).unwrap();
        let other: std::collections::BTreeMap<u64, Vec<u8>> =
            chunks.other_data.iter().cloned().collect();
        // d1 (slot 1) is failing → excluded; only d2 remains in other_data.
        assert_eq!(other.len(), 1);
        assert!(other.contains_key(&2));
        assert_eq!(other.get(&2).unwrap(), &vec![0u8; 4096]);

        // Offset 0 (block index 0): d2 has real data.
        let chunks0 = gather_stripe(&cfg, 1, 0, 4096).unwrap();
        let other0: std::collections::BTreeMap<u64, Vec<u8>> =
            chunks0.other_data.iter().cloned().collect();
        assert_eq!(other0.get(&2).unwrap(), &vec![0x22; 4096]);
    }

    #[test]
    fn gather_missing_disk_is_a_hard_error() {
        // C6: a disk whose node vanished (or is unreadable) must be a hard
        // error, never silently zeroed — zero substitution is ONLY for
        // offsets past a smaller disk's declared size.
        let dir = tempfile::tempdir().unwrap();
        let d1 = make_image(&dir, "d1", 8192, 0x11);
        let missing = dir.path().join("does-not-exist");
        let cfg = config_from_files(
            vec![(1, d1.clone()), (2, missing.clone())],
            None,
            None,
            vec![],
        );
        let err = gather_stripe(&cfg, 1, 0, 4096).unwrap_err();
        assert_eq!(
            err.kind(),
            io::ErrorKind::NotFound,
            "missing disk must surface as an open error, not zeros"
        );
    }

    #[test]
    fn gather_zero_fills_only_the_straddle_tail() {
        // C6: a block that straddles the device end (part inside, part
        // past) reads the available bytes and zero-fills only the tail —
        // the asymmetric-array convention for the missing region.  d2 is
        // 4096+2048 = 6144 bytes; reading a 4096-byte block at raw_phys
        // 4096 yields 2048 real bytes + 2048 zeros.
        let dir = tempfile::tempdir().unwrap();
        let d1 = make_image(&dir, "d1", 8192, 0x11);
        let d2 = {
            let path = dir.path().join("d2");
            let mut f = fs::File::create(&path).unwrap();
            f.write_all(&vec![0x22u8; 4096]).unwrap();
            f.write_all(&vec![0x33u8; 2048]).unwrap();
            f.sync_all().unwrap();
            path
        };
        let cfg = config_from_files(vec![(1, d1.clone()), (2, d2.clone())], None, None, vec![]);
        let chunks = gather_stripe(&cfg, 1, 4096, 4096).unwrap();
        let other: std::collections::BTreeMap<u64, Vec<u8>> =
            chunks.other_data.iter().cloned().collect();
        let b = other.get(&2).unwrap();
        assert_eq!(
            &b[..2048],
            &vec![0x33u8; 2048],
            "real bytes inside the device"
        );
        assert_eq!(
            &b[2048..],
            &vec![0u8; 2048],
            "tail past the end zero-filled"
        );
    }

    #[test]
    fn gather_adds_per_disk_rdev_offset() {
        // Two disks with different rdevOffsets (32K and 32M, mirroring CI
        // disk2).  Reading at array_phys=0 must read each raw file at its
        // own rdevOffset.  We pattern the bytes so we can identify which
        // offset produced each block.
        let dir = tempfile::tempdir().unwrap();
        // d1: 64K total, byte at offset 32K = 0xAA, elsewhere 0x00.
        let d1 = {
            let path = dir.path().join("d1");
            let mut f = fs::File::create(&path).unwrap();
            f.write_all(&vec![0u8; 32 * 1024]).unwrap();
            f.write_all(&[0xAA]).unwrap();
            f.write_all(&vec![0u8; 32 * 1024 - 1]).unwrap();
            f.sync_all().unwrap();
            path
        };
        // d2: 64M+4K total, byte at offset 32M = 0xBB, elsewhere 0x00.
        let d2 = {
            let path = dir.path().join("d2");
            let mut f = fs::File::create(&path).unwrap();
            // 32M zero, one BB, then enough trailing zero to reach 32M+4K.
            f.write_all(&vec![0u8; 32 * 1024 * 1024]).unwrap();
            f.write_all(&[0xBB]).unwrap();
            f.write_all(&vec![0u8; 4096 - 1]).unwrap();
            f.sync_all().unwrap();
            path
        };
        let cfg = config_from_files(
            vec![(1, d1.clone()), (2, d2.clone())],
            None,
            None,
            vec![(d1.clone(), 32 * 1024), (d2.clone(), 32 * 1024 * 1024)],
        );
        // array_phys=0 → d1 raw_phys=32K (block at 0xAA), d2 raw_phys=32M
        // (block at 0xBB).  Read 4-byte blocks; only the first byte is
        // our marker, the rest are 0.
        let chunks = gather_stripe(&cfg, 1, 0, 4).unwrap();
        let other: std::collections::BTreeMap<u64, Vec<u8>> =
            chunks.other_data.iter().cloned().collect();
        let b2 = other.get(&2).unwrap();
        assert_eq!(b2[0], 0xBB, "d2 chunk must be read at its 32M rdevOffset");
        assert_eq!(&b2[1..], &[0u8; 3]);

        // And confirm d1 (failing=1) is excluded.
        assert!(!other.contains_key(&1));
    }

    /// Detach a loop device when the guard drops (test cleanup).
    struct LoopGuard(String);
    impl Drop for LoopGuard {
        fn drop(&mut self) {
            let _ = std::process::Command::new("losetup")
                .args(["-d"])
                .arg(&self.0)
                .status();
        }
    }

    #[test]
    fn read_block_on_a_real_block_device_is_not_zeroed() {
        // REGRESSION: `fstat()` reports `st_size` = 0 for block devices on
        // Linux, so `f.metadata().len()` made `read_block_or_zeros` treat
        // EVERY offset as past-the-end and return zeros — silently breaking
        // parity reconstruction (canary false-failures) on every real array.
        // The capacity must come from BLKGETSIZE64.  This test needs root +
        // losetup (both present on CI runners); it skips gracefully
        // otherwise so unprivileged `cargo test` stays green.
        if unsafe { nix::libc::geteuid() } != 0 {
            eprintln!("skipping: block-device test requires root");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("img");
        {
            let mut f = fs::File::create(&img).unwrap();
            f.write_all(&vec![0x11u8; 4096]).unwrap();
            f.write_all(&vec![0x22u8; 4096]).unwrap();
            f.write_all(&vec![0x33u8; 4096]).unwrap();
            f.sync_all().unwrap();
        }
        let out = match std::process::Command::new("losetup")
            .args(["-f", "--show"])
            .arg(&img)
            .output()
        {
            Ok(o) if o.status.success() => o,
            other => {
                eprintln!("skipping: losetup unavailable ({other:?})");
                return;
            }
        };
        let dev = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let _guard = LoopGuard(dev.clone());

        // The bug premise, asserted so a kernel that ever changes this
        // behaviour fails loudly instead of silently masking a regression:
        // st_size is 0 for block devices, fstat cannot be the size source.
        assert_eq!(
            fs::metadata(&dev).unwrap().len(),
            0,
            "premise: fstat st_size is 0 for block devices on Linux"
        );
        let dev_path = PathBuf::from(&dev);
        let cfg = config_from_files(vec![(1, dev_path.clone())], None, None, vec![]);

        // Full blocks inside the device: REAL bytes, never zeros.
        let b0 = read_block_or_zeros(&cfg, &dev_path, 0, 4096).unwrap();
        assert_eq!(b0, vec![0x11u8; 4096], "block 0 must be real data");
        let b1 = read_block_or_zeros(&cfg, &dev_path, 4096, 4096).unwrap();
        assert_eq!(b1, vec![0x22u8; 4096], "block 1 must be real data");
        let b2 = read_block_or_zeros(&cfg, &dev_path, 8192, 4096).unwrap();
        assert_eq!(b2, vec![0x33u8; 4096], "block 2 must be real data");

        // Straddle the device end: 2048 real bytes + zero-filled tail.
        let s = read_block_or_zeros(&cfg, &dev_path, 8192 + 2048, 4096).unwrap();
        assert_eq!(&s[..2048], &vec![0x33u8; 2048], "real bytes inside");
        assert_eq!(&s[2048..], &vec![0u8; 2048], "tail zero-filled");

        // Fully past the end: the asymmetric-array zero-pad convention.
        let z = read_block_or_zeros(&cfg, &dev_path, 12288, 4096).unwrap();
        assert_eq!(z, vec![0u8; 4096], "past the end pads zeros");
    }
}
