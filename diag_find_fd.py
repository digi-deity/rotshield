#!/usr/bin/env python3
"""Hand-off diagnostic: settle once and for all where btrfs_manipulate.py wrote
the 0xfd corruption on the raw rdev /dev/loop2, and where recover.py wrote the
0x02 recovery byte.

We re-run btrfs_manipulate.py with a known --byte-value and a non-zero file
offset, then scan a wide window on /dev/loop2 byte-by-byte for the marker byte.
We then do the same on /dev/loop0 (parity) and on /dev/nmd1p1 (array partition,
no shift).

Output: the exact raw offsets on /dev/loop2 that contain 0xfd / 0x02, and the
exact array-space offset on /dev/nmd1p1 that contains 0xfd. From those numbers
the per-disk rdevOffset shift falls out by subtraction.
"""
import os, subprocess, sys, time

def be_root():
    if os.geteuid() != 0:
        sys.exit("must be root")

def run(cmd):
    return subprocess.run(cmd, check=True, capture_output=True, text=True)

def scan_byte(dev: str, byte_val: int, region_start: int, region_end: int,
              step: int = 4096) -> list[int]:
    """Find every page-aligned offset in [region_start, region_end) where the
    first byte equals byte_val."""
    found = []
    with open(dev, 'rb') as f:
        off = region_start
        while off < region_end:
            f.seek(off)
            buf = f.read(1)
            if buf and buf[0] == byte_val:
                found.append(off)
            off += step
    return found

def scan_first_byte_in_4k(dev: str, byte_val: int,
                          region_start: int, region_end: int) -> list[int]:
    """Scan every 4 KiB sector and report sectors whose first byte == byte_val."""
    return scan_byte(dev, byte_val, region_start, region_end, step=4096)

be_root()
MARKER = 0xfd
SECTOR_INDEX = 100  # sector index 100 → bytes 0x64000000 * 1024 — non-zero so reads aren't zeros
FILE_OFF = SECTOR_INDEX * 4096  # 409600 bytes

# Fresh corruption with a deterministic byte value
run(['rm', '-f', '/mnt/disk1/file1'])
subprocess.run([
    sys.executable, 'btrfs_manipulate.py',
    '/mnt/disk1/file1', '--size-mb', '50',
    '--file-offset', str(FILE_OFF),
    '--byte-value', f'0x{MARKER:02x}',
], check=True, cwd=os.path.dirname(os.path.abspath(__file__)))

from btrfs_recon.constants import BTRFS_SECTOR_SIZE
from btrfs_recon.parsing import (
    parse_superblock, build_chunk_tree, find_tree_root, find_extent_data,
)
from btrfs_recon.structure import ObjectId
from md_array import find_mount_for_path, get_array_config

target = '/mnt/disk1/file1'
mount_point, mount_dev = find_mount_for_path(target)
inode = os.stat(target).st_ino
with open(mount_dev, 'rb') as fp:
    sb = parse_superblock(fp)
    devid_fp = {sb.dev_item.devid: fp}
    tree = build_chunk_tree(sb, devid_fp)
    fs_root = find_tree_root(sb, tree, devid_fp, ObjectId.FsTree)
    item = find_extent_data(tree, devid_fp, fs_root, inode, FILE_OFF)
    logical = item.data.ref.disk_bytenr + (FILE_OFF - item.key.offset)
    chunk = next(iter(tree.at(logical)))
    array_phys = chunk.data['stripes'][0][1] + (logical - chunk.begin)
    devid = chunk.data['stripes'][0][0]

print(f"target file offset : {FILE_OFF} (sector {FILE_OFF // 4096})")
print(f"devid              : {devid}")
print(f"array_phys (nmd1p1): 0x{array_phys:x}")
config = get_array_config()
raw_path = config.data_devs[devid]
raw_shift = config.raw_offset_for(raw_path)
print(f"raw rdev           : {raw_path}")
print(f"rdevOffset bytes   : 0x{raw_shift:x}")

# drain caches so we see on-disk state
os.sync()
with open('/proc/sys/vm/drop_caches', 'w') as f:
    f.write('3')
for dev in ['/dev/nmd1p1', '/dev/loop0', '/dev/loop2', '/dev/loop3']:
    try:
        run(['blockdev', '--flushbufs', dev])
    except subprocess.CalledProcessError:
        pass

# Wide scan ± 1 MiB around the chunk-tree array_phys
window = 1 << 20
lo = array_phys - window
hi = array_phys + window
print(f"\nscanning /dev/nmd1p1 (array partition) for 0x{MARKER:02x} "
      f"in [0x{lo:x}, 0x{hi:x}) ...")
nmd_hits = scan_first_byte_in_4k('/dev/nmd1p1', MARKER, max(lo,0), hi)
print(f"  /dev/nmd1p1 hits: {[hex(h) for h in nmd_hits]}")

# On raw rdev, the chunk-tree array_phys corresponds to raw_phys = array_phys + raw_shift
raw_lo = lo + raw_shift
raw_hi = hi + raw_shift
print(f"scanning /dev/loop2 (raw rdev) for 0x{MARKER:02x} "
      f"in [0x{raw_lo:x}, 0x{raw_hi:x}) ...")
loop2_hits = scan_first_byte_in_4k('/dev/loop2', MARKER, max(raw_lo,0), raw_hi)
print(f"  /dev/loop2 hits: {[hex(h) for h in loop2_hits]}")

print(f"\nPairwise diffs (loop2 hit − array hit):")
for nh in nmd_hits[:5]:
    for lh in loop2_hits[:5]:
        print(f"  0x{lh:x} − 0x{nh:x} = 0x{lh-nh:x}")