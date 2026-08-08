//! Gather and write aligned stripe chunks across the array's disks, in
//! raw-rdev space.

use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;

use nix::libc;

use crate::array::config::ArrayConfig;

// ioctl returning a block device's capacity in bytes.
const BLKGETSIZE64: std::os::raw::c_ulong = 0x8008_1272;

/// Capacity of the open file/device: fstat size for regular files,
/// BLKGETSIZE64 for block devices (whose fstat st_size is 0 on Linux).
pub fn device_size(f: &fs::File) -> io::Result<u64> {
    let md = f.metadata()?;
    if !md.file_type().is_block_device() {
        return Ok(md.len());
    }
    let mut size: u64 = 0;

    let rc = unsafe { libc::ioctl(f.as_raw_fd(), BLKGETSIZE64, &mut size as *mut u64) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(size)
}

/// The aligned chunks of one stripe, ready for the recovery engine.
#[derive(Debug)]
pub struct StripeChunks {
    /// (slot, block) for every data disk except the failing one.
    pub other_data: Vec<(u64, Vec<u8>)>,

    /// Primary parity chunk at this offset, if the array has a P disk.
    pub p_block: Option<Vec<u8>>,

    /// Secondary parity chunk at this offset, if the array has a Q disk.
    pub q_block: Option<Vec<u8>>,
}

/// Read the block_size-byte chunk at `array_phys` (array-partition space)
/// on every data disk except `failing_slot`, plus the P and Q chunks.
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

/// Read one block from `dev_path`, zero-filling when the offset lies past
/// the device end (the asymmetric-array convention). Any other failure —
/// open, seek, or a short read inside the device — is a hard error.
pub fn read_block_or_zeros(
    config: &ArrayConfig,
    dev_path: &Path,
    array_phys: u64,
    block_size: usize,
) -> io::Result<Vec<u8>> {
    let raw_phys = array_phys + config.raw_offset_for(dev_path);

    let mut f = fs::File::open(dev_path)?;

    // The device/file capacity decides the zero-pad boundary.
    let size = device_size(&f)?;
    // Past the device end: the asymmetric-array zero-pad convention.
    if raw_phys >= size {
        return Ok(vec![0u8; block_size]);
    }

    // Block straddling the device end: read the available bytes and
    // zero-fill the tail (same convention as the pad case).
    let available = (size - raw_phys).min(block_size as u64) as usize;
    let mut buf = vec![0u8; block_size];
    f.seek(SeekFrom::Start(raw_phys))?;
    f.read_exact(&mut buf[..available])?;
    // `buf[available..]` stays zero-filled.
    if available < block_size {
        return Ok(buf);
    }

    Ok(buf)
}

/// Write `data` to `dev_path` at `array_phys` (array-partition space; the
/// disk's rdevOffset is added internally), fsyncing before returning.
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
        let dir = tempfile::tempdir().unwrap();
        let d1 = make_image(&dir, "d1", 8192, 0x11);
        let d2 = make_image(&dir, "d2", 4096, 0x22);
        let cfg = config_from_files(vec![(1, d1.clone()), (2, d2.clone())], None, None, vec![]);

        let chunks = gather_stripe(&cfg, 1, 4096, 4096).unwrap();
        let other: std::collections::BTreeMap<u64, Vec<u8>> =
            chunks.other_data.iter().cloned().collect();

        assert_eq!(other.len(), 1);
        assert!(other.contains_key(&2));
        assert_eq!(other.get(&2).unwrap(), &vec![0u8; 4096]);

        let chunks0 = gather_stripe(&cfg, 1, 0, 4096).unwrap();
        let other0: std::collections::BTreeMap<u64, Vec<u8>> =
            chunks0.other_data.iter().cloned().collect();
        assert_eq!(other0.get(&2).unwrap(), &vec![0x22; 4096]);
    }

    #[test]
    fn gather_missing_disk_is_a_hard_error() {
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
        let dir = tempfile::tempdir().unwrap();

        let d1 = {
            let path = dir.path().join("d1");
            let mut f = fs::File::create(&path).unwrap();
            f.write_all(&vec![0u8; 32 * 1024]).unwrap();
            f.write_all(&[0xAA]).unwrap();
            f.write_all(&vec![0u8; 32 * 1024 - 1]).unwrap();
            f.sync_all().unwrap();
            path
        };

        let d2 = {
            let path = dir.path().join("d2");
            let mut f = fs::File::create(&path).unwrap();

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

        let chunks = gather_stripe(&cfg, 1, 0, 4).unwrap();
        let other: std::collections::BTreeMap<u64, Vec<u8>> =
            chunks.other_data.iter().cloned().collect();
        let b2 = other.get(&2).unwrap();
        assert_eq!(b2[0], 0xBB, "d2 chunk must be read at its 32M rdevOffset");
        assert_eq!(&b2[1..], &[0u8; 3]);

        assert!(!other.contains_key(&1));
    }

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

        assert_eq!(
            fs::metadata(&dev).unwrap().len(),
            0,
            "premise: fstat st_size is 0 for block devices on Linux"
        );
        let dev_path = PathBuf::from(&dev);
        let cfg = config_from_files(vec![(1, dev_path.clone())], None, None, vec![]);

        let b0 = read_block_or_zeros(&cfg, &dev_path, 0, 4096).unwrap();
        assert_eq!(b0, vec![0x11u8; 4096], "block 0 must be real data");
        let b1 = read_block_or_zeros(&cfg, &dev_path, 4096, 4096).unwrap();
        assert_eq!(b1, vec![0x22u8; 4096], "block 1 must be real data");
        let b2 = read_block_or_zeros(&cfg, &dev_path, 8192, 4096).unwrap();
        assert_eq!(b2, vec![0x33u8; 4096], "block 2 must be real data");

        let s = read_block_or_zeros(&cfg, &dev_path, 8192 + 2048, 4096).unwrap();
        assert_eq!(&s[..2048], &vec![0x33u8; 2048], "real bytes inside");
        assert_eq!(&s[2048..], &vec![0u8; 2048], "tail zero-filled");

        let z = read_block_or_zeros(&cfg, &dev_path, 12288, 4096).unwrap();
        assert_eq!(z, vec![0u8; 4096], "past the end pads zeros");
    }
}
