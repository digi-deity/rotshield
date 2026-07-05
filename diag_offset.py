#!/usr/bin/env python3
"""Diagnostic: verify rdevOffset translation model by reading an actual data
sector through three paths and comparing bytes + CRC32C.

  Path A — array partition  /dev/nmd1p1  at array-phys offset P
  Path B — raw rdev         /dev/loop2   at raw-phys offset P + rdevOffset.1*512
  Path C — raw rdev         /dev/loop2   at array-phys offset P        (WRONG)

A and B must match byte-for-byte; C must differ.  Confirms that every
read/write on a raw rdev needs rdevOffset added, and the array-partition
path needs no shift.
"""
import sys, os
from crc32c import crc32c
from btrfs_recon.constants import BTRFS_SECTOR_SIZE
from btrfs_recon.parsing import (
    parse_superblock, build_chunk_tree, find_tree_root, find_extent_data,
    lookup_csum,
)
from btrfs_recon.structure import ObjectId
from md_array import find_mount_for_path, get_array_config

target = '/mnt/disk1/file1'
mount_point, mount_dev = find_mount_for_path(target)
inode = os.stat(target).st_ino
print(f'mount_dev={mount_dev}  inode={inode}')
config = get_array_config()
print('data_devs:', config.data_devs)
print('rdev_offsets:', {k: hex(v) for k, v in config.rdev_offsets.items()})

with open(mount_dev, 'rb') as fp:
    sb = parse_superblock(fp)
    devid_fp = {sb.dev_item.devid: fp}
    tree = build_chunk_tree(sb, devid_fp)
    fs_root = find_tree_root(sb, tree, devid_fp, ObjectId.FsTree)
    # Use a non-zero file sector so the read isn't accidentally all-zeros
    # (the test disks were dd'd from /dev/zero, so a zero-sector read would
    # match the stored csum at any offset and hide the offset bug).
    FILE_OFF = 4096 * 100  # sector index 100 → 4-byte LE 0x64000000 repeated
    item = find_extent_data(tree, devid_fp, fs_root, inode, FILE_OFF)
    logical = item.data.ref.disk_bytenr + (FILE_OFF - item.key.offset)
    chunk = next(iter(tree.at(logical)))
    phys_base_array = chunk.data['stripes'][0][1]
    devid = chunk.data['stripes'][0][0]
    array_phys = phys_base_array + (logical - chunk.begin)
    sector_logical = (logical // BTRFS_SECTOR_SIZE) * BTRFS_SECTOR_SIZE
    csum_root = find_tree_root(sb, tree, devid_fp, ObjectId.CsumTree)
    stored_csum = lookup_csum(fp, tree, csum_root, sector_logical)

print(f'devid={devid} logical=0x{logical:x} array_phys=0x{array_phys:x}')
print(f'stored_csum=0x{stored_csum:08x}')

raw_path = config.data_devs[devid]
raw_offset = config.rdev_offsets[raw_path]
print(f'raw_path={raw_path} raw_offset_bytes={raw_offset} (0x{raw_offset:x})')

P = (array_phys // BTRFS_SECTOR_SIZE) * BTRFS_SECTOR_SIZE
print(f'sector array_phys P=0x{P:x}')

with open(mount_dev, 'rb') as fp:
    fp.seek(P); A = fp.read(BTRFS_SECTOR_SIZE)
with open(raw_path, 'rb') as fp:
    fp.seek(P + raw_offset); B = fp.read(BTRFS_SECTOR_SIZE)
with open(raw_path, 'rb') as fp:
    fp.seek(P); C = fp.read(BTRFS_SECTOR_SIZE)

print(f'A (array part)        crc=0x{crc32c(A):08x}  ==stored? {crc32c(A)==stored_csum}')
print(f'B (raw + rdevOffset)  crc=0x{crc32c(B):08x}  ==stored? {crc32c(B)==stored_csum}  (SHOULD match)')
print(f'C (raw NO shift)      crc=0x{crc32c(C):08x}  ==stored? {crc32c(C)==stored_csum}  (SHOULD NOT)')
print(f'A==B : {A==B}  (SHOULD be True)')
print(f'A==C : {A==C}  (SHOULD be False — proves offset needed)')