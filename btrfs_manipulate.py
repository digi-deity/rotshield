#!/usr/bin/env python3
# /// script
# requires-python = ">=3.10"
# dependencies = [
#   "crc32c",
#   "construct",
#   "construct-typing",
#   "intervaltree"
# ]
# ///

"""
btrfs_manipulate.py

This script intentionally introduces a single-byte corruption into the
physical storage backing a file on a Btrfs filesystem. It is intended for
experimentation with data integrity, checksum failures, and recovery behavior
on systems where parity and redundancy are provided outside of Btrfs itself
(for example SnapRAID, mergerfs+SnapRAID, Unraid, or other non-RAID/parity
schemes).

Given a file and a byte offset (or a randomly chosen offset), the script:

1. Locates the file's EXTENT_DATA item in the Btrfs metadata trees.
2. Determines the corresponding logical address within the filesystem.
3. Maps that logical address to a physical offset on the underlying block
   device using the CHUNK_TREE.
4. Finds the checksum stored in the CSUM_TREE for the affected data block.
5. Verifies that the on-disk contents currently match the stored checksum.
6. Modifies a single byte directly on the underlying device, bypassing Btrfs.
7. Verifies that the corruption causes a checksum mismatch.
8. Leaves parity intentionally stale so that recovery mechanisms can be
   studied.

The script supports both compressed and uncompressed extents. For compressed
extents, corruption is applied to the bytes as they are physically stored on
disk; no attempt is made to reason about the uncompressed representation.
From the perspective of this tool, the stored bytes are the authoritative
data protected by Btrfs checksums.

This tool is intended for educational and testing purposes only. It performs
deliberate on-disk corruption and should never be used on valuable data.
Running it on a mounted filesystem may result in checksum errors, read
failures, or permanent data loss.

The script aims to answer the question:

    "If a single physical byte on disk changes without Btrfs knowing about
    it, what will Btrfs detect and how will higher-level parity mechanisms
    behave?"

It does not attempt to repair corruption, update checksums, or simulate disk
failures. Its sole purpose is to create controlled, reproducible data
corruption scenarios.

Run with:  uv run /root/btrfs_manipulate.py <target-file>
"""
from __future__ import annotations

import os
import sys
import random
import argparse
import subprocess
from crc32c import crc32c

from btrfs_recon.constants import BTRFS_SECTOR_SIZE
from btrfs_recon.parsing import (
    parse_superblock,
    build_chunk_tree,
    find_tree_root,
    find_extent_data,
    lookup_csum,
)
from btrfs_recon.structure import ObjectId
from md_array import find_mount_for_path, must_be_root, resolve_devid_to_device


# ─────────────────────────────────────────────────────────────────────────────
# Setup
# ─────────────────────────────────────────────────────────────────────────────

def hexdump(data: bytes, base: int = 0, width: int = 16) -> str:
    """Mimic `hexdump -C` for a bytes window starting at `base`."""
    lines = []
    for off in range(0, len(data), width):
        chunk = data[off:off + width]
        hexpart = ' '.join(f'{b:02x}' for b in chunk)
        # hexdump -C pads the hex column to 48 chars (16*3 - 1)
        hexpart = hexpart.ljust(width * 3 - 1)
        asc = ''.join(chr(b) if 32 <= b < 127 else '.' for b in chunk)
        lines.append(f'{base + off:08x}  {hexpart}  |{asc}|')
    return '\n'.join(lines)


def dump_window(dev: str, label: str, phys_offset: int, count: int = 3) -> None:
    """Hex-dump a 64-byte window centred on `phys_offset` on `dev`."""
    # 64-byte window: 32 bytes before the target through 32 bytes after.
    window_size = 64
    start_offset = max(phys_offset - 32, 0)
    
    with open(dev, 'rb') as fp:
        fp.seek(start_offset)
        data = fp.read(window_size)
    print(f'--- {label} ({dev}) ---')
    print(hexdump(data, base=start_offset))


def resolve_logical_to_physical(tree, logical: int):
    """Map a btrfs logical address to (devid, phys, chunk_virt_start, chunk_phys_base)."""
    matches = list(tree.offsets(logical))
    if not matches:
        sys.exit(f'ERROR: No chunk found that contains virtual address {logical}')
    devid, phys, _n = matches[0]
    block = next(iter(tree.at(logical)))
    chunk_virt_start = block.begin
    chunk_phys_base = block.data['stripes'][0][1]
    return devid, phys, chunk_virt_start, chunk_phys_base


def btrfs_sync(mount_point: str) -> None:
    """Force btrfs to commit pending transactions (csum tree, etc.) to disk.

    `os.sync()` flushes dirty pages but btrfs may keep the csum tree changes
    in its journal until an explicit `btrfs filesystem sync` runs. Without
    this, walking the CSUM_TREE right after creating a file may yield no
    matching EXTENT_CSUM item.
    """
    subprocess.run(
        ['btrfs', 'filesystem', 'sync', mount_point],
        check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )


def compute_block_csum(dev: str, phys_offset: int,
                       block_size: int = BTRFS_SECTOR_SIZE) -> int:
    """Read `block_size` bytes from `dev` at `phys_offset` and return crc32c."""
    with open(dev, 'rb') as fp:
        fp.seek(phys_offset)
        block = fp.read(block_size)
    if len(block) < block_size:
        sys.exit(f'ERROR: short read from {dev} at {phys_offset} '
                 f'(wanted {block_size}, got {len(block)})')
    return crc32c(block)



# ─────────────────────────────────────────────────────────────────────────────
# Cache management (replaces `echo 3 > /proc/sys/vm/drop_caches`)
# ─────────────────────────────────────────────────────────────────────────────

def drop_caches() -> None:
    with open('/proc/sys/vm/drop_caches', 'w') as f:
        f.write('3')


# ─────────────────────────────────────────────────────────────────────────────
# File test data (replaces the dd | tr pipeline)
# ─────────────────────────────────────────────────────────────────────────────

def create_test_file(path: str, size_mb: int = 50) -> None:
    print(f'Creating {size_mb} MB test file at {path}')

    try:
        os.remove(path)
    except FileNotFoundError:
        pass

    with open(path, 'wb') as f:
        for sector in range(size_mb * 256):
            # one 4K sector
            pattern = sector.to_bytes(4, 'little') * 1024
            f.write(pattern)

    os.sync()


# ─────────────────────────────────────────────────────────────────────────────
# Main corruption routine
# ─────────────────────────────────────────────────────────────────────────────

def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('target',
                        help='path to the file to corrupt (must be on a mounted btrfs filesystem)')
    parser.add_argument('--size-mb', type=int, default=50,
                        help='size of the test file to create in MB, if it does not already '
                             'exist (default: %(default)s)')
    parser.add_argument('--byte-value', default=None,
                        help='hex byte value to write at the corruption point '
                             '(e.g. 0x00); if the on-disk byte already equals this value, '
                             'its bitwise complement (XOR 0xFF) is written instead to '
                             'guarantee a real change. '
                             'Default: XOR the existing byte with 0xFF (always a guaranteed flip)')
    parser.add_argument('--overwrite', action='store_true',
                        help='recreate the test file even if it already exists; '
                             'by default an existing file is reused as-is')
    parser.add_argument('--file-offset', type=int, default=None,
                        help='byte offset within the file to corrupt '
                             '(default: pick a random location across all extents); '
                             'must be in [0, file_size)')
    parser.add_argument('--dry-run', action='store_true',
                        help='resolve and print the target physical offset, but do NOT write '
                             'anything to disk')
    args = parser.parse_args()

    must_be_root()

    target = args.target
    mount_point, mount_dev = find_mount_for_path(target)

    # ─────────────────────────────────────────────────────────────────────────
    # Step 1 — Create the test file if it does not already exist
    # ─────────────────────────────────────────────────────────────────────────
    # Each 4 KiB sector is filled with its sector index repeated as a 4-byte
    # little-endian integer, making any corruption trivially visible in a hex
    # dump and allowing the sector to be identified by its content alone.
    print()
    print('=== Step 1: Creating test file ===')
    if args.overwrite or not os.path.exists(target):
        if args.overwrite:
            # Also clear any older bigfile* test siblings for cleanliness.
            target_dir = os.path.dirname(target)
            for name in os.listdir(target_dir):
                if name.startswith('bigfile'):
                    try:
                        os.remove(os.path.join(target_dir, name))
                    except OSError:
                        pass
        create_test_file(target, size_mb=args.size_mb)
    else:
        print(f'(keeping existing file {target})')
    os.sync()
    # btrfs keeps new checksum tree writes in its journal until an explicit
    # filesystem sync — without this, the CSUM_TREE walk below would find no
    # EXTENT_CSUM item covering the freshly-written extent.
    if not args.dry_run:
        btrfs_sync(mount_point)

    # ─────────────────────────────────────────────────────────────────────────
    # Step 1b — Determine the target byte offset within the file
    # ─────────────────────────────────────────────────────────────────────────
    # If the caller supplied --file-offset we use it verbatim (after a bounds
    # check). Otherwise we pick a random byte across the entire file — note
    # that for large files btrfs may have allocated multiple extents, so the
    # random offset might land anywhere across them; find_extent_data handles
    # this by walking all EXTENT_DATA items for the inode.
    file_size = os.path.getsize(target)
    if args.file_offset is not None:
        file_offset = args.file_offset
        if not (0 <= file_offset < file_size):
            sys.exit(
                f'ERROR: --file-offset {file_offset} is out of bounds '
                f'for a file of {file_size} bytes '
                f'(valid range: 0 – {file_size - 1})'
            )
        print(f'File byte offset  : {file_offset} (0x{file_offset:x}) [user-specified]')
    else:
        file_offset = random.randrange(file_size)
        print(f'File byte offset  : {file_offset} (0x{file_offset:x}) '
              f'[random, file size={file_size}]')

    # ─────────────────────────────────────────────────────────────────────────
    # Step 2 — Find the file's physical location via btrfs on-disk structures
    # ─────────────────────────────────────────────────────────────────────────
    # btrfs uses its own virtual (logical) address space; a separate CHUNK_TREE
    # translates those to physical disk offsets. We walk both with btrfs_recon
    # (no DB, no btrfs-progs) to find:
    #   - the file's EXTENT_DATA record covering file_offset in the FS_TREE
    #   - the chunk mapping it to a physical byte offset on a real device
    print()
    print('=== Step 2a: Opening btrfs filesystem and walking trees ===')
    with open(mount_dev, 'rb') as fp:
        superblock = parse_superblock(fp)
        devid_fp_map = {superblock.dev_item.devid: fp}
        tree = build_chunk_tree(superblock, devid_fp_map)

        inode = os.stat(target).st_ino
        print(f'target: {target}  inode={inode}')
        print(f'btrfs device: {mount_dev}')
        try:
            fs_root = find_tree_root(superblock, tree, devid_fp_map, ObjectId.FsTree)
            extent_item = find_extent_data(tree, devid_fp_map, fs_root, inode, file_offset)
        except KeyError as e:
            sys.exit(f'ERROR: {e}')

        print()
        print('=== DEBUG: raw extent fields ===')
        print(f'extent_item.key.offset             = {extent_item.key.offset}')
        print(f'extent_item.data.ref.disk_bytenr   = {extent_item.data.ref.disk_bytenr}')
        print(f'extent_item.data.ref.offset        = {extent_item.data.ref.offset}')
        print(f'extent_item.data.ref.num_bytes     = {extent_item.data.ref.num_bytes}')
        print(f'extent_item.data.ref.disk_num_bytes= {extent_item.data.ref.disk_num_bytes}')
        print(f'type(extent_item.data.ref)         = {type(extent_item.data.ref)}')
        print()

        if extent_item.data.type != extent_item.data.type.REGULAR:
            sys.exit(f'ERROR: first extent is not REGULAR (type={extent_item.data.type})')
        virt_addr = extent_item.data.ref.disk_bytenr
        extent_size = extent_item.data.ref.disk_num_bytes

        # Translate the file-level byte offset into the logical (virtual) disk
        # address of that exact byte, then find the sector boundary that btrfs
        # uses to index its per-sector checksums.
        #
        # data_ref_offset is the extent's internal start offset (non-zero only
        # for cloned/reflinked extents; 0 for normal writes).
        data_ref_offset = getattr(extent_item.data.ref, 'offset', 0)

        if extent_item.data.ref.num_bytes == extent_item.data.ref.disk_num_bytes:
            # Uncompressed extent: preserve the selected file byte, regardless of
            # whether it was supplied explicitly or chosen randomly.
            offset_in_extent = file_offset - extent_item.key.offset
        else:
            # Compressed extent: file offsets do not map linearly to stored bytes, so
            # choose a random byte among the bytes actually stored on disk.
            offset_in_extent = random.randrange(extent_item.data.ref.disk_num_bytes)

        target_logical = virt_addr + data_ref_offset + offset_in_extent
        sector_logical = (target_logical // BTRFS_SECTOR_SIZE) * BTRFS_SECTOR_SIZE

        # ───────────────────────────────────────────────────────────────────
        # Step 2b — Map target logical address → physical offset on device
        # ───────────────────────────────────────────────────────────────────
        print()
        print('=== Step 2b: Mapping logical → physical offset ===',)
        devid, real_phys_offset, chunk_virt_start, chunk_phys_base = \
            resolve_logical_to_physical(tree, target_logical)
        sector_phys_offset = real_phys_offset - (target_logical - sector_logical)
        print(f'Physical offset: {real_phys_offset} (0x{real_phys_offset:x})')

        # ───────────────────────────────────────────────────────────────────
        # Step 2c — Translate btrfs devid → actual block device path
        # ───────────────────────────────────────────────────────────────────
        print()
        underlying_dev = resolve_devid_to_device(devid)
        print(f'Block device: {underlying_dev}')

        # ───────────────────────────────────────────────────────────────────
        # Step 2d — Locate the stored checksum for the file's target data block
        # ───────────────────────────────────────────────────────────────────
        # Walk the CSUM_TREE (objectid 7) to find the EXTENT_CSUM leaf item
        # whose logical range covers the target byte's sector. The stored
        # CRC32C is used in Steps 3a and 5a to prove that the silent corruption
        # actually invalidates the btrfs checksum.
        #
        # The stored checksum is read from the array device (the superblock
        # source); the *computed* checksum is read from the underlying data
        # disk partition — the device we write to in Step 4, which is never
        # seen by the array layer so parity remains stale.
        if superblock.csum_type != 0:
            print()
            print(f'=== Step 2d: skipping checksum verification '
                  f'(unsupported csum_type={superblock.csum_type}; only CRC32C=0 supported) ===')
            stored_csum_int = None
        else:
            print()
            print(f'=== Step 2d: Locating stored checksum (CSUM_TREE walk) ===')
            try:
                csum_root = find_tree_root(superblock, tree, devid_fp_map, ObjectId.CsumTree)
                print(f'CSUM_TREE root: 0x{csum_root:x}')
                stored_csum_int = lookup_csum(fp, tree, csum_root, sector_logical)
            except KeyError as e:
                sys.exit(f'ERROR: {e} (was a btrfs filesystem sync forced after creation?)')
            print(f'Stored csum (uint32 LE) : 0x{stored_csum_int:08x}')

        # ───────────────────────────────────────────────────────────────────
        # Step 3 — Sanity check: confirm both devices show 0xFF at the target
        # ───────────────────────────────────────────────────────────────────
        print()
        print(f'=== Step 3: Sanity check — hex dump at target offset {real_phys_offset} ===')
        dump_window(underlying_dev, 'Underlying partition', real_phys_offset)

        # ───────────────────────────────────────────────────────────────────
        # Step 3a — Pre-corruption: confirm stored csum matches computed csum
        # ───────────────────────────────────────────────────────────────────
        # Independently compute the CRC32C of the target 4K data sector,
        # reading from the UNDERLYING partition (the device we will corrupt).
        # For healthy data, this MUST equal the value stored in the CSUM_TREE.
        if stored_csum_int is not None:
            print()
            print('=== Step 3a: Pre-corruption checksum verification ===')
            computed_pre = compute_block_csum(underlying_dev, sector_phys_offset)
            print(f'  stored csum   : 0x{stored_csum_int:08x}')
            print(f'  computed csum : 0x{computed_pre:08x}')
            if stored_csum_int == computed_pre:
                print('  ✓ MATCH — on-disk data agrees with stored checksum '
                      '(data is healthy; corruption below will be detectable)')
            else:
                sys.exit('  ✗ MISMATCH — expected match before corruption; '
                         'on-disk data already differs from CSUM_TREE')

        # ───────────────────────────────────────────────────────────────────
        # Step 4 — Inject the silent corruption
        # ───────────────────────────────────────────────────────────────────
        # Write the chosen byte value directly to the underlying partition at
        # the exact physical offset of the target file byte, bypassing btrfs
        # (so checksums go stale) and the NonRAID array (so parity disks are
        # NOT updated and cannot heal it).
        byte_value = args.byte_value and int(args.byte_value, 0)
        byte_offset = real_phys_offset
        print()
        if args.dry_run:
            print(f'=== Step 4: (dry-run) Would corrupt {underlying_dev} at byte offset {byte_offset} ===')
            print('(dry-run: not actually writing)')
        else:
            with open(underlying_dev, 'r+b') as raw_fp:
                raw_fp.seek(byte_offset)
                current = raw_fp.read(1)[0]
                if byte_value is None:
                    byte_value = current ^ 0xFF
                elif current == byte_value:
                    byte_value = byte_value ^ 0xFF
                    print(f'  NOTE: on-disk byte is already 0x{current:02x}; '
                          f'writing 0x{byte_value:02x} instead so corruption actually takes effect')
                print(f'=== Step 4: Writing 0x{byte_value:02x} to {underlying_dev} at byte offset {byte_offset} ===')
                raw_fp.seek(byte_offset)
                raw_fp.write(bytes([byte_value]))
                raw_fp.flush()
                os.fsync(raw_fp.fileno())
            print(f'Flipped byte on {underlying_dev}')
            print(f'Array device {mount_dev} was NOT touched — parity remains stale')

        # ───────────────────────────────────────────────────────────────────
        # Step 5 — Drop page cache and confirm the corruption is on disk
        # ───────────────────────────────────────────────────────────────────
        # Linux caches recently-read disk data in RAM. Without flushing, reads
        # from either device might return the old in-memory bytes rather than
        # the now-corrupted on-disk bytes, making the hex dump misleading.
        print()
        print('=== Step 5: Drop page cache and verify corruption is on disk ===')
        os.sync()
        if args.dry_run:
            print('(dry-run: not dropping caches or re-dumping)')
        else:
            drop_caches()
            dump_window(underlying_dev, 'Underlying partition (post-corruption)', real_phys_offset)

        # ───────────────────────────────────────────────────────────────────
        # Step 5a — Post-corruption: confirm stored csum NO LONGER matches
        # ───────────────────────────────────────────────────────────────────
        if stored_csum_int is not None and not args.dry_run:
            print()
            print('=== Step 5a: Post-corruption checksum verification ===')
            computed_post = compute_block_csum(underlying_dev, sector_phys_offset)
            print(f'  stored csum   : 0x{stored_csum_int:08x}')
            print(f'  computed csum : 0x{computed_post:08x}')
            if stored_csum_int != computed_post:
                print('  ✓ MISMATCH — corruption successfully invalidated the on-disk checksum')
            else:
                sys.exit('  ✗ ERROR — checksum still matches! Corruption failed to change the data block.')

    print()
    print('=== Summary ===')
    if args.dry_run:
        print(f'(dry-run) Would corrupt {underlying_dev} at offset {byte_offset} '
              f'(file offset {file_offset})')
    else:
        print(f'Corrupted byte  : {underlying_dev} at offset {byte_offset} '
              f'(file offset {file_offset})')
        print(f'Parity state    : stale — cannot heal this')
        print(f'btrfs           : will report checksum error on next read of {target}')


if __name__ == '__main__':
    main()