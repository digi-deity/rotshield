//! The scrub loop — for every REGULAR data extent, read each 4096-byte
//! sector from disk, compute its CRC32C, and compare against the stored
//! checksum from the CSUM tree.

use crate::btrfs::chunk::ChunkMap;
use crate::btrfs::extent::FileExtent;
use crate::btrfs::reader::FsReader;
use crate::btrfs::superblock::BTRFS_SECTOR_SIZE;
use crate::btrfs::csum::CsumMap;

/// Result of scrubbing a single sector.
///
/// `devid` and `array_phys` give the on-disk physical location in
/// **array-partition space**: which disk and at what byte offset on that
/// disk's array partition (`/dev/nmd1p1`).  These are filesystem-agnostic
/// — any array recovery layer only needs "which disk, which byte" to do
/// XOR parity reconstruction, no knowledge of btrfs chunks or logical
/// addresses.  See the "Address spaces and I/O paths" doc in
/// `array::mod` for what each space means and how the I/O paths differ.
///
/// `logical`, `inode`, and `file_offset` are kept for logging and for
/// filesystem-specific callers but are not needed by recovery.
///
/// Checksums are carried as raw little-endian `u32` bytes here (btrfs's
/// on-disk layout) so the [`crate::btrfs::BtrfsScrub`] adapter can pack
/// them into `Box<dyn Fn(&[u8]) -> bool>` closures without re-deriving
/// the algorithm at the boundary.  `stored_csum` is `None` for sectors
/// with no CSUM-tree entry; `actual_csum` is always populated (it's just
/// `crc32c(data)`).
#[derive(Debug)]
pub struct SectorResult {
    pub logical: u64,
    /// btrfs device ID (== NonRAID slot number for our arrays).
    pub devid: u64,
    /// Physical offset in **array-partition space** (on the array
    /// partition device, before `rdevOffset` is added).  Recovery adds
    /// `rdevOffset` to get raw-rdev space.
    pub array_phys: u64,
    pub inode: u64,
    pub file_offset: u64,
    /// Stored checksum from the CSUM tree, as raw little-endian bytes.
    /// `None` if no CSUM entry covers this sector.
    pub stored_csum: Option<[u8; 4]>,
    /// `crc32c(data)` for the on-disk data, as raw little-endian bytes.
    pub actual_csum: [u8; 4],
    pub ok: bool,
}

/// Scrub statistics.
#[derive(Debug, Default)]
pub struct ScrubStats {
    pub sectors_checked: u64,
    pub sectors_ok: u64,
    pub sectors_mismatch: u64,
    pub sectors_no_csum: u64,
    pub sectors_read_error: u64,
    pub bytes_checked: u64,
}

/// Scrub all sectors of all REGULAR extents.
///
/// Calls `on_sector` for each sector that mismatches or has no checksum,
/// so the caller can print/report them.  The callback receives a fully
/// populated `SectorResult` including the on-disk physical location
/// `(devid, phys)` — computed via `chunk_map.lookup` inside the scrub —
/// so callers that want to act on a mismatch (e.g. parity recovery) get a
/// filesystem-agnostic physical address without needing to borrow the
/// chunk map themselves.
pub fn scrub_extents<F>(
    reader: &mut FsReader,
    chunk_map: &ChunkMap,
    csum_map: &CsumMap,
    extents: &[FileExtent],
    mut on_sector: F,
) -> ScrubStats
where
    F: FnMut(&SectorResult),
{
    let mut stats = ScrubStats::default();
    let sector_size = BTRFS_SECTOR_SIZE as u64;

    for ext in extents {
        // Sparse extents (disk_bytenr == 0) have no on-disk data to verify.
        if ext.disk_bytenr == 0 {
            continue;
        }

        let disk_start = ext.disk_start();
        let mut remaining = ext.num_bytes;
        let mut logical = disk_start;
        let mut file_off = ext.file_offset;

        while remaining > 0 {
            let len = std::cmp::min(remaining, sector_size);

            // Only verify full sectors — partial trailing sectors don't have
            // their own checksum in the csum tree.
            if len == sector_size {
                let sector_logical = (logical / sector_size) * sector_size;
                stats.sectors_checked += 1;
                stats.bytes_checked += sector_size;

                match reader.read_logical(chunk_map, sector_logical, sector_size as usize) {
                    Ok(data) => {
                        let actual = crc32c::crc32c(&data);
                        let actual_bytes = actual.to_le_bytes();
                        // Resolve to array-partition space once, here, so
                        // the callback gets a filesystem-agnostic
                        // (devid, array_phys) without needing the chunk map.
                        // Recovery adds rdevOffset to reach raw-rdev space.
                        let (devid, array_phys) = chunk_map
                            .lookup(sector_logical)
                            .unwrap_or((0, 0));
                        match csum_map.get(&sector_logical) {
                            Some(&stored) => {
                                let stored_bytes = stored.to_le_bytes();
                                if actual_bytes == stored_bytes {
                                    stats.sectors_ok += 1;
                                } else {
                                    stats.sectors_mismatch += 1;
                                    on_sector(&SectorResult {
                                        logical: sector_logical,
                                        devid,
                                        array_phys,
                                        inode: ext.inode,
                                        file_offset: file_off,
                                        stored_csum: Some(stored_bytes),
                                        actual_csum: actual_bytes,
                                        ok: false,
                                    });
                                }
                            }
                            None => {
                                stats.sectors_no_csum += 1;
                                on_sector(&SectorResult {
                                    logical: sector_logical,
                                    devid,
                                    array_phys,
                                    inode: ext.inode,
                                    file_offset: file_off,
                                    stored_csum: None,
                                    actual_csum: actual_bytes,
                                    ok: false,
                                });
                            }
                        }
                    }
                    Err(e) => {
                        stats.sectors_read_error += 1;
                        eprintln!(
                            "read error at logical 0x{:x} (ino {} off 0x{:x}): {}",
                            sector_logical, ext.inode, file_off, e
                        );
                    }
                }
            }

            remaining -= len;
            logical += len;
            file_off += len;
        }
    }

    stats
}