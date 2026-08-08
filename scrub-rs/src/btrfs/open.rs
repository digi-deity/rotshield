//! Open a single-device btrfs filesystem: superblock (with backup fallback),
//! chunk map, well-known tree roots, and open-time metadata-error accounting.

use std::fs::File;
use std::io;
use std::sync::atomic::Ordering;

use crate::status::StatusCounters;

use super::chunk::{ChunkItem, ChunkMap, ChunkRecord};
use super::csum_strategy::CsumStrategy;
use super::key;
use super::reader::FsReader;
use super::root::RootItem;
use super::superblock::Superblock;
use super::tree::walk_leaves;

/// Logical addresses of the well-known tree roots the scrub needs.
#[derive(Debug, Clone, Copy)]
pub struct TreeRoots {
    pub fs_root: u64,
    pub csum_root: u64,

    /// Present only when the root-tree walk found the tree's ROOT_ITEM.
    pub dev_tree_root: Option<u64>,

    /// Present only when the root-tree walk found the tree's ROOT_ITEM.
    pub extent_tree_root: Option<u64>,
}

/// Everything a scrub needs to read the filesystem.
pub struct BtrfsContext {
    pub reader: FsReader,
    pub chunk_map: ChunkMap,
    pub superblock: Superblock,
    pub roots: TreeRoots,

    /// Checksum algorithm selected by the superblock.
    pub strategy: CsumStrategy,

    /// Metadata nodes whose every mirror copy failed header verification during open.
    pub metadata_header_errors: u64,

    /// Mirror copies that disagreed during open (a good copy was recovered).
    pub metadata_mirror_mismatches: u64,

    /// Metadata nodes that failed with a read (EIO) error during open.
    pub metadata_read_errors: u64,
}

/// Open `dev` and assemble the full read context: superblock, chunk map,
/// and tree roots, counting any metadata failures found along the way.
pub fn open(dev: &str, base_offset: u64) -> io::Result<BtrfsContext> {
    let mut fp = File::open(dev)?;
    let superblock = Superblock::read(&mut fp, base_offset).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("error reading btrfs superblock: {e}"),
        )
    })?;

    // Single-device only: each NonRAID disk hosts its own filesystem; a
    // member of a multi-device pool would be misread via its striped geometry.
    if superblock.num_devices != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{}: btrfs superblock reports num_devices = {} — this tool only supports \
                 SINGLE-device filesystems (each NonRAID disk hosts its own independent \
                 btrfs). A member disk of a multi-device btrfs pool would be misread \
                 (striped chunk geometry); refusing to continue.",
                dev, superblock.num_devices
            ),
        ));
    }

    // A wiped primary that fell back to a backup is a recoverable metadata
    // divergence: counted, not fatal.
    let primary_ok = Superblock::read_primary(&mut fp, base_offset).is_ok();

    let mut metadata_mirror_mismatches: u64 = 0;
    if !primary_ok {
        eprintln!(
            "note: primary superblock (64 KiB) unreadable; fell back to an \
             intact backup copy — recoverable metadata divergence (rc unaffected)"
        );
        metadata_mirror_mismatches += 1;
    }

    // Sanity check: the device must be large enough for the declared size.
    let dev_size = crate::array::stripe::device_size(&fp).unwrap_or(0);
    if dev_size > 0 && dev_size < base_offset + superblock.total_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "device too short: {} bytes, superblock declares {} bytes used from offset {}",
                dev_size, superblock.total_bytes, base_offset
            ),
        ));
    }

    let strategy = CsumStrategy::from_superblock(&superblock)?;

    // Bootstrap: the system chunk array maps the chunk tree, whose walk
    // below yields the full chunk map.
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
    .with_devid(superblock.devid)
    .with_fsid(superblock.fsid);

    let mut chunk_records: Vec<ChunkRecord> = Vec::new();
    let mut metadata_header_errors: u64 = 0;
    let mut metadata_read_errors: u64 = 0;
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
            metadata_header_errors += 1;
        },
        |_logical| {},
        |_logical| {
            metadata_mirror_mismatches += 1;
        },
        |_logical| {
            metadata_read_errors += 1;
        },
    )?;
    for rec in &chunk_records {
        chunk_map.insert(rec);
    }

    chunk_map
        // Refuse striped data chunks: the scrub maps logical→physical linearly.
        .validate_data_profiles()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{dev}: {e}")))?;

    // Resolve the well-known roots from the root tree's ROOT_ITEMs.
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
            metadata_header_errors += 1;
        },
        |_logical| {},
        |_logical| {
            metadata_mirror_mismatches += 1;
        },
        |_logical| {
            metadata_read_errors += 1;
        },
    )?;
    let roots = TreeRoots {
        fs_root: fs_root.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "FS_TREE root not found in btrfs root tree",
            )
        })?,
        csum_root: csum_root.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "CSUM_TREE root not found in btrfs root tree",
            )
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
        metadata_mirror_mismatches,
        metadata_read_errors,
    })
}

/// Re-read the superblock and resolve the live EXTENT/CSUM roots — what
/// write-time re-confirmation walks. None when either read fails.
pub fn live_data_tree_roots(
    reader: &mut FsReader,
    chunk_map: &ChunkMap,
    base_offset: u64,
    counters: Option<&StatusCounters>,
) -> Option<(u64, u64)> {
    let sb = read_live_superblock(reader, base_offset, counters)?;
    resolve_live_tree_roots(reader, chunk_map, counters, &sb)
}

/// Read the superblock through a cloned fd; a failure bumps the metadata
/// read-error counter and returns None.
pub(crate) fn read_live_superblock(
    reader: &FsReader,
    base_offset: u64,
    counters: Option<&StatusCounters>,
) -> Option<Superblock> {
    let mut fp = match reader.reopen() {
        Ok(fp) => fp,
        Err(_) => {
            if let Some(c) = counters {
                c.metadata_read_errors.fetch_add(1, Ordering::Relaxed);
            }
            return None;
        }
    };
    match Superblock::read(&mut fp, base_offset) {
        Ok(sb) => Some(sb),
        Err(_) => {
            if let Some(c) = counters {
                c.metadata_read_errors.fetch_add(1, Ordering::Relaxed);
            }
            None
        }
    }
}

/// Walk the live root tree for the EXTENT_TREE and CSUM_TREE roots.
pub(crate) fn resolve_live_tree_roots(
    reader: &mut FsReader,
    chunk_map: &ChunkMap,
    counters: Option<&StatusCounters>,
    sb: &Superblock,
) -> Option<(u64, u64)> {
    let root_tree = sb.root;
    let mut extent_root: Option<u64> = None;
    let mut csum_root: Option<u64> = None;
    let walk = walk_leaves(
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
                // Early exit once both roots are found; the resulting
                // walk error is the expected completion signal.
                if extent_root.is_some() && csum_root.is_some() {
                    return Err(io::Error::other(
                        "live root-tree walk complete (early exit)",
                    ));
                }
            }
            Ok(())
        },
        |_logical| {
            if let Some(c) = counters {
                c.metadata_header_errors.fetch_add(1, Ordering::Relaxed);
            }
        },
        |_logical| {},
        |_logical| {
            if let Some(c) = counters {
                c.metadata_mirror_mismatches.fetch_add(1, Ordering::Relaxed);
            }
        },
        |_logical| {
            if let Some(c) = counters {
                c.metadata_read_errors.fetch_add(1, Ordering::Relaxed);
            }
        },
    );
    // The early-exit error is success; a clean Ok(()) means the walk ended
    // without finding both roots.
    match walk {
        Err(_) => match (extent_root, csum_root) {
            (Some(ext), Some(csum)) => Some((ext, csum)),
            _ => {
                if let Some(c) = counters {
                    c.metadata_read_errors.fetch_add(1, Ordering::Relaxed);
                }
                None
            }
        },
        Ok(()) => match (extent_root, csum_root) {
            (Some(ext), Some(csum)) => Some((ext, csum)),
            _ => {
                if let Some(c) = counters {
                    c.metadata_header_errors.fetch_add(1, Ordering::Relaxed);
                }
                None
            }
        },
    }
}
