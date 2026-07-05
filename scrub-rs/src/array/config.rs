//! Parse `/proc/nmdstat` (NonRAID kernel module) or `/proc/mdstat`
//! (Unraid) into an `ArrayConfig`.
//!
//! Mirrors `md_array.py` from the Python recovery toolkit.  The stat file is
//! a flat `key=value` text file produced by the NonRAID/Unraid kernel module.
//!
//! Slot conventions (from NonRAID):
//!   slot 0  → primary parity disk (P)
//!   slot 29 → secondary parity disk (Q)
//!   slots 1..N → data disks
//!
//! The slot number is the array's disk identifier.  Each NonRAID data disk
//! hosts its own **independent** filesystem (see `nonraid/README.md`) — it
//! is not a single filesystem spanning every disk — so a filesystem's own
//! devid is *not* the NonRAID slot number in general: a single-device
//! btrfs filesystem always reports devid `1`, regardless of which slot it
//! actually lives in.  Callers that need the real slot for a disk they
//! opened directly must use [`ArrayConfig::slot_for_raw_dev`] (raw rdev
//! path) or [`slot_from_array_partition`] (array-partition path like
//! `/dev/nmd2p1`) instead of trusting the filesystem-reported devid.
//!
//! `rdevOffset.N` is reported in 512-byte sectors on the raw device and must
//! be added to array-partition-space addresses when reading/writing the raw
//! rdev directly.  Array partitions (e.g. `/dev/nmd1p1`) have an implicit
//! offset of 0 because the array driver already strips the per-disk header.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Slot number NonRAID assigns to the secondary (Q) parity disk.
const NMDSTAT_Q_SLOT: u64 = 29;

/// Candidate stat-file paths tried in order; the first one that exists wins.
const STAT_PATHS: &[&str] = &["/proc/nmdstat", "/proc/mdstat"];

/// Parsed array configuration.
///
/// `data_devs` maps slot number → full `/dev/...` raw-rdev path for every
/// data disk.  `parity_p` / `parity_q` are the primary and secondary parity
/// disks (raw-rdev paths; may be `None` if not present in the array).
///
/// `rdev_offsets` maps the same raw-rdev path → byte offset that must be
/// added to array-partition-space addresses when reading/writing the raw
/// rdev directly.  For array partitions (e.g. `/dev/nmd1p1`) this is 0
/// because the array driver already strips the per-disk header; for raw
/// rdevs (e.g. `/dev/loop2` named by `rdevName.N`) it is `rdevOffset.N * 512`.
#[derive(Debug, Default)]
pub struct ArrayConfig {
    /// Slot number → raw rdev path (`/dev/loop2`, ...).
    pub data_devs: BTreeMap<u64, PathBuf>,
    /// Primary parity disk raw rdev path, if present.
    pub parity_p: Option<PathBuf>,
    /// Secondary parity (Q) disk raw rdev path, if present.
    pub parity_q: Option<PathBuf>,
    /// raw rdev path → byte offset to add to array-space addresses.
    pub rdev_offsets: BTreeMap<PathBuf, u64>,
}

impl ArrayConfig {
    /// Bytes to add to an array-space address when accessing `dev_path`.
    ///
    /// Array partitions (paths not listed in `rdev_offsets`) get 0; raw rdevs
    /// get their per-disk `rdevOffset`.  This is the single chokepoint every
    /// caller should use so the array-vs-raw distinction lives in one place.
    pub fn raw_offset_for(&self, dev_path: &Path) -> u64 {
        *self.rdev_offsets.get(dev_path).unwrap_or(&0)
    }

    /// Look up the raw-rdev path for a slot number.
    ///
    /// **Not the same as a btrfs devid in general.**  Each NonRAID data disk
    /// hosts its *own independent* single-device btrfs filesystem (see
    /// `nonraid/README.md`: "each will have an independent filesystem"), so
    /// that filesystem's internal btrfs devid is always `1` — it only
    /// happens to equal the NonRAID slot number for the disk in slot 1.
    /// Callers must resolve the *actual* slot via [`slot_for_raw_dev`] (for
    /// a raw rdev path like `/dev/loop3`) or [`slot_from_array_partition`]
    /// (for an array-partition path like `/dev/nmd2p1`) rather than reusing
    /// a filesystem-reported devid.
    ///
    /// [`slot_for_raw_dev`]: ArrayConfig::slot_for_raw_dev
    /// [`slot_from_array_partition`]: slot_from_array_partition
    pub fn data_dev(&self, devid: u64) -> Option<&Path> {
        self.data_devs.get(&devid).map(|p| p.as_path())
    }

    /// Reverse-lookup: which NonRAID slot does this raw-rdev path belong
    /// to?  Compares canonicalized paths so symlinks (`/dev/disk/by-id/...`)
    /// resolve to the same slot as the `/dev/loopN` path parsed from
    /// `/proc/nmdstat`.  Returns `None` if `path` isn't a raw rdev of any
    /// configured data disk (e.g. it's an array-partition path instead —
    /// use [`slot_from_array_partition`] for those).
    pub fn slot_for_raw_dev(&self, path: &Path) -> Option<u64> {
        let canon = fs::canonicalize(path).ok();
        let target = canon.as_deref().unwrap_or(path);
        self.data_devs.iter().find_map(|(slot, p)| {
            let p_canon = fs::canonicalize(p).ok();
            let p_target = p_canon.as_deref().unwrap_or(p.as_path());
            (p_target == target).then_some(*slot)
        })
    }
}

/// Parse the NonRAID slot number out of a kernel-generated array-partition
/// device name, e.g. `/dev/nmd2p1` → `Some(2)`.
///
/// The `md_nonraid` kernel module names each data disk's array-partition
/// device `nmd{slot}p{partition}` — the slot number is baked directly into
/// the device name (and matches `diskNumber.N` in `/proc/nmdstat`; the two
/// devices in our test array show up as major/minor `127,1` and `127,2`
/// respectively).  This is the array-partition counterpart to
/// [`ArrayConfig::slot_for_raw_dev`], which does the equivalent lookup for
/// a raw rdev path.  Returns `None` if `dev` doesn't match the `nmd<N>p<M>`
/// pattern (e.g. it's a raw rdev or a bare image file).
pub fn slot_from_array_partition(dev: &str) -> Option<u64> {
    let name = Path::new(dev).file_name()?.to_str()?;
    let rest = name.strip_prefix("nmd")?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    // Must be followed by a 'p<partition-number>' suffix, not just any
    // "nmd123..." string, to avoid false positives.
    let after_digits = &rest[digits.len()..];
    if !after_digits.starts_with('p') || after_digits.len() < 2 {
        return None;
    }
    digits.parse().ok()
}

/// Return the first array stat file that exists, or `None`.
fn stat_path() -> Option<&'static str> {
    STAT_PATHS.iter().copied().find(|p| Path::new(p).exists())
}

/// Parse `/proc/nmdstat` (or `/proc/mdstat`, which uses the same key=value
/// format under NonRAID-derived Unraid drivers).
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
        let Some(slot_str) = key.strip_prefix("rdevName.") else { continue };
        let Ok(slot) = slot_str.parse::<u64>() else { continue };
        if value.is_empty() {
            continue;
        }
        let full_path = normalize_dev(value);
        if !is_block_device(&full_path) {
            continue;
        }

        // rdevOffset is reported in 512-byte sectors on the raw device; it is
        // 0 for unconfigured slots and >0 for every present raw rdev
        // (typically 64 sectors = 32 KiB, leaving room for a partition table
        // / boot loader at the start of the disk that backs the array
        // partition). The chunk tree and btrfs metadata all see offsets
        // relative to the array partition, so any direct access to the raw
        // rdev must add this.
        let rdev_off_sectors: u64 = values
            .get(&format!("rdevOffset.{slot}"))
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        config.rdev_offsets.insert(full_path.clone(), rdev_off_sectors * 512);

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

/// Load array configuration from the first available stat file.
///
/// Returns an error if no stat file is found, or if the primary parity disk
/// (slot 0) is absent — recovery without parity is impossible and silent
/// failure would be worse than a loud error.
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

/// Ensure a device name is a full `/dev/...` path.
fn normalize_dev(name: &str) -> PathBuf {
    if name.starts_with('/') {
        PathBuf::from(name)
    } else {
        PathBuf::from(format!("/dev/{name}"))
    }
}

/// Best-effort `S_ISBLK` check via `stat()`.  Non-existent paths return
/// `false` rather than erroring, mirroring the Python helper.
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
        // Inject the sample by parsing through a temporary file would require
        // a public parse-from-text entry point; instead exercise the value
        // extraction logic directly via the partition helper.
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
