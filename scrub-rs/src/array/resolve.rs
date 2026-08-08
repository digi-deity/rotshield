//! Resolve a (devid, array offset) pair into the raw-rdev path and byte
//! offset.

use std::io;
use std::path::PathBuf;

use crate::array::config::ArrayConfig;

/// Where a position in array space actually lives on a raw device.
#[derive(Debug, Clone)]
pub struct ResolvedLocation {
    pub devid: u64,

    /// The raw rdev path.
    pub dev_path: PathBuf,

    /// `array_phys` plus the disk's rdev offset.
    pub raw_phys: u64,

    pub array_phys: u64,
}

/// Look up the raw-rdev path for `devid` and translate `array_phys` into
/// raw-rdev space.
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
