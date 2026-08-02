//! Small little-endian byte-reading helpers used across the crate.

use std::io::{self, Read, Seek, SeekFrom};

/// Read `n` bytes at `offset` from `fp`.
pub fn read_at<R: Read + Seek>(fp: &mut R, offset: u64, n: usize) -> io::Result<Vec<u8>> {
    fp.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; n];
    fp.read_exact(&mut buf)?;
    Ok(buf)
}

/// Read `n` bytes at `offset` from `fp` using a **positional** read
/// (`pread`), which does not consult or mutate the file's cursor.
///
/// This matters because `File::try_clone` (used to hand the scrub reader
/// thread its own file handle — see `scrub::scrub_dev_tree`) dupes the fd
/// but — per `std::fs::File::try_clone`'s documented behaviour — the
/// clone **shares the same underlying open-file-description, including the
/// seek position**, with the original.  [`read_at`] above is seek-based
/// (`seek` + `read_exact`), so if the main thread and the reader thread
/// both call it concurrently on their respective handles to the *same*
/// open file description, one thread's `seek` can land between the other
/// thread's `seek` and `read_exact`, silently reading the wrong bytes —
/// producing spurious checksum mismatches that go away at low pipeline
/// depth (where the two threads happen not to overlap) and reappear as
/// pipeline depth increases. `pread` reads at an explicit offset without
/// touching the shared cursor, so concurrent callers never race each
/// other. Every read that can run concurrently with the scrub reader
/// thread (i.e. anything through [`crate::btrfs::reader::FsReader`] and
/// the reader-thread's own reads in `scrub::scrub_dev_tree`) must use
/// this instead of [`read_at`]. Unix-only (`pread` has no portable
/// cross-platform equivalent in `std`); this crate targets Linux.
#[cfg(unix)]
pub fn pread_at(fp: &std::fs::File, offset: u64, n: usize) -> io::Result<Vec<u8>> {
    use std::os::unix::fs::FileExt;
    // Fault-injection hook: when `SCRUB_FAULT=start[,len]` is set, any read
    // whose byte range overlaps `[start, start+len)` fails with EIO.  Lets
    // the integration harness exercise the EIO paths (divide-and-conquer
    // isolation, unreadable recovery candidates, metadata read errors)
    // without a real failing device or `dmsetup`.  No-op when unset, so
    // healthy-disk throughput is unchanged.  See
    // `docs/EIO-robustness-design.md` §7.
    if let Some((fstart, flen)) = fault_range() {
        let fend = fstart.saturating_add(flen);
        let req_end = offset.saturating_add(n as u64);
        if offset < fend && req_end > fstart {
            return Err(io::Error::from_raw_os_error(5)); // EIO
        }
    }
    let mut buf = vec![0u8; n];
    fp.read_exact_at(&mut buf, offset)?;
    Ok(buf)
}

/// Parse the `SCRUB_FAULT` env var (once per process): `start[,len]` in
/// bytes.  `len` defaults to 4096 (one sector).  `None` when unset or
/// malformed (unset = healthy-disk behaviour).
#[cfg(unix)]
fn fault_range() -> Option<(u64, u64)> {
    use std::sync::OnceLock;
    static FAULT: OnceLock<Option<(u64, u64)>> = OnceLock::new();
    *FAULT.get_or_init(|| {
        let raw = std::env::var("SCRUB_FAULT").ok()?;
        let (start, len) = match raw.split_once(',') {
            Some((s, l)) => (s.trim(), l.trim()),
            None => (raw.trim(), "4096"),
        };
        let start: u64 = start.parse().ok()?;
        let len: u64 = len.parse().ok()?;
        Some((start, len.max(1)))
    })
}

pub fn le_u16(buf: &[u8], pos: usize) -> u16 {
    u16::from_le_bytes([buf[pos], buf[pos + 1]])
}

pub fn le_u32(buf: &[u8], pos: usize) -> u32 {
    u32::from_le_bytes([buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]])
}

pub fn le_u64(buf: &[u8], pos: usize) -> u64 {
    u64::from_le_bytes([
        buf[pos],
        buf[pos + 1],
        buf[pos + 2],
        buf[pos + 3],
        buf[pos + 4],
        buf[pos + 5],
        buf[pos + 6],
        buf[pos + 7],
    ])
}

/// Hex-encode `bytes` lowercase (4-byte btrfs crc32c becomes 8 hex chars;
/// a 16-byte fsid becomes 32; a future ZFS 32-byte sha256 becomes 64).
///
/// Single home for the hex-format helper.  Previously both `main.rs` and
/// `btrfs/scrub_driver.rs` carried their own copies.  Lives here (and not
/// in a future `serde/format.rs`) because the only callers in this crate
/// are formatting btrfs-owned fields (fsids, csum bytes) — the helper is
/// adjacent to the btrfs byte-reading primitives it already pairs with.
pub fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    use std::fmt::Write as _;
    for b in bytes {
        let _ = write!(&mut s, "{b:02x}");
    }
    s
}
