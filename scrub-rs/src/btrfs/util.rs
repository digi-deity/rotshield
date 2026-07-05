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