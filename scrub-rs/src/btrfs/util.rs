//! Small little-endian byte-reading helpers used across the crate.

use std::io::{self, Read, Seek, SeekFrom};

/// Read `n` bytes at `offset` from `fp`.
pub fn read_at<R: Read + Seek>(fp: &mut R, offset: u64, n: usize) -> io::Result<Vec<u8>> {
    fp.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; n];
    fp.read_exact(&mut buf)?;
    Ok(buf)
}

pub fn le_u16(buf: &[u8], pos: usize) -> u16 {
    u16::from_le_bytes([buf[pos], buf[pos + 1]])
}

pub fn le_u32(buf: &[u8], pos: usize) -> u32 {
    u32::from_le_bytes([
        buf[pos],
        buf[pos + 1],
        buf[pos + 2],
        buf[pos + 3],
    ])
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