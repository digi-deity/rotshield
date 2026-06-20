"""Minimal, inspection-only btrfs parsing helpers.

No DB, no async, no progress bars — just enough to:
  * parse a superblock
  * walk the chunk tree (building a ChunkTreeCache for logical→physical)
  * walk any btrfs B-tree (root, fs, checksum, …) yielding leaf items
  * find EXTENT_DATA for a given inode and file offset
  * look up the stored CRC32C checksum for a logical data sector
"""
from __future__ import annotations

import struct
from collections import deque
from typing import BinaryIO, Iterator, Type, Union

import construct as cs

from btrfs_recon.constants import BTRFS_SECTOR_SIZE
from btrfs_recon.structure import (
    Header,
    KeyType,
    ObjectId,
    Struct,
    Superblock,
    TreeNode,
    LeafItem,
)
from btrfs_recon.types import DevId, PhysicalAddress
from btrfs_recon.util.chunk_cache import ChunkTreeCache

# Default offset of the primary superblock (64 KiB into the device).
SUPERBLOCK_OFFSET = 0x10_000


def parse_at(
    fp: BinaryIO,
    pos: int,
    type_: Union[cs.Construct, Type[Struct]],
    **contextkw,
):
    """Parse `type_` at byte offset `pos` from the open binary stream `fp`."""
    if isinstance(type_, type) and issubclass(type_, Struct):
        type_ = type_.as_struct()
    return cs.Pointer(pos, type_).parse_stream(fp, **contextkw)


def parse_superblock(fp: BinaryIO, pos: int = SUPERBLOCK_OFFSET) -> Superblock:
    """Parse the primary superblock from a device handle."""
    return parse_at(fp, pos, Superblock)


def build_chunk_tree(
    superblock: Superblock,
    devid_fp_map: dict[int, BinaryIO],
) -> ChunkTreeCache:
    """Walk the chunk tree and build a logical→physical mapping cache.

    `devid_fp_map` maps btrfs devid → open binary file handle for that device.
    For a single-device filesystem (the common case here), it has one entry.
    """
    tree = ChunkTreeCache()

    # The superblock carries a bootstrap of system chunks (the ones needed to
    # even start walking the chunk tree itself). Seed the cache with them.
    for sys_chunk in superblock.sys_chunks:
        tree.insert(
            sys_chunk.key.offset,
            sys_chunk.key.offset + sys_chunk.chunk.length,
            sys_chunk.chunk.stripes,
            stripe_len=sys_chunk.chunk.stripe_len,
            flags=sys_chunk.chunk.ty,
        )

    # Walk the chunk tree starting from its root (a logical address that we
    # must translate to a physical offset using the bootstrap above).
    chunk_root_phys = list(tree.offsets(superblock.chunk_root))
    queue: deque[tuple[int, int]] = deque(
        (devid, phys) for devid, phys, _n in chunk_root_phys
    )

    while queue:
        devid, physical = queue.popleft()
        fp = devid_fp_map[devid]
        node = parse_at(fp, physical, TreeNode)

        if node.header.level == 0:
            # Leaf: collect every CHUNK_ITEM into the cache.
            for item in node.items:
                if item.key.ty != KeyType.ChunkItem:
                    continue
                tree.insert(
                    item.key.offset,
                    item.key.offset + item.data.length,
                    item.data.stripes,
                    stripe_len=item.data.stripe_len,
                    flags=item.data.ty,
                )
        else:
            # Internal node: enqueue each child pointer (a logical block
            # address, translated via the cache built so far).
            for ptr in node.items:
                for devid, phys, _n in tree.offsets(ptr.blockptr):
                    queue.append((devid, phys))

    return tree


def walk_btree(
    root_logical: int,
    tree: ChunkTreeCache,
    devid_fp_map: dict[int, BinaryIO],
) -> Iterator[LeafItem]:
    """Walk a btrfs B-tree from `root_logical` and yield every leaf item.

    Works for any tree whose nodes are standard btrfs TreeNodes (root tree,
    fs tree, checksum tree, etc.).
    """
    queue: deque[tuple[int, int]] = deque(
        (devid, phys) for devid, phys, _n in tree.offsets(root_logical)
    )
    while queue:
        devid, physical = queue.popleft()
        fp = devid_fp_map[devid]
        node = parse_at(fp, physical, TreeNode)

        if node.header.level == 0:
            yield from node.items
        else:
            for ptr in node.items:
                for devid, phys, _n in tree.offsets(ptr.blockptr):
                    queue.append((devid, phys))


def open_fs(image_path) -> tuple[BinaryIO, Superblock, ChunkTreeCache]:
    """Convenience: open a single-device btrfs image and return (fp, superblock, tree)."""
    fp = open(image_path, 'rb')
    superblock = parse_superblock(fp)
    dev_item = superblock.dev_item
    devid_fp_map = {dev_item.devid: fp}
    tree = build_chunk_tree(superblock, devid_fp_map)
    return fp, superblock, tree


def walk_leaves(
    fp: BinaryIO,
    root_logical: int,
    tree: ChunkTreeCache,
) -> Iterator[tuple[TreeNode, int]]:
    """BFS-walk a btrfs B-tree and yield (leaf_node, leaf_phys) for every leaf.

    Unlike `walk_btree`, this keeps the full node so callers can locate item
    data relative to the leaf's on-disk position (e.g. for checksum lookups).
    Only a single file handle is supported (single-device filesystem).
    """
    queue: deque[tuple[int, int]] = deque(
        (devid, phys) for devid, phys, _n in tree.offsets(root_logical)
    )
    while queue:
        devid, phys = queue.popleft()
        node = parse_at(fp, phys, TreeNode)
        if node.header.level == 0:
            yield node, phys
        else:
            for ptr in node.items:
                for d, p, _n in tree.offsets(ptr.blockptr):
                    queue.append((d, p))


def find_tree_root(
    superblock: Superblock,
    tree: ChunkTreeCache,
    devid_fp_map: dict[int, BinaryIO],
    objectid: ObjectId,
) -> int:
    """Return the root node bytenr of a tree identified by its root-tree objectid.

    Walks the root tree (at `superblock.root`) and locates the ROOT_ITEM whose
    objectid matches the requested tree (e.g. ``ObjectId.FsTree``,
    ``ObjectId.CsumTree``).  Raises ``KeyError`` if not found.
    """
    for item in walk_btree(superblock.root, tree, devid_fp_map):
        if item.key.objectid == objectid and item.key.ty == KeyType.RootItem:
            return item.data.bytenr
    raise KeyError(f'ROOT_ITEM for objectid {objectid!r} not found in root tree')


def find_extent_data(
    tree: ChunkTreeCache,
    devid_fp_map: dict[int, BinaryIO],
    fs_root: int,
    inode: int,
    file_offset: int,
) -> LeafItem:
    """Find the EXTENT_DATA leaf item covering ``file_offset`` for ``inode``.

    Walks the FS_TREE starting at ``fs_root`` (a logical address, e.g. from
    ``find_tree_root(..., ObjectId.FsTree)``).  Only REGULAR (non-inline,
    non-prealloc) extents are considered.

    Raises ``KeyError`` if no matching extent is found.
    """
    for item in walk_btree(fs_root, tree, devid_fp_map):
        if item.key.objectid != inode or item.key.ty != KeyType.ExtentData:
            continue
        if item.data.type != item.data.type.REGULAR:
            continue
        ext_start = item.key.offset
        ext_len = item.data.ref.disk_num_bytes
        if ext_start <= file_offset < ext_start + ext_len:
            return item
    raise KeyError(
        f'No REGULAR EXTENT_DATA found for inode {inode} '
        f'covering file offset {file_offset}'
    )


def lookup_csum(
    fp: BinaryIO,
    tree: ChunkTreeCache,
    csum_root: int,
    sector_logical: int,
    csum_size: int = 4,
) -> int:
    """Return the stored CRC32C (as a little-endian uint32) for a data sector.

    ``sector_logical`` must be sector-aligned (a multiple of
    ``BTRFS_SECTOR_SIZE``).  ``csum_root`` is the logical address of the
    CSUM_TREE root node, e.g. from
    ``find_tree_root(..., ObjectId.CsumTree)``.

    Raises ``KeyError`` if no EXTENT_CSUM item covers the requested sector.
    """
    for leaf, _ in walk_leaves(fp, csum_root, tree):
        for item in leaf.items:
            if item.key.ty != KeyType.ExtentCsum:
                continue
            num_sectors = item.size // csum_size
            covered_end = item.key.offset + num_sectors * BTRFS_SECTOR_SIZE
            if item.key.offset <= sector_logical < covered_end:
                sector_index = (sector_logical - item.key.offset) // BTRFS_SECTOR_SIZE
                data_pos = (
                    leaf.phys_start
                    + Header.sizeof()
                    + item.offset
                    + sector_index * csum_size
                )
                fp.seek(data_pos)
                raw = fp.read(csum_size)
                return struct.unpack('<I', raw)[0]
    raise KeyError(
        f'No EXTENT_CSUM item covers sector logical=0x{sector_logical:x}'
    )
