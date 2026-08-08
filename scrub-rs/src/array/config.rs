//! Parse the array layout from /proc/nmdstat (or /proc/mdstat): slots,
//! parity disks, and per-disk rdev offsets.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

// Slot the NonRAID module assigns to the secondary (Q) parity disk.
const NMDSTAT_Q_SLOT: u64 = 29;

// Candidate stat files, tried in order.
const STAT_PATHS: &[&str] = &["/proc/nmdstat", "/proc/mdstat"];

/// Parsed array layout.
#[derive(Debug, Default, Clone)]
pub struct ArrayConfig {
    /// NonRAID slot → raw rdev path of that data disk.
    pub data_devs: BTreeMap<u64, PathBuf>,

    /// Primary parity (P) disk — slot 0.
    pub parity_p: Option<PathBuf>,

    /// Secondary parity (Q) disk — slot 29.
    pub parity_q: Option<PathBuf>,

    /// Raw rdev path → byte offset to add to array-space addresses.
    pub rdev_offsets: BTreeMap<PathBuf, u64>,
}

impl ArrayConfig {
    /// Offset to add when accessing `dev_path` directly (0 for array partitions,
    /// which already have the per-disk header stripped).
    pub fn raw_offset_for(&self, dev_path: &Path) -> u64 {
        *self.rdev_offsets.get(dev_path).unwrap_or(&0)
    }

    /// Raw-rdev path for a data slot.
    pub fn data_dev(&self, devid: u64) -> Option<&Path> {
        self.data_devs.get(&devid).map(|p| p.as_path())
    }

    /// Reverse lookup: which slot owns this raw-rdev path? Symlinks are
    /// canonicalized on both sides.
    pub fn slot_for_raw_dev(&self, path: &Path) -> Option<u64> {
        let canon = fs::canonicalize(path).ok();
        let target = canon.as_deref().unwrap_or(path);
        self.data_devs.iter().find_map(|(slot, p)| {
            let p_canon = fs::canonicalize(p).ok();
            let p_target = p_canon.as_deref().unwrap_or(p.as_path());
            (p_target == target).then_some(*slot)
        })
    }

    /// `array_phys + rdevOffset` for a slot — for logging/display; the I/O
    /// functions resolve offsets internally.
    pub fn raw_phys(&self, slot: u64, array_phys: u64) -> Option<u64> {
        let dev = self.data_dev(slot)?;
        Some(array_phys + self.raw_offset_for(dev))
    }
}

/// Parse the slot out of an array-partition name like /dev/nmd2p1.
pub fn slot_from_array_partition(dev: &str) -> Option<u64> {
    let name = Path::new(dev).file_name()?.to_str()?;
    let rest = name.strip_prefix("nmd")?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }

    // Require a p<partition> suffix so bare "nmd123" names are not
    // mistaken for slots.
    let after_digits = &rest[digits.len()..];
    if !after_digits.starts_with('p') || after_digits.len() < 2 {
        return None;
    }
    digits.parse().ok()
}

/// First existing stat file, or None.
fn stat_path() -> Option<&'static str> {
    STAT_PATHS.iter().copied().find(|p| Path::new(p).exists())
}

/// Parse one stat file (flat key=value) into an ArrayConfig.
fn parse_nmdstat(path: &str) -> std::io::Result<ArrayConfig> {
    let text = fs::read_to_string(path)?;
    let values: BTreeMap<String, String> = text
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let (k, v) = line.split_once('=')?;
            Some((k.to_string(), v.trim().to_string()))
        })
        .collect();

    let mut config = ArrayConfig::default();

    for (key, value) in &values {
        let Some(slot_str) = key.strip_prefix("rdevName.") else {
            continue;
        };
        let Ok(slot) = slot_str.parse::<u64>() else {
            continue;
        };
        if value.is_empty() {
            continue;
        }
        let full_path = normalize_dev(value);
        if !is_block_device(&full_path) {
            continue;
        }

        // rdevOffset is reported in 512-byte sectors; convert to bytes.
        // Unconfigured slots report 0.
        let rdev_off_sectors: u64 = values
            .get(&format!("rdevOffset.{slot}"))
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        config
            .rdev_offsets
            .insert(full_path.clone(), rdev_off_sectors * 512);

        // Slot 0 = P, slot 29 = Q; everything else is a data disk.
        if slot == 0 {
            config.parity_p = Some(full_path);
        } else if slot == NMDSTAT_Q_SLOT {
            config.parity_q = Some(full_path);
        } else {
            config.data_devs.insert(slot, full_path);
        }
    }

    Ok(config)
}

/// Load the array config; fails when no stat file exists or when the
/// primary parity disk is absent.
pub fn load() -> std::io::Result<ArrayConfig> {
    let Some(path) = stat_path() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "neither /proc/nmdstat nor /proc/mdstat found",
        ));
    };
    let config = parse_nmdstat(path)?;
    if config.parity_p.is_none() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("primary parity disk (slot 0) not found or invalid in {path}"),
        ));
    }
    Ok(config)
}

/// Ensure a device name is an absolute /dev/... path.
fn normalize_dev(name: &str) -> PathBuf {
    if name.starts_with('/') {
        PathBuf::from(name)
    } else {
        PathBuf::from(format!("/dev/{name}"))
    }
}

/// Best-effort S_ISBLK check; a missing path is not a block device.
fn is_block_device(path: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;
    match fs::metadata(path) {
        Ok(md) => md.file_type().is_block_device(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
rdevName.0=loop0
rdevOffset.0=64
rdevName.1=loop2
rdevOffset.1=64
rdevName.2=loop3
rdevOffset.2=64
rdevName.29=loop1
rdevOffset.29=64
rdevName.3=
rdevOffset.3=0
";

    #[test]
    fn parses_slots_and_offsets() {
        let values: BTreeMap<String, String> = SAMPLE
            .lines()
            .filter_map(|l| {
                let (k, v) = l.split_once('=')?;
                Some((k.to_string(), v.trim().to_string()))
            })
            .collect();
        assert_eq!(values.get("rdevName.0").unwrap(), "loop0");
        assert_eq!(values.get("rdevOffset.29").unwrap(), "64");
        assert_eq!(values.get("rdevName.3").unwrap(), "");
    }
}
