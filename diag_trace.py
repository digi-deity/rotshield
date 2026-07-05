#!/usr/bin/env python3
"""Trace the FULL offset chain used by corrupt + recover, byte by byte.

Reproduces what btrfs_manipulate.py computes (Step 4 corrupt write) and what
recover.py computes (handle_failure + recover_sector + write_recovered_data),
then reads the byte at every plausible offset on:
  - the array partition (/dev/nmd1p1) — chunk tree space
  - the raw rdev (/dev/loop2)         — chunk tree space, +rdevOffset, ...
  - the parity disk (/dev/loop0)      — same trio for parity reads

This shows unambiguously:
  1. where the corruption write actually landed,
  2. where each disk's "same offset" XOR read pulls from in recover.py,
  3. whether recover.py reads/writes at the same raw byte the corruptor did.

Run AFTER btrfs_manipulate.py has corrupted a known file offset.
"""
import os
from btrfs_recon.constants import BTRFS_SECTOR_SIZE
from btrfs_recon.parsing import (
    parse_superblock, build_chunk_tree, find_tree_root, find_extent_data,
    lookup_csum,
)
from btrfs_recon.structure import ObjectId
from md_array import find_mount_for_path, get_array_config

FILE_OFF = 409600
# sector-100 first byte is 0x64 (LE) untouched; corruptor flipped to 0x9b
EXPECTED = 0x64
CORRUPT  = 0x9b


def read_byte(path, off):
    try:
        with open(path, 'rb') as f:
            f.seek(off); return f.read(1)[0]
    except OSError as e:
        return None


target = '/mnt/disk1/file1'
mount_point, mount_dev = find_mount_for_path(target)
inode = os.stat(target).st_ino

config = get_array_config()
raw_shift = config.raw_offset_for('/dev/loop2')
print(f'mount_dev={mount_dev} inode={inode}')
print(f'data_devs: {config.data_devs}')
print(f'parity_p: {config.parity_p}')
print(f'rdev_offsets: { {k: hex(v) for k,v in config.rdev_offsets.items()} }')
print(f'raw_shift for /dev/loop2 = 0x{raw_shift:x}')
print()

# --- btrfs metadata: chunk tree target (array-partition-space) ---
with open(mount_dev, 'rb') as fp:
    sb = parse_superblock(fp)
    devid_fp = {sb.dev_item.devid: fp}
    tree = build_chunk_tree(sb, devid_fp)
    fs_root = find_tree_root(sb, tree, devid_fp, ObjectId.FsTree)
    item = find_extent_data(tree, devid_fp, fs_root, inode, FILE_OFF)
    logical = item.data.ref.disk_bytenr + (FILE_OFF - item.key.offset)
    chunk = next(iter(tree.at(logical)))
    phys_base = chunk.data['stripes'][0][1]
    devid = chunk.data['stripes'][0][0]
    array_phys = phys_base + (logical - chunk.begin)

print(f'CHUNK TREE: devid={devid} logical=0x{logical:x} '
      f'array_phys=0x{array_phys:x}')
print()

# — candidate offsets on each device —
SHIFT = raw_shift
candidates = ['array_phys', 'array_phys+shift', 'phys_base+SHIFT+...']
array_part = '/dev/nmd1p1'
data_rdev   = '/dev/loop2'
parity_rdev = config.parity_p

print(f'{"device":<14} {"off label":<20} {"off hex":<12} byte')
print('-' * 70)
for label, off in [
    ('array_phys',           array_phys),
    ('array_phys+shift',     array_phys + SHIFT),
]:
    for path in [array_part, data_rdev, parity_rdev]:
        b = read_byte(path, off)
        bhex = f'0x{b:02x}' if b is not None else 'N/A'
        print(f'{path:<14} {label:<20} 0x{off:<10x} {bhex}')
    print()

# --- what each script actually used ---
print('=== WHAT EACH SCRIPT WROTE / READ ===')
print()
print(f'btrfs_manipulate.py Step 4 corrupt write:')
print(f'  underlying_dev = /dev/loop2 (raw rdev)')
print(f'  byte_offset (array_phys)  = 0x{array_phys:x}')
print(f'  raw_shift = 0x{SHIFT:x}')
print(f'  → seeks /dev/loop2 at 0x{array_phys + SHIFT:x}')
print(f'  byte there now = 0x{read_byte(data_rdev, array_phys + SHIFT):02x}')
print()
print(f'recover.py handle_failure + recover_sector + write_recovered_data:')
print(f'  base_phys (from chunk tree, array space) = 0x{array_phys:x}')
print(f'  base_phys += raw_offset_for(failing_path) → 0x{array_phys + SHIFT:x}')
print(f'  → reads failing_path (/dev/loop2) at 0x{array_phys + SHIFT:x}: '
      f'byte=0x{read_byte(data_rdev, array_phys + SHIFT):02x}')
print(f'  → reads parity_p (/dev/loop0)  at 0x{array_phys + SHIFT:x}: '
      f'byte=0x{read_byte(parity_rdev, array_phys + SHIFT):02x}')
print(f'  → writes failing_path (/dev/loop2) at 0x{array_phys + SHIFT:x}')
print()

# Compare: is the parity read actually mirroring data at the same RAW byte?
# Each raw rdev (loop0, loop2..) has its own rdevOffset, all the same here (=0x8000).
# So "same raw byte across all rdevs" = 0x{array_phys + SHIFT} universally.
print('Cross-check: same RAW byte on every rdev?')
for path, dev in [('/dev/loop0','parity'), ('/dev/loop2','data1'),
                  ('/dev/loop3','data2')]:
    shr = config.raw_offset_for(path)
    b = read_byte(path, array_phys + shr)
    print(f'  {path:<12} (rdevOffset=0x{shr:x}) @ 0x{array_phys + shr:x}: '
          f'0x{b:02x}' if b is not None else f'  {path}: N/A')