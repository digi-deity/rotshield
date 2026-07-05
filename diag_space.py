#!/usr/bin/env python3
"""Diagnostic: determine whether the chunk tree's stripe phys_base is in
array-partition space (needs rdevOffset added for raw-rdev access) or
raw-rdev space (already includes rdevOffset, no shift needed).

Strategy: take the corrupt sector we just wrote (file_offset 409600,
known corrupt byte 0x9b on /dev/nmd1p1 @ array-space 0x6564000), read the
chunk tree, and compare the reported phys_base+offset to:
  - 0x6564000 (array-partition space)
  - 0x6564000 + 0x8000 = 0x656c000 (array-partition + rdevOffset)

Whichever matches tells us what space the chunk tree uses.
"""
import os
from btrfs_recon.constants import BTRFS_SECTOR_SIZE
from btrfs_recon.parsing import (
    parse_superblock, build_chunk_tree, find_tree_root, find_extent_data,
)
from btrfs_recon.structure import ObjectId
from md_array import find_mount_for_path, get_array_config

target = '/mnt/disk1/file1'
mount_point, mount_dev = find_mount_for_path(target)
inode = os.stat(target).st_ino
FILE_OFF = 409600

config = get_array_config()
print(f'mount_dev={mount_dev} inode={inode}')
print(f'data_devs: {config.data_devs}')
print(f'rdev_offsets: {{k: hex(v) for k,v in config.rdev_offsets.items()}} = '
      f'{ {k: hex(v) for k,v in config.rdev_offsets.items()} }')

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
    chunk_phys = phys_base + (logical - chunk.begin)
    sector_phys = (chunk_phys // BTRFS_SECTOR_SIZE) * BTRFS_SECTOR_SIZE

print(f'devid={devid} logical=0x{logical:x}')
print(f'chunk.begin       = 0x{chunk.begin:x}')
print(f'stripe phys_base  = 0x{phys_base:x}')
print(f'chunk_phys target = 0x{chunk_phys:x}')
print(f'sector_phys       = 0x{sector_phys:x}')
print()

ARRAY_SPACE = 0x6564000  # where /dev/nmd1p1 shows the 0x9b corrupt byte
RAW_SHIFT   = config.raw_offset_for(config.data_devs[devid])
print(f'array-space 0x6564000 vs sector_phys 0x{sector_phys:x}: {sector_phys == ARRAY_SPACE}')
print(f'raw-shifted 0x{0x6564000 + RAW_SHIFT:x} vs sector_phys 0x{sector_phys:x}: '
      f'{sector_phys == 0x6564000 + RAW_SHIFT}')
print()
print('=> chunk tree addresses are in: '
      + ('ARRAY-PARTITION space (need +rdevOffset for raw rdev)'
         if sector_phys == ARRAY_SPACE
         else 'RAW-RDEV space (no shift needed for raw rdev)'))