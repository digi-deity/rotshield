"""
md_array.py – helpers for reading NonRAID (/proc/nmdstat) or Unraid/mdadm
(/proc/mdstat) array configuration and mapping btrfs device IDs to block
device paths.

/proc/nmdstat  (NonRAID kernel module) is a flat key=value file:
    rdevName.0=loop0p1    ← parity P
    rdevName.29=loop1p1   ← parity Q  (slot 29 is NonRAID's convention)
    rdevName.1=loop2p1    ← data disk devid 1
    rdevName.2=loop3p1    ← data disk devid 2
    ...

/proc/mdstat (Unraid / md/mdadm) support can be added in _parse_mdstat().
"""
from __future__ import annotations

import os
import re
import sys
from dataclasses import dataclass, field

# Stat files tried in order; the first one that exists is used.
_STAT_PATHS = ['/proc/nmdstat', '/proc/mdstat']

# Slot number NonRAID assigns to the secondary (Q) parity disk.
_NMDSTAT_Q_SLOT = 29


# ─────────────────────────────────────────────────────────────────────────────
# Public data type
# ─────────────────────────────────────────────────────────────────────────────

@dataclass
class ArrayConfig:
    """Parsed array configuration.

    data_devs maps slot/btrfs-devid → full /dev/... raw-rdev path for every
    data disk. parity_p / parity_q are the primary and secondary parity
    disks (raw-rdev paths; may be None if not present in the array).

    rdev_offsets maps the same raw-rdev path → byte offset that must be
    added to array-partition-space addresses when reading/writing the raw
    rdev device directly.  For array partitions (e.g. /dev/nmd1p1) this is
    0 because the array driver already strips the per-disk header; for raw
    rdevs (e.g. /dev/loop2 named by rdevName.N) it is rdevOffset.N × 512.

    A single helper, raw_offset_for(dev_path), returns this value for any
    path so callers do not need to know which kind of device they hold.
    """
    data_devs: dict[int, str] = field(default_factory=dict)
    parity_p: str | None = None
    parity_q: str | None = None
    rdev_offsets: dict[str, int] = field(default_factory=dict)

    def raw_offset_for(self, dev_path: str) -> int:
        """Bytes to add to an array-space address when accessing ``dev_path``.

        Array partitions (paths not listed in rdev_offsets) get 0; raw rdevs
        get their per-disk rdevOffset.  This is the single chokepoint every
        caller should use so the array-vs-raw distinction lives in one place.
        """
        return self.rdev_offsets.get(dev_path, 0)


# ─────────────────────────────────────────────────────────────────────────────
# Helpers
# ─────────────────────────────────────────────────────────────────────────────

def must_be_root() -> None:
    """Exit with an error if the process is not running as root."""
    import sys
    if os.geteuid() != 0:
        sys.exit('Please run as root (sudo).')


def findmnt_source(mount_point: str) -> str:
    """Return the block device a mount point is mounted from.

    Reads /proc/mounts directly; equivalent to ``findmnt -n -o SOURCE``.
    """
    with open('/proc/mounts') as f:
        for line in f:
            fields = line.split()
            if len(fields) >= 2 and fields[1] == mount_point:
                return fields[0]
    sys.exit(f'ERROR: could not find device for mount point {mount_point}')


def find_mount_for_path(path: str) -> tuple[str, str]:
    """Return ``(mount_point, device)`` for the filesystem containing *path*.

    Selects the longest matching mount point from /proc/mounts so that nested
    mounts are handled correctly (e.g. /mnt/disk1 wins over /mnt).
    """
    real = os.path.realpath(path)
    best_mp = best_dev = None
    with open('/proc/mounts') as f:
        for line in f:
            fields = line.split()
            if len(fields) < 2:
                continue
            dev, mp = fields[0], fields[1]
            # A mount point matches if it is a prefix of the resolved path.
            if real == mp or real.startswith(mp.rstrip('/') + '/'):
                if best_mp is None or len(mp) > len(best_mp):
                    best_mp, best_dev = mp, dev
    if best_mp is None:
        sys.exit(f'ERROR: could not determine mount point for {path}')
    return best_mp, best_dev


def _is_block_device(path: str) -> bool:
    try:
        st = os.stat(path)
        return (st.st_mode & 0o170000) == 0o060000  # S_ISBLK
    except OSError:
        return False


def _normalize_dev(name: str) -> str:
    """Ensure a device name is a full /dev/ path."""
    return name if name.startswith('/') else f'/dev/{name}'


# ─────────────────────────────────────────────────────────────────────────────
# Stat-file parsers
# ─────────────────────────────────────────────────────────────────────────────

def _parse_nmdstat(path: str) -> ArrayConfig:
    """Parse /proc/nmdstat (NonRAID kernel module key=value format)."""
    values: dict[str, str] = {}
    with open(path) as f:
        for line in f:
            line = line.strip()
            if '=' in line:
                key, _, value = line.partition('=')
                values[key] = value

    config = ArrayConfig()
    for key, value in values.items():
        if not key.startswith('rdevName.'):
            continue
        try:
            slot = int(key.split('.')[1])
        except (ValueError, IndexError):
            continue

        if not value or not value.strip():
            continue

        full_path = _normalize_dev(value.strip())
        if not os.path.exists(full_path) or not _is_block_device(full_path):
            continue

        # rdevOffset is reported in 512-byte sectors on the raw device; it is
        # 0 for unconfigured slots and >0 for every present raw rdev (typically
        # 64 sectors = 32 KiB, leaving room for a partition table / boot loader
        # at the start of the disk that backs the array partition). The chunk
        # tree and btrfs metadata all see offsets relative to the array
        # partition, so any direct access to the raw rdev must add this.
        rdev_off_key = f'rdevOffset.{slot}'
        rdev_off_sectors = int(values.get(rdev_off_key, '0') or '0')
        config.rdev_offsets[full_path] = rdev_off_sectors * 512

        if slot == 0:
            config.parity_p = full_path
        elif slot == _NMDSTAT_Q_SLOT:
            config.parity_q = full_path
        else:
            config.data_devs[slot] = full_path

    return config


def _parse_mdstat(path: str) -> ArrayConfig:
    """Parse /proc/mdstat (Unraid).

    NonRAID is derived from the open-source Unraid drivers and uses the same
    key=value format as /proc/nmdstat, so the same parser applies.
    """
    return _parse_nmdstat(path)


# ─────────────────────────────────────────────────────────────────────────────
# Public API
# ─────────────────────────────────────────────────────────────────────────────

def _stat_path() -> str | None:
    """Return the first array stat file that exists, or None."""
    for p in _STAT_PATHS:
        if os.path.exists(p):
            return p
    return None


def get_array_config() -> ArrayConfig:
    """Return array configuration from /proc/nmdstat or /proc/mdstat.

    Exits with an error if no stat file is found or required disks are absent.
    """
    path = _stat_path()
    if path is None:
        sys.exit('ERROR: neither /proc/nmdstat nor /proc/mdstat found.')

    if path.endswith('nmdstat'):
        config = _parse_nmdstat(path)
    else:
        config = _parse_mdstat(path)

    if not config.parity_p:
        sys.exit(f'ERROR: primary parity disk (slot 0) not found or invalid in {path}')
    return config


def resolve_devid_to_device(devid: int) -> str:
    """Map a btrfs devid to an underlying block device path via the array config.

    Exits with an error if no stat file is found or the devid is not listed —
    writing to the wrong device could silently corrupt unrelated data.
    """
    config = get_array_config()
    dev = config.data_devs.get(devid)
    if not dev:
        sys.exit(
            f'ERROR: btrfs devid {devid} not found in array config; '
            f'refusing to proceed without a confirmed device mapping.'
        )
    return dev
