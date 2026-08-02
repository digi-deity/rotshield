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
use std::path::Path;

use crate::array::config::ArrayConfig;

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
/// Seeking past a block device's end returns `EINVAL` on Linux.  NonRAID
/// arrays with asymmetric data disks hit this when the failing disk is
/// the large one and the offset is past a smaller disk's capacity — the
/// missing region contributes zeros to the parity relationship, so
/// substitute a zero block instead of erroring.
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
    let mut f = match fs::File::open(dev_path) {
        Ok(f) => f,
        Err(e)
            if e.kind() == io::ErrorKind::NotFound
                || e.kind() == io::ErrorKind::PermissionDenied =>
        {
            return Ok(vec![0u8; block_size]);
        }
        Err(e) => return Err(e),
    };
    if let Err(e) = f.seek(SeekFrom::Start(raw_phys)) {
        if e.kind() == io::ErrorKind::InvalidInput {
            return Ok(vec![0u8; block_size]);
        }
        return Err(e);
    }
    let mut buf = vec![0u8; block_size];
    match f.read(&mut buf) {
        Ok(0) => {}
        Ok(_n) => {
            // Short reads already leave the trailing bytes zero-filled in
            // buf, matching the parity relationship's zero-pad convention.
        }
        Err(e)
            if e.kind() == io::ErrorKind::UnexpectedEof
                || e.kind() == io::ErrorKind::InvalidInput =>
        {
            // Past device end: treat as zeros (buf already zero-filled).
        }
        Err(e) => return Err(e),
    }
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
}
