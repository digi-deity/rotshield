#!/usr/bin/env python3
# /// script
# requires-python = ">=3.10"
# dependencies = [
#   "crc32c>=2.2.post0",
#   "btrfs-recon",  # local package at /root/btrfs-recon (resolved below)
# ]
#
# [tool.uv.sources]
# btrfs-recon = { path = "/root/btrfs-recon" }
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

Run with:  uv run /root/btrfs_manipulate.py
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
from md_array import findmnt_source, must_be_root, resolve_devid_to_device


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
    """Hex-dump a window centred just before `phys_offset` on `dev`.

    Mirrors the shell `dump_window` helper: read `count` blocks of 2048 bytes
    starting one block before the file's physical start, so it is obvious where
    the file data begins.
    """
    # Modified to focus on a small window around the target offset instead of full blocks
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
    parser.add_argument('--target', default='/mnt/disk1/bigfile2.bin',
                        help='path to the test file to corrupt (default: %(default)s)')
    parser.add_argument('--mount-point', default='/mnt/disk1',
                        help='mount point of the btrfs filesystem (default: %(default)s)')
    parser.add_argument('--device', default=None,
                        help='block device the btrfs filesystem lives on '
                             '(default: auto-detect from mount point)')
    parser.add_argument('--size-mb', type=int, default=50,
                        help='size of the test file to create in MB (default: %(default)s)')
    parser.add_argument('--byte-value', default='0x00',
                        help='byte to write at the corruption point (default: %(default)s)')
    parser.add_argument('--overwrite', action='store_true',
                        help='recreate the test file even if it already exists; '
                             'by default an existing file is reused as-is')
    parser.add_argument('--file-offset', type=int, default=None,
                        help='byte offset within the file to corrupt '
                             '(default: pick a random location across all extents); '
                             'must be in [0, file_size)')
    parser.add_argument('--dry-run', action='store_true',
                        help='resolve and print the file offset, but do NOT corrupt it')
    args = parser.parse_args()

    must_be_root()

    mount_dev = args.device or findmnt_source(args.mount_point)
    target = args.target

    # ─────────────────────────────────────────────────────────────────────────
    # Step 1 — Create a recognisable test file (all 0xFF)
    # ─────────────────────────────────────────────────────────────────────────
    # 0xFF is chosen so any corruption is trivially visible in a hex dump:
    # a single 0x00 byte stands out immediately against a wall of FFs.
    #
    # 300 MB forces btrfs to allocate a dedicated data chunk, giving us a
    # single clean extent to resolve (small files get packed inline or mixed
    # into metadata nodes, making the offset math messy).
    print()
    print('=== Step 1: Creating test file (all 0xFF) ===')
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
        btrfs_sync(args.mount_point)

    # ─────────────────────────────────────────────────────────────────────────
    # Step 1b — Determine the target byte offset within the file
    # ─────────────────────────────────────────────────────────────────────────
    # If the caller supplied --file-offset we use it verbatim (after a bounds
    # check). Otherwise we pick a random byte across the entire file — note
    # that for large files btrfs may have allocated multiple extents, so the
    # random offset might land anywhere across them; find_file_extent handles
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
        print(f'Found EXTENT_DATA for inode {inode}')
        print(f'  extent key offset (file)   = {extent_item.key.offset}')
        print(f'  disk_bytenr (virtual addr) = {virt_addr} (0x{virt_addr:x})')
        print(f'  disk_num_bytes             = {extent_size}')
        print(f'  num_bytes                  = {extent_item.data.ref.num_bytes}')
        print(f'  disk_num_bytes             = {extent_size}')

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

        print()
        print('=== DEBUG ===')
        print(f'extent key.offset      = {extent_item.key.offset}')
        print(f'ref.offset             = {data_ref_offset}')
        print(f'disk_bytenr            = {virt_addr}')
        print(f'num_bytes              = {extent_item.data.ref.num_bytes}')
        print(f'disk_num_bytes         = {extent_item.data.ref.disk_num_bytes}')
        print(f'file_offset            = {file_offset}')
        print(f'offset_in_extent       = {offset_in_extent}')
        print(f'target_logical         = {target_logical}')
        print(f'sector_logical         = {sector_logical}')

        print(f'  offset in extent           = {offset_in_extent}')
        print(f'  target logical address     = {target_logical} (0x{target_logical:x})')
        print(f'  sector logical address     = {sector_logical} (0x{sector_logical:x})')

        print()
        print('=== DEBUG: possible scrub windows ===')

        for size_kb in (4, 8, 16, 32, 64, 128, 256):
            size = size_kb * 1024
            start = target_logical & ~(size - 1)

            print(
                f'{size_kb:3d} KiB window: '
                f'start={start:#x}, '
                f'offset={target_logical - start}'
            )

        print()

        print()
        print('=== DEBUG: btrfs logical-resolve ===')

        for logical in (
            target_logical,
            sector_logical,
            sector_logical - 4096,
            sector_logical - 2 * 4096,
            sector_logical - 7 * 4096,
        ):
            print(f'\nlogical = {logical:#x}')

            try:
                out = subprocess.check_output(
                    [
                        'btrfs',
                        'inspect-internal',
                        'logical-resolve',
                        str(logical),
                        os.path.dirname(target),
                    ],
                    text=True,
                )

                print(out.rstrip())

            except subprocess.CalledProcessError as e:
                print('logical-resolve failed')
                print(e.output)

        # ───────────────────────────────────────────────────────────────────
        # Step 2b — Map target logical address → physical offset on device
        # ───────────────────────────────────────────────────────────────────
        print()
        print('=== Step 2b: Mapping logical address to physical offset (CHUNK_TREE) ===')
        devid, real_phys_offset, chunk_virt_start, chunk_phys_base = \
            resolve_logical_to_physical(tree, target_logical)
        # The chunk mapping is linear, so the sector-aligned physical offset is
        # simply real_phys_offset minus the byte's position within its sector.
        sector_phys_offset = real_phys_offset - (target_logical - sector_logical)

        # ───────────────────────────────────────────────────────────────────
        # Step 2c — Translate btrfs devid → actual block device path
        # (moved up: needed by the neighbouring-sectors debug dump below)
        # ───────────────────────────────────────────────────────────────────
        print()
        print(f'=== Step 2c: Resolving btrfs devid {devid} → block device path ===')
        underlying_dev = resolve_devid_to_device(devid, mount_dev)
        print(f'devid {devid} → {underlying_dev}')

        print()
        print("=== DEBUG: neighbouring sectors ===")
        for i in range(-2, 3):
            off = sector_phys_offset + i*4096
            with open(underlying_dev, "rb") as dbg_fp:
                dbg_fp.seek(off)
                d = dbg_fp.read(8)

            print(
                f"{i:+d} sectors "
                f"phys={off:#x} "
                f"bytes={d.hex(' ')}"
            )
        print()
        print(f'Chunk virtual start  : {chunk_virt_start}')
        print(f'Chunk physical base  : {chunk_phys_base}')
        print(f'Target physical byte : {real_phys_offset}')
        print(f'Sector phys offset   : {sector_phys_offset}')
        print(f'Logical→physical delta = {real_phys_offset - target_logical}')
        print(f'btrfs devid          : {devid}')

        # ───────────────────────────────────────────────────────────────────
        # Step 2c — Translate btrfs devid → actual block device path
        # ───────────────────────────────────────────────────────────────────
        print()
        print(f'=== Step 2c: Resolving btrfs devid {devid} → block device path ===')
        underlying_dev = resolve_devid_to_device(devid, mount_dev)
        print(f'devid {devid} → {underlying_dev}')

        # ───────────────────────────────────────────────────────────────────
        # Step 2d — Locate the stored checksum for the file's target data block
        # ───────────────────────────────────────────────────────────────────
        # This goes beyond the original shell script — btrfs_manipulate.sh does
        # NOT compute or read checksums. We walk the dedicated CSUM_TREE
        # (objectid 7) and find the EXTENT_CSUM leaf item whose logical range
        # covers the target byte's sector, then read its raw CRC32C bytes
        # so we can prove (in Steps 3a and 5a) that the silent corruption
        # actually invalidates the on-disk btrfs checksum, not just the file's
        # visible data.
        #
        # The stored checksum is read from the same array device we're already
        # parsing trees from; the *computed* checksum comes from the underlying
        # partition (we corrupt THAT device in Step 4, and the array device
        # would otherwise just hand back the cached, uncorrupted bytes).
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
        print(f'=== Step 3: Sanity check — both devices should show 0xFF at offset {real_phys_offset} ===')
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
        byte_value = int(args.byte_value, 0)
        byte_offset = real_phys_offset
        print()
        print(f'=== Step 4: Writing 0x{byte_value:02x} to {underlying_dev} at byte offset {byte_offset} ===')
        if args.dry_run:
            print('(dry-run: not actually writing)')
        else:
            with open(underlying_dev, 'r+b') as raw_fp:
                raw_fp.seek(byte_offset)
                current = raw_fp.read(1)[0]
                if current == byte_value:
                    byte_value = byte_value ^ 0xFF  # guarantee a real change
                    print(f'  NOTE: on-disk byte is already 0x{current:02x}; '
                        f'writing 0x{byte_value:02x} instead so corruption actually takes effect')
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