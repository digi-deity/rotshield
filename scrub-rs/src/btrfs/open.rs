//! Shared btrfs filesystem opening — the single construction site that
//! every caller in the crate goes through ([`BtrfsScrub::open`],
//! `bin/craft_corrupt`, and the `--resolve` debug subcommand in `main.rs`).
//!
//! Owning this in one place kills the three-way duplication of the
//! `superblock → chunk-map bootstrap → chunk-tree walk → root-tree walk`
//! pipeline that previously drifted across those call sites.  The callers
//! inspect `BtrfsContext`'s public fields directly — they need
//! `reader`/`chunk_map`/`superblock`/`fs_root`/`csum_root` to do further
//! format-specific walks, so we don't wrap them behind getters that would
//! just re-borrow on every access.

use std::fs::File;
use std::io;

use super::chunk::{ChunkItem, ChunkMap, ChunkRecord};
use super::key;
use super::reader::FsReader;
use super::root::RootItem;
use super::superblock::Superblock;
use super::tree::walk_leaves;

/// Roots of the well-known btrfs trees, located by walking the root tree.
///
/// `fs_root` and `csum_root` are the only two nearly every caller needs.
/// [`crate::btrfs::open`] errors early if either is missing — the
/// scrub/corrupt/resolve paths all require both, so enforcing that here
/// keeps the contract tight.
#[derive(Debug, Clone, Copy)]
pub struct TreeRoots {
    pub fs_root: u64,
    pub csum_root: u64,
}

/// A btrfs filesystem opened for reading, after the chunk map is populated
/// and the FS/CSUM tree roots are located.
///
/// This is what [`crate::btrfs::open`] returns — the shared head of every
/// downstream walk.  Callers borrow `reader` mutably (the reads modify the
/// seek position) and `chunk_map` immutably.
pub struct BtrfsContext {
    pub reader: FsReader,
    pub chunk_map: ChunkMap,
    pub superblock: Superblock,
    pub roots: TreeRoots,
}

/// Open `dev` (a block device or image file) at `base_offset`, populate the
/// chunk map, and locate the FS_TREE / CSUM_TREE roots — the common
/// preamble every caller needs before any format-specific work.
///
/// `base_offset` is 0 for a bare btrfs image or an array partition
/// (`/dev/nmd1p1`); the per-disk `rdevOffset` (e.g. 64 sectors = 32 KiB)
/// for a whole-disk raw rdev like `/dev/loop2`.
///
/// Hoisted here from the three call sites that previously each re-implemented
/// this pipeline (`BtrfsScrub::open`, `bin/craft_corrupt::open_fs`,
/// `main::resolve_cmd`).  All three now delegate here.
pub fn open(dev: &str, base_offset: u64) -> io::Result<BtrfsContext> {
    let mut fp = File::open(dev)?;
    let superblock = Superblock::read(&mut fp, base_offset).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("error reading btrfs superblock: {e}"),
        )
    })?;

    // Bootstrap the chunk map from the sys-chunk array, then walk the chunk
    // tree itself to populate the rest.  Two-pass because the chunk tree's
    // own logical addresses must resolve while we're still walking it.
    let sys_chunks = super::chunk::parse_sys_chunks(&superblock.sys_chunks);
    let mut chunk_map = ChunkMap::default();
    for rec in &sys_chunks {
        chunk_map.insert(rec);
    }
    let mut reader = FsReader::new(
        File::open(dev)?,
        superblock.node_size as usize,
        base_offset,
    );

    let mut chunk_records: Vec<ChunkRecord> = Vec::new();
    walk_leaves(&mut reader, &chunk_map, superblock.chunk_root, |_r, leaf, _logical| {
        for i in 0..leaf.slots.len() {
            let slot = leaf.slots[i];
            if slot.key.ty == key::key_type::CHUNK_ITEM {
                let chunk = ChunkItem::parse(leaf.item_data(i));
                chunk_records.push(ChunkRecord {
                    logical: slot.key.offset,
                    chunk,
                });
            }
        }
        Ok(())
    })?;
    for rec in &chunk_records {
        chunk_map.insert(rec);
    }

    // Walk the root tree to locate FS_TREE and CSUM_TREE.
    let mut fs_root: Option<u64> = None;
    let mut csum_root: Option<u64> = None;
    walk_leaves(&mut reader, &chunk_map, superblock.root, |_r, leaf, _logical| {
        for i in 0..leaf.slots.len() {
            let slot = leaf.slots[i];
            if slot.key.ty == key::key_type::ROOT_ITEM {
                let ri = RootItem::parse(leaf.item_data(i));
                match slot.key.objectid {
                    key::objectid::FS_TREE => fs_root = Some(ri.bytenr),
                    key::objectid::CSUM_TREE => csum_root = Some(ri.bytenr),
                    _ => {}
                }
            }
        }
        Ok(())
    })?;
    let roots = TreeRoots {
        fs_root: fs_root.ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "FS_TREE root not found in btrfs root tree")
        })?,
        csum_root: csum_root.ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "CSUM_TREE root not found in btrfs root tree")
        })?,
    };

    Ok(BtrfsContext {
        reader,
        chunk_map,
        superblock,
        roots,
    })
}