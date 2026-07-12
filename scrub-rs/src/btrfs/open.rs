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
use super::csum_strategy::CsumStrategy;
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
    /// Logical address of the DEV_TREE root.  Present on every btrfs
    /// filesystem (objectid 4); the physical-order scrub
    /// ([`crate::btrfs::scrub::scrub_dev_tree`]) walks it to enumerate
    /// dev-extents in ascending physical order.  `None` only if the root
    /// tree somehow lacks a DEV_TREE ROOT_ITEM — which should never happen
    /// on a real filesystem, so callers may `expect` it.
    pub dev_tree_root: Option<u64>,
    /// Logical address of the EXTENT_TREE root (objectid 2).  Used by the
    /// data-scrub mismatch filter ([`crate::btrfs::extent::extent_covers`])
    /// to confirm a mismatching sector is still owned by a live data extent
    /// (vs. an orphaned/freed csum entry left behind by churn) before it is
    /// reported as corruption.  `None` only if the root tree lacks an
    /// EXTENT_TREE ROOT_ITEM — which should never happen on a real
    /// filesystem, so callers may `expect` it.
    pub extent_tree_root: Option<u64>,
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
    /// The checksum strategy (algorithm + sector size) derived from the
    /// superblock.  Threaded into [`FsReader`] so every metadata node/leaf
    /// header is verified on read, and into the scrub so data csums use the
    /// right algorithm.  Exposed here so callers (e.g. `BtrfsScrub`) don't
    /// rebuild it themselves.
    pub strategy: CsumStrategy,
    /// Number of metadata nodes whose *all* mirror copies failed
    /// header-checksum verification during the chunk-tree and root-tree
    /// walks (i.e. DUP/RAID1 metadata with no good copy).  A single corrupt
    /// copy that has a good sibling is transparently recovered and not
    /// counted here.  Surfaced so the scrub can report it as a
    /// `metadata_header_errors` stat rather than letting it pass silently.
    pub metadata_header_errors: u64,
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

    // The checksum strategy (algorithm + sector size) is derived from the
    // superblock and threaded into the reader so every metadata node/leaf
    // header is verified on read.  Fail loudly on an unsupported csum type
    // rather than silently producing false mismatches downstream.
    let strategy = CsumStrategy::from_superblock(&superblock)?;

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
        Some(strategy),
    )
    // Each NonRAID slot is a single-device filesystem, so the reader's
    // backing store is exactly one device.  Register its devid so the
    // physical-order scrub's `read_physical` calls are guarded against
    // ever targeting the wrong disk.
    .with_devid(superblock.devid)
    // Register the filesystem UUID so `read_node` can reject a metadata
    // block whose `fsid` does not match (a misdirected read or a block
    // from a different filesystem).
    .with_fsid(superblock.fsid);

    let mut chunk_records: Vec<ChunkRecord> = Vec::new();
    let mut metadata_header_errors: u64 = 0;
    walk_leaves(
        &mut reader,
        &chunk_map,
        superblock.chunk_root,
        |_r, leaf, _logical| {
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
        },
        |_logical| {
            // A mirrored (DUP) chunk-tree node with no good copy — count it.
            metadata_header_errors += 1;
        },
    )?;
    for rec in &chunk_records {
        chunk_map.insert(rec);
    }

    // Walk the root tree to locate FS_TREE and CSUM_TREE.
    let mut fs_root: Option<u64> = None;
    let mut csum_root: Option<u64> = None;
    let mut dev_tree_root: Option<u64> = None;
    let mut extent_tree_root: Option<u64> = None;
    walk_leaves(
        &mut reader,
        &chunk_map,
        superblock.root,
        |_r, leaf, _logical| {
            for i in 0..leaf.slots.len() {
                let slot = leaf.slots[i];
                if slot.key.ty == key::key_type::ROOT_ITEM {
                    let ri = RootItem::parse(leaf.item_data(i));
                    match slot.key.objectid {
                        key::objectid::FS_TREE => fs_root = Some(ri.bytenr),
                        key::objectid::CSUM_TREE => csum_root = Some(ri.bytenr),
                        key::objectid::DEV_TREE => dev_tree_root = Some(ri.bytenr),
                        key::objectid::EXTENT_TREE => extent_tree_root = Some(ri.bytenr),
                        _ => {}
                    }
                }
            }
            Ok(())
        },
        |_logical| {
            // A mirrored (DUP) root-tree node with no good copy — count it.
            metadata_header_errors += 1;
        },
    )?;
    let roots = TreeRoots {
        fs_root: fs_root.ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "FS_TREE root not found in btrfs root tree")
        })?,
        csum_root: csum_root.ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "CSUM_TREE root not found in btrfs root tree")
        })?,
        dev_tree_root,
        extent_tree_root,
    };

    Ok(BtrfsContext {
        reader,
        chunk_map,
        superblock,
        roots,
        strategy,
        metadata_header_errors,
    })
}

/// Re-read the **current** data-tree roots (EXTENT_TREE *and* CSUM_TREE)
/// from the live on-disk superblock, rather than the ones captured at
/// `open()` time.
///
/// Why this matters: the scrub walks a *frozen snapshot* of the metadata it
/// loaded at `open()` (csum map, chunk map, tree roots). On a live mounted
/// filesystem that snapshot goes stale the moment a new transaction commits
/// — the root tree, and with it the EXTENT_TREE and CSUM_TREE root bytenrs,
/// move to newer locations. A reconfirmation that consulted the *open-time*
/// trees could therefore mis-classify a freshly-freed or rewritten extent,
/// producing a false positive (or, worse, reading a freed/reused block).
///
/// So the mismatch filter calls this *only when a csum mismatch is found*,
/// to obtain the absolute latest EXTENT_TREE and CSUM_TREE roots and
/// reconfirm against the most recent committed trees. This is deliberately
/// NOT done up front: it would force the whole scrub onto the live trees
/// and defeat the point of the frozen snapshot (stable, reproducible walk
/// order, no mid-scrub tree mutation). The main scrub path keeps using the
/// in-memory snapshot; only the rare mismatch takes the cost of a few extra
/// tree descents (superblock read + root-tree walk + two point lookups) to
/// get current truth.
///
/// Both trees are returned because a faithful reconfirmation needs both:
/// * the **EXTENT_TREE** answers "is this logical sector still owned by a
///   live data extent?" (liveness / stale / `nodatasum`);
/// * the **CSUM_TREE** answers "what csum does the *current* filesystem
///   expect here?" — if churn rewrote the extent, the live csum differs
///   from what we read, which is benign churn rather than corruption.
///
/// Returns `None` if the live superblock or root tree cannot be read — the
/// caller should then be conservative and treat the sector as a real
/// mismatch (never hide corruption just because we couldn't re-verify).
pub fn live_data_tree_roots(
    reader: &mut FsReader,
    chunk_map: &ChunkMap,
    base_offset: u64,
) -> Option<(u64, u64)> {
    // The reader owns the File handle; dup it and read the primary superblock.
    let mut fp = reader.reopen().ok()?;
    let sb = Superblock::read(&mut fp, base_offset).ok()?;
    let root_tree = sb.root;

    // Walk the *current* root tree to find the EXTENT_TREE and CSUM_TREE
    // ROOT_ITEMs.
    let mut extent_root: Option<u64> = None;
    let mut csum_root: Option<u64> = None;
    walk_leaves(
        reader,
        chunk_map,
        root_tree,
        |_r, leaf, _logical| {
            for i in 0..leaf.slots.len() {
                let slot = leaf.slots[i];
                if slot.key.ty != key::key_type::ROOT_ITEM {
                    continue;
                }
                let ri = RootItem::parse(leaf.item_data(i));
                match slot.key.objectid {
                    key::objectid::EXTENT_TREE => extent_root = Some(ri.bytenr),
                    key::objectid::CSUM_TREE => csum_root = Some(ri.bytenr),
                    _ => {}
                }
            }
            Ok(())
        },
        |_logical| {
            // A root-tree node with no good copy — we can't trust the walk;
            // treat as unverifiable and let the caller be conservative.
        },
    )
    .ok()?;
    Some((extent_root?, csum_root?))
}