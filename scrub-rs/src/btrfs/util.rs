use std::io::{self, Read, Seek, SeekFrom};

pub fn read_at<R: Read + Seek>(fp: &mut R, offset: u64, n: usize) -> io::Result<Vec<u8>> {
    fp.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; n];
    fp.read_exact(&mut buf)?;
    Ok(buf)
}

#[cfg(unix)]
pub fn pread_at(fp: &std::fs::File, offset: u64, n: usize) -> io::Result<Vec<u8>> {
    use std::os::unix::fs::FileExt;
    // Positional read: the fd cursor is left untouched, so reads on cloned
    // handles that share one open file description cannot interfere.
    // Honors the SCRUB_FAULT env hook (see fault_range) for fault-injection.
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

// SCRUB_FAULT=<start>,<len> (len defaults to 4096) makes pread_at fail with
// EIO over that byte range; test-only fault injection.
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

// btrfs integers are little-endian; these read LE values at pos and panic if
// the read runs past the end of buf.
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

pub fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    use std::fmt::Write as _;
    for b in bytes {
        let _ = write!(&mut s, "{b:02x}");
    }
    s
}
