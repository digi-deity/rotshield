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
"""btrfs_manipulate.py — Python port of btrfs_manipulate.sh.

Injects a "silent" corruption into a btrfs file to reproduce the exact failure
mode this project exists to solve:

    btrfs detects the damage via its per-block checksums
    \u2193  but  \u2193
    the NonRAID parity disks are stale and cannot heal it

"Silent" = we write directly to the raw block device, bypassing both the
btrfs filesystem layer and the NonRAID array layer. Neither sees the write,
so neither updates its checksums or parity.

This version uses the stripped-down, inspection-only `btrfs_recon` package
(no DB, no async, no btrfs-progs shelling out) to locate a file's data on
disk by walking the btrfs on-disk structures directly.

Run with:  sudo uv run /root/btrfs_manipulate.py
"""
from __future__ import annotations

import os
import re
import sys
import struct
import argparse
import subprocess
from collections import deque
from pathlib import Path

from crc32c import crc32c

from btrfs_recon.parsing import parse_superblock, build_chunk_tree, walk_btree, parse_at
from btrfs_recon.structure import KeyType, ObjectId, TreeNode, Header


# ─────────────────────────────────────────────────────────────────────────────
# Setup
# ─────────────────────────────────────────────────────────────────────────────

def must_be_root() -> None:
    if os.geteuid() != 0:
        sys.exit("Please run as root (sudo).")


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
    block = 2048
    skip = max(phys_offset // block - 1, 0)
    size = block * count
    with open(dev, 'rb') as fp:
        fp.seek(skip * block)
        data = fp.read(size)
    print(f'--- {label} ({dev}) ---')
    print(hexdump(data, base=skip * block))


# ─────────────────────────────────────────────────────────────────────────────
# btrfs structure inspection (replaces `btrfs inspect-internal dump-tree`)
# ─────────────────────────────────────────────────────────────────────────────

def find_file_extent(
    fp,
    superblock,
    tree,
    devid_fp_map: dict,
    mount_point: str,
    target_file: str,
):
    """Walk root tree → FS_TREE → find first EXTENT_DATA for the file's inode.

    Returns (inode, file_extent_item). Raises if not found.

    This replaces Step 2a of the shell script:
        VIRT_ADDR=$(btrfs inspect-internal dump-tree -t FS_TREE "$REAL_DEV" | awk ...)
    """
    inode = os.stat(target_file).st_ino
    print(f'target: {target_file}  inode={inode}')

    # Real device btrfs is mounted on (e.g. /dev/nmd1p1 for the array).
    real_dev = findmnt_source(mount_point)
    print(f'btrfs device: {real_dev}')

    # 1. Find FS_TREE root (objectid 5, ROOT_ITEM) in the root tree.
    fs_root_bytenr = None
    for item in walk_btree(superblock.root, tree, devid_fp_map):
        if (item.key.objectid == ObjectId.FsTree
                and item.key.ty == KeyType.RootItem):
            fs_root_bytenr = item.data.bytenr
            break
    if fs_root_bytenr is None:
        sys.exit('ERROR: FS_TREE ROOT_ITEM not found in root tree')

    # 2. Walk the FS_TREE, find the first EXTENT_DATA at key offset 0.
    for item in walk_btree(fs_root_bytenr, tree, devid_fp_map):
        if (item.key.objectid == inode
                and item.key.ty == KeyType.ExtentData
                and item.key.offset == 0):
            return inode, item
    sys.exit(f'ERROR: No EXTENT_DATA found for inode {inode} in FS_TREE')


def resolve_logical_to_physical(tree, logical: int):
    """Map a btrfs logical (virtual) address to a physical byte offset.

    Replaces Step 2b of the shell script:
        btrfs inspect-internal dump-tree -t CHUNK "$REAL_DEV" | awk ...

    Returns (devid, physical_offset, chunk_virtual_start, chunk_physical_base).
    Yields the first mapping (callers can pick a copy for mirrored chunks).
    """
    matches = list(tree.offsets(logical))
    if not matches:
        sys.exit(f'ERROR: No chunk found that contains virtual address {logical}')

    devid, phys, _n = matches[0]
    # Find the containing interval so we can report the chunk boundaries too.
    block = next(iter(tree.at(logical)))
    chunk_virt_start = block.begin
    chunk_phys_base = block.data['stripes'][0][1]
    return devid, phys, chunk_virt_start, chunk_phys_base


# ─────────────────────────────────────────────────────────────────────────────
# Checksum tree inspection (extends beyond the bash script — the original
# btrfs_manipulate.sh does NOT compute or verify checksums; this is a new
# feature that proves the silent corruption actually invalidates the on-disk
# btrfs checksum, not just the file's visible data).
# ─────────────────────────────────────────────────────────────────────────────

# btrfs stores one CRC32C per 4096-byte data sector as a little-endian uint32.
CSUM_BLOCK_SIZE = 4096
CSUM_SIZE_CRC32C = 4  # bytes per checksum


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


def walk_leaves(fp, root_logical, tree):
    """BFS-walk a btrfs B-tree; yield (leaf_node, leaf_phys) for each leaf.

    Identical to `walk_btree` but keeps the parent leaf node around so we
    can locate item data relative to the leaf's on-disk position.
    """
    queue: deque = deque((d, p) for d, p, _ in tree.offsets(root_logical))
    while queue:
        devid, phys = queue.popleft()
        node = parse_at(fp, phys, TreeNode)
        if node.header.level == 0:
            yield node, phys
        else:
            for ptr in node.items:
                for d, p, _ in tree.offsets(ptr.blockptr):
                    queue.append((d, p))


def find_csum_root(superblock, tree, devid_fp_map) -> int:
    """Locate the root of the dedicated CRC32C checksum tree (objectid 7).

    The CSUM_TREE (a.k.a. CSUM_TREE_ROOT) is registered as a ROOT_ITEM in the
    root tree, keyed by ObjectId.CsumTree (7).
    """
    for item in walk_btree(superblock.root, tree, devid_fp_map):
        if (item.key.objectid == ObjectId.CsumTree
                and item.key.ty == KeyType.RootItem):
            return item.data.bytenr
    sys.exit('ERROR: CSUM_TREE ROOT_ITEM not found in root tree')


def find_csum_covering(fp, tree, csum_root_logical, target_logical,
                      csum_size: int = CSUM_SIZE_CRC32C):
    """Return (leaf_node, csum_leaf_item) for the first EXTENT_CSUM whose
    logical range covers `target_logical`. Returns None if not found.

    An EXTENT_CSUM item's key.offset is the logical byte address where its
    covered data range begins, but its on-disk `size` is the length of the
    *checksum array* (num_sectors * csum_size bytes) — NOT the length of the
    data it covers. The covered data length is
    `(size / csum_size) * CSUM_BLOCK_SIZE`.
    """
    for leaf, _ in walk_leaves(fp, csum_root_logical, tree):
        for item in leaf.items:
            if item.key.ty != KeyType.ExtentCsum:
                continue
            covered_len = (item.size // csum_size) * CSUM_BLOCK_SIZE
            if item.key.offset <= target_logical < item.key.offset + covered_len:
                return leaf, item
    return None


def read_stored_csum(fp, leaf, item, target_logical,
                     csum_size: int = CSUM_SIZE_CRC32C) -> bytes:
    """Read the raw csum bytes for the data sector containing `target_logical`.

    btrfs layout: an EXTENT_CSUM leaf item holds a contiguous array of
    checksums (one per sector). The array begins on disk at byte offset
    `leaf_phys + header_size + item.offset`. For a given logical address:
        block_index = (target_logical - item.key.offset) / sector_size
        csum_byte   = array_start + block_index * csum_size
    """
    csum_byte = (leaf.phys_start
                 + Header.sizeof()
                 + item.offset
                 + ((target_logical - item.key.offset) // CSUM_BLOCK_SIZE) * csum_size)
    fp.seek(csum_byte)
    return fp.read(csum_size)


def compute_block_csum(dev: str, phys_offset: int,
                       block_size: int = CSUM_BLOCK_SIZE) -> int:
    """Read `block_size` bytes from `dev` at `phys_offset` and return crc32c."""
    with open(dev, 'rb') as fp:
        fp.seek(phys_offset)
        block = fp.read(block_size)
    if len(block) < block_size:
        sys.exit(f'ERROR: short read from {dev} at {phys_offset} '
                 f'(wanted {block_size}, got {len(block)})')
    return crc32c(block)


# ─────────────────────────────────────────────────────────────────────────────
# Device resolution (replaces /proc/nmdstat awk parsing)
# ─────────────────────────────────────────────────────────────────────────────

def findmnt_source(mount_point: str) -> str:
    """Return the block device path a mount point is mounted from (like `findmnt -n -o SOURCE`)."""
    with open('/proc/mounts') as f:
        for line in f:
            fields = line.split()
            if len(fields) >= 2 and fields[1] == mount_point:
                return fields[0]
    sys.exit(f'ERROR: could not find device for mount point {mount_point}')


def resolve_devid_to_device(devid: int, mount_dev: str) -> str:
    """Translate btrfs devid → underlying block device path via /proc/nmdstat.

    The NonRAID kernel module exposes /proc/nmdstat with lines:
        rdevName.<slot>=<name>
    where <slot> corresponds to the btrfs devid for data disks, and <name> is
    a bare device name (e.g. "loop2p1"). Normalize it to a full /dev/ path.

    If /proc/nmdstat is not present (plain btrfs, not a NonRAID array), we
    fall back to using the mounted device directly.
    """
    if os.path.exists('/proc/nmdstat'):
        pat = re.compile(rf'^rdevName\.{devid}=(.+)$')
        with open('/proc/nmdstat') as f:
            for line in f:
                m = pat.match(line.strip())
                if m:
                    name = m.group(1)
                    if not name.startswith('/'):
                        name = f'/dev/{name}'
                    if not os.path.exists(name) or not _is_block_device(name):
                        sys.exit(f'ERROR: {name} is not a block device')
                    return name
        sys.exit(f'ERROR: rdevName.{devid} not found in /proc/nmdstat')
    return mount_dev


def _is_block_device(path: str) -> bool:
    try:
        st = os.stat(path)
        return (st.st_mode & 0o170000) == 0o060000  # S_ISBLK
    except OSError:
        return False


# ─────────────────────────────────────────────────────────────────────────────
# Cache management (replaces `echo 3 > /proc/sys/vm/drop_caches`)
# ─────────────────────────────────────────────────────────────────────────────

def drop_caches() -> None:
    with open('/proc/sys/vm/drop_caches', 'w') as f:
        f.write('3')


# ─────────────────────────────────────────────────────────────────────────────
# File test data (replaces the dd | tr pipeline)
# ─────────────────────────────────────────────────────────────────────────────

def create_test_file(path: str, size_mb: int = 300) -> None:
    """Create a file filled with 0xFF bytes (visible against any corruption)."""
    print(f'Creating {size_mb} MB test file (all 0xFF) at {path}')
    chunk = b'\xff' * (1024 * 1024)  # 1 MB
    # Remove any old test file so we always start clean.
    try:
        os.remove(path)
    except FileNotFoundError:
        pass
    with open(path, 'wb') as f:
        for _ in range(size_mb):
            f.write(chunk)
    f.close()
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
    parser.add_argument('--size-mb', type=int, default=300,
                        help='size of the test file to create in MB (default: %(default)s)')
    parser.add_argument('--byte-value', default='0x00',
                        help='byte to write at the corruption point (default: %(default)s)')
    parser.add_argument('--no-create', action='store_true',
                        help='do not create the test file; assume it already exists')
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
    if not args.no_create:
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
        print(f'(skipping file creation; using existing {target})')
    os.sync()
    # btrfs keeps new checksum tree writes in its journal until an explicit
    # filesystem sync — without this, the CSUM_TREE walk below would find no
    # EXTENT_CSUM item covering the freshly-written extent.
    if not args.dry_run:
        btrfs_sync(args.mount_point)

    # ─────────────────────────────────────────────────────────────────────────
    # Step 2 — Find the file's physical location via btrfs on-disk structures
    # ─────────────────────────────────────────────────────────────────────────
    # btrfs uses its own virtual (logical) address space; a separate CHUNK_TREE
    # translates those to physical disk offsets. We walk both with btrfs_recon
    # (no DB, no btrfs-progs) to find:
    #   - the file's first EXTENT_DATA record in the FS_TREE (logical extent)
    #   - the chunk mapping it to a physical byte offset on a real device
    print()
    print('=== Step 2a: Opening btrfs filesystem and walking trees ===')
    with open(mount_dev, 'rb') as fp:
        superblock = parse_superblock(fp)
        devid_fp_map = {superblock.dev_item.devid: fp}
        tree = build_chunk_tree(superblock, devid_fp_map)

        inode, extent_item = find_file_extent(
            fp, superblock, tree, devid_fp_map, args.mount_point, target,
        )

        if extent_item.data.type != extent_item.data.type.REGULAR:
            sys.exit(f'ERROR: first extent is not REGULAR (type={extent_item.data.type})')
        virt_addr = extent_item.data.ref.disk_bytenr
        extent_size = extent_item.data.ref.disk_num_bytes
        print(f'Found EXTENT_DATA for inode {inode}')
        print(f'  disk_bytenr (virtual addr) = {virt_addr} (0x{virt_addr:x})')
        print(f'  disk_num_bytes             = {extent_size}')

        # ───────────────────────────────────────────────────────────────────
        # Step 2b — Map virtual address → physical offset on device
        # ───────────────────────────────────────────────────────────────────
        print()
        print('=== Step 2b: Mapping virtual address to physical offset (CHUNK_TREE) ===')
        devid, real_phys_offset, chunk_virt_start, chunk_phys_base = \
            resolve_logical_to_physical(tree, virt_addr)
        print(f'Chunk virtual start  : {chunk_virt_start}')
        print(f'Chunk physical base  : {chunk_phys_base}')
        print(f'File physical offset : {real_phys_offset}')
        print(f'btrfs devid          : {devid}')

        # ───────────────────────────────────────────────────────────────────
        # Step 2c — Translate btrfs devid → actual block device path
        # ───────────────────────────────────────────────────────────────────
        print()
        print(f'=== Step 2c: Resolving btrfs devid {devid} \u2192 block device path ===')
        underlying_dev = resolve_devid_to_device(devid, mount_dev)
        print(f'devid {devid} \u2192 {underlying_dev}')

        # ───────────────────────────────────────────────────────────────────
        # Step 2d — Locate the stored checksum for the file's first data block
        # ───────────────────────────────────────────────────────────────────
        # This goes beyond the original shell script — btrfs_manipulate.sh does
        # NOT compute or read checksums. We walk the dedicated CSUM_TREE
        # (objectid 7) and find the EXTENT_CSUM leaf item whose logical range
        # covers the file's first data sector, then read its raw CRC32C bytes
        # so we can prove (in Steps 3a and 5a) that the silent corruption
        # actually invalidates the on-disk btrfs checksum.
        #
        # The stored checksum is read from the same array device we're already
        # parsing trees from; the *computed* checksum comes from the underlying
        # partition (we corrupt THAT device in Step 4, and the array device
        # would otherwise just hand back the cached, uncorrupted bytes).
        csum_size = CSUM_SIZE_CRC32C if superblock.csum_type == 0 else None
        if superblock.csum_type != 0:
            print()
            print(f'=== Step 2d: skipping checksum verification '
                  f'(unsupported csum_type={superblock.csum_type}; only CRC32C=0 supported) ===')
            stored_csum_bytes = None
        else:
            print()
            print(f'=== Step 2d: Locating stored checksum (CSUM_TREE walk) ===')
            csum_root = find_csum_root(superblock, tree, devid_fp_map)
            print(f'CSUM_TREE root: 0x{csum_root:x}')
            csum_match = find_csum_covering(fp, tree, csum_root, virt_addr,
                                            csum_size=csum_size)
            if csum_match is None:
                sys.exit('ERROR: no EXTENT_CSUM item covers the file extent '
                         '(was a btrfs filesystem sync forced after creation?)')
            csum_leaf, csum_item = csum_match
            stored_csum_bytes = read_stored_csum(
                fp, csum_leaf, csum_item, virt_addr, csum_size=csum_size,
            )
            stored_csum_int = struct.unpack('<I', stored_csum_bytes)[0]
            print(f'CSUM leaf logical  : 0x{csum_leaf.header.bytenr:x}')
            print(f'CSUM item key      : offset=0x{csum_item.key.offset:x} '
                  f'size={csum_item.size}')
            print(f'Stored csum bytes  : {stored_csum_bytes.hex()} '
                  f'(uint32 LE = 0x{stored_csum_int:08x})')

        # ───────────────────────────────────────────────────────────────────
        # Step 3 — Sanity check: confirm both devices show 0xFF at the target
        # ───────────────────────────────────────────────────────────────────
        print()
        print(f'=== Step 3: Sanity check \u2014 both devices should show 0xFF at offset {real_phys_offset} ===')
        dump_window(mount_dev, 'Array device', real_phys_offset)
        dump_window(underlying_dev, 'Underlying partition', real_phys_offset)

        # ───────────────────────────────────────────────────────────────────
        # Step 3a — Pre-corruption: confirm stored csum matches computed csum
        # ───────────────────────────────────────────────────────────────────
        # Independently compute the CRC32C of the file's first 4K data block,
        # reading from the UNDERLYING partition (the device we will corrupt).
        # For healthy data, this MUST equal the value stored in the CSUM_TREE.
        if stored_csum_bytes is not None:
            print()
            print('=== Step 3a: Pre-corruption checksum verification ===')
            computed_pre = compute_block_csum(underlying_dev, real_phys_offset)
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
        # Flip the second byte of the file's data (real_phys_offset + 1) from
        # 0xFF to 0x00 by writing *directly to the underlying partition*,
        # bypassing btrfs (so checksums go stale) and the NonRAID array
        # (so parity disks are NOT updated and cannot heal it).
        #
        # Target byte +1 (not +0) because the very first byte of a btrfs data
        # block can coincide with internal block header bytes depending on
        # alignment; byte +1 is reliably inside the raw file payload.
        byte_value = int(args.byte_value, 0)
        byte_offset = real_phys_offset + 1
        print()
        print(f'=== Step 4: Writing 0x{byte_value:02x} to {underlying_dev} at byte offset {byte_offset} ===')
        if args.dry_run:
            print('(dry-run: not actually writing)')
        else:
            # Open the raw device R+W, seek to the target byte, write one byte,
            # and fsync+close immediately. The underlying OS block device sees
            # the write; the overlying btrfs + NonRAID layers do not (so the
            # btrfs checksum goes stale and the parity disks are not updated).
            with open(underlying_dev, 'r+b') as raw_fp:
                raw_fp.seek(byte_offset)
                raw_fp.write(bytes([byte_value]))
                raw_fp.flush()
                os.fsync(raw_fp.fileno())
            print(f'Flipped byte on {underlying_dev}')
            print(f'Array device {mount_dev} was NOT touched \u2014 parity remains stale')

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
            dump_window(mount_dev, 'Array device (post-corruption)', real_phys_offset)
            dump_window(underlying_dev, 'Underlying partition (post-corruption)', real_phys_offset)

        # ───────────────────────────────────────────────────────────────────
        # Step 5a — Post-corruption: confirm stored csum NO LONGER matches
        # ───────────────────────────────────────────────────────────────────
        if stored_csum_bytes is not None:
            print()
            print('=== Step 5a: Post-corruption checksum verification ===')
            computed_post = compute_block_csum(underlying_dev, real_phys_offset)
            print(f'  stored csum   : 0x{stored_csum_int:08x}')
            print(f'  computed csum : 0x{computed_post:08x}')
            if stored_csum_int != computed_post:
                print('  ✓ MISMATCH — corruption successfully invalidated the on-disk checksum')
            else:
                sys.exit('  ✗ ERROR — checksum still matches! Corruption failed to change the data block.')

    print()
    print('=== Summary ===')
    if args.dry_run:
        print(f'(dry-run) Would corrupt {underlying_dev} at offset {byte_offset}')
    else:
        print(f'Corrupted byte  : {underlying_dev} at offset {byte_offset}')
        print(f'Parity state    : stale \u2014 cannot heal this')
        print(f'btrfs           : will report checksum error on next read of {target}')


if __name__ == '__main__':
    main()