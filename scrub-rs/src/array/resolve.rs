//! Translate a physical on-disk location `(devid, array-partition phys)` to
//! a raw-rdev path + raw physical offset.
//!
//! This is the array-side half of the bridge between the filesystem world
//! (which knows where a bad block lives on which disk) and the array world
//! (which knows how to open the raw rdev that backs that disk and what
//! per-disk `rdevOffset` header to add).  It takes a `(devid, phys)` pair
//! and has no knowledge of btrfs chunks, logical addresses, or ZFS vdevs;
//! any filesystem scrub that can produce a `(devid, phys)` pair can drive
//! recovery through this module.
//!
//! Mirrors the relevant pieces of `recover.py::find_physical_offset` and
//! `handle_failure`'s raw-offset translation.

use std::io;
use std::path::PathBuf;

use crate::array::config::ArrayConfig;

/// A fully-resolved on-disk location: which raw rdev to open, and at what
/// byte offset (already including the per-disk `rdevOffset`).
#[derive(Debug, Clone)]
pub struct ResolvedLocation {
    pub devid: u64,
    /// Raw rdev path (e.g. `/dev/loop2`).
    pub dev_path: PathBuf,
    /// Physical offset on the raw rdev, in bytes (array-space phys + rdevOffset).
    pub raw_phys: u64,
    /// Physical offset in array-partition space (before rdevOffset was added).
    pub array_phys: u64,
}

/// Resolve `(devid, array_phys)` to a concrete raw-rdev location using the
/// array config.
///
/// `devid` must be the actual **NonRAID slot number**, not a raw
/// filesystem-reported devid — every NonRAID data disk hosts its own
/// independent single-device filesystem, so a filesystem's own devid is
/// always `1` regardless of slot (see `array::config` module docs).
/// Callers must resolve the real slot first, e.g. via
/// `ArrayConfig::slot_for_raw_dev` or `config::slot_from_array_partition`.
/// `array_phys` is the byte offset on that disk's array partition.  The
/// returned `raw_phys` adds the per-disk `rdevOffset` so callers can
/// `pread`/`pwrite` the raw rdev directly.
///
/// Returns `Err` if the devid is not a data disk in the array config.
pub fn resolve(config: &ArrayConfig, devid: u64, array_phys: u64) -> io::Result<ResolvedLocation> {
    let dev_path = config.data_dev(devid).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("devid {devid} is not a data disk in the array config"),
        )
    })?;

    let raw_phys = array_phys + config.raw_offset_for(dev_path);

    Ok(ResolvedLocation {
        devid,
        dev_path: dev_path.to_path_buf(),
        raw_phys,
        array_phys,
    })
}
