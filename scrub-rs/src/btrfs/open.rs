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
use std::sync::atomic::Ordering;

use crate::status::StatusCounters;

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
    /// Number of mirrored (DUP/RAID1/…) metadata nodes whose copies
    /// DISAGREE — at least one copy is header-checksum valid but the valid
    /// copies are not byte-identical.  This is the self-heal-recoverable
    /// counterpart to `metadata_header_errors`: the filesystem can still
    /// read the good copy, but a correct scrub reports the divergence (as
    /// the kernel's `btrfs scrub` does) rather than healing it silently.
    /// Filled in by the chunk-tree/root-tree walk callbacks during `open`;
    /// surfaced for visibility (does not affect the exit code on its
    /// own, since the good copy means the data is intact).
    pub metadata_mirror_mismatches: u64,
    /// Number of metadata nodes that failed with a **read (device `EIO`)**
    /// error during the chunk/root-tree walks — the bytes could not be
    /// fetched at all.  Distinct from `metadata_header_errors` (checksum
    /// corruption): an `EIO` is hardware, the operator response differs.  A
    /// node that EIOs is skipped (not descended) and counted here so the
    /// coverage gap surfaces rather than silently under-scrubbing.
    pub metadata_read_errors: u64,
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

    // H9: the entire design of this tool (linear chunk mapping,
    // dev-extent-driven physical order, one devid, NonRAID slot = recovery
    // target) is only valid for a SINGLE-device filesystem.  Pointing it at
    // a member disk of a real multi-device btrfs pool would misread striped
    // chunk geometry and map every mismatch to the wrong logical address —
    // refuse loudly instead of running to completion with meaningless
    // numbers.  (A degraded mount of a multi-device pool keeps the original
    // num_devices count, so this check catches pool members even when only
    // one disk is present.)
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

    // Detect a wiped/primary-superblock-with-intact-backup situation.  The
    // main `read` above transparently fell back to a backup copy (64 MiB /
    // 256 GiB / 1 TiB) when the primary (64 KiB) was unusable, so the scrub
    // can still proceed — but a bad primary copy is itself a metadata
    // divergence the operator should know about (exactly the self-heal-
    // recoverable shape of a DUP metadata node with one bad mirror).  We
    // probe the primary copy on its own; if it fails but we already have a
    // good backup, count it as a recoverable metadata mirror mismatch and
    // continue the scrub.  This is deliberately NOT fatal: the filesystem
    // is fully readable via the backup, so the data result is trustworthy.
    let primary_ok = Superblock::read_primary(&mut fp, base_offset).is_ok();
    // Recoverable-metadata counter, declared up front so the primary-SB
    // divergence check below (and the tree walks later) can accumulate into
    // the same field.
    let mut metadata_mirror_mismatches: u64 = 0;
    if !primary_ok {
        eprintln!(
            "note: primary superblock (64 KiB) unreadable; fell back to an \
             intact backup copy — recoverable metadata divergence (rc unaffected)"
        );
        metadata_mirror_mismatches += 1;
    }

    // Sanity-check the device is large enough to actually hold the
    // filesystem the superblock describes.  A truncated image (or a
    // short/partial device) still has a valid primary superblock at
    // 0x10000, so without this check we'd happily walk the few sectors that
    // remain and report a false "clean" — exactly the case `btrfs check`
    // refuses with "couldn't read chunk root" / "short device".  Treat a
    // device shorter than the declared total_bytes as unopenable.
    // [`crate::array::stripe::device_size`] (NOT `metadata().len()` — on
    // Linux `st_size` is 0 for block devices, which would silently disable
    // this guard on every real array disk).
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
            // A mirrored (DUP) chunk-tree node with no good copy — count it.
            metadata_header_errors += 1;
        },
        |_logical| {
            // A chunk-tree node whose only verifiable copies are stale
            // (block freed/repurposed by a concurrent transaction): the
            // filesystem moved on — normal churn, not an error.  Skip
            // silently; staleness is never counted (see tree.rs `on_stale`).
        },
        |_logical| {
            // A mirrored (DUP) chunk-tree node whose copies disagree but
            // still has a good copy — self-heal-recoverable divergence.
            metadata_mirror_mismatches += 1;
        },
        |_logical| {
            // A chunk-tree node that failed with a READ (EIO) error — the
            // bytes could not be fetched at all.  Skip + count so the gap
            // surfaces instead of silently under-scrubbing the device.
            metadata_read_errors += 1;
        },
    )?;
    for rec in &chunk_records {
        chunk_map.insert(rec);
    }

    // H9: refuse any DATA chunk whose profile the physical-order scrub
    // cannot map linearly.  Only SINGLE and DUP data chunks are supported;
    // a striped RAID profile (RAID0/RAID10/RAID5/RAID6) means a dev-extent
    // is not a contiguous sub-range of the chunk's logical space, so the
    // `phys - dev_extent.phys_start` linear mapping would silently point at
    // the wrong sectors.  (The superblock num_devices check above already
    // rejects genuine multi-device pools; this is the belt-and-suspenders
    // guard for the single-device case and for data chunks the filesystem
    // never should have contained.)  Metadata/system chunk profiles are
    // deliberately NOT checked — a single-device FS can legally hold
    // RAID1/RAID1C3 metadata after a dup-to-raid1 convert, and those trees
    // are verified at walk time, not linearly mapped.
    chunk_map
        .validate_data_profiles()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{dev}: {e}")))?;

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
        |_logical| {
            // A root-tree node whose only verifiable copies are stale
            // (block freed/repurposed by a concurrent transaction): the
            // filesystem moved on — normal churn, not an error.  Skip
            // silently.
        },
        |_logical| {
            // A mirrored (DUP) root-tree node whose copies disagree but
            // still has a good copy — self-heal-recoverable divergence.
            metadata_mirror_mismatches += 1;
        },
        |_logical| {
            // A root-tree node that failed with a READ (EIO) error — the
            // bytes could not be fetched at all.  Skip + count so the gap
            // surfaces instead of silently under-scrubbing.
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

    // `metadata_mirror_mismatches` was accumulated by the two `walk_leaves`
    // calls above (chunk-tree and root-tree walks).  Each walk now reports
    // a mirrored (DUP/RAID1) node whose copies disagree but still has a good
    // copy — the self-heal-recoverable divergence — via its
    // `on_mirror_mismatch` callback.  Because the comparison happens *inside*
    // `read_node` (which already reads every stripe of a node in a single
    // pass and counts how many are csum-valid), this is a lockstep check
    // folded into the one existing tree walk: no second traversal, no
    // scanning of freed/padding blocks.  `metadata_header_errors` is also
    // accumulated by those same walks (nodes with no good copy at all).

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
/// `counters` (when present) receives every metadata failure this walk
/// encounters, so tree-root-level unreadability is surfaced in the
/// metadata-error counters exactly like the per-leaf CSUM_TREE case (see
/// [`crate::btrfs::csum::LazyCsumProvider`]): no-good-copy nodes count as
/// `metadata_header_errors`, mirror divergences as
/// `metadata_mirror_mismatches`, and read (EIO) failures as
/// `metadata_read_errors`.  Stale (freed/repurposed) nodes are normal
/// churn and are deliberately NOT counted, matching the per-leaf
/// `on_stale` no-op.
///
/// Returns `None` if the live superblock or root tree cannot be read —
/// with the failure counted into `counters` — or if the walk completes
/// without finding both ROOT_ITEMs (branches may have been skipped by the
/// metadata failures above).  A `None` return means the sector's liveness
/// CANNOT be verified: callers must treat it as
/// [`crate::fs::Reconfirm::Unverifiable`] (skip any recovery write), never
/// as confirmed corruption.
pub fn live_data_tree_roots(
    reader: &mut FsReader,
    chunk_map: &ChunkMap,
    base_offset: u64,
    counters: Option<&StatusCounters>,
) -> Option<(u64, u64)> {
    // Read the live superblock first (cheap — one 4 KiB positional read),
    // then walk the root tree it points at.  Split into two helpers so the
    // batched re-confirmer ([`crate::btrfs::scrub_driver::BtrfsReconfirmer`])
    // can read the superblock ONCE and reuse previously-resolved roots when
    // the generation is unchanged (H4/M2), instead of re-walking the entire
    // root tree per candidate.
    let sb = read_live_superblock(reader, base_offset, counters)?;
    resolve_live_tree_roots(reader, chunk_map, counters, &sb)
}

/// Read the live superblock via a dup'd reader handle, counting either
/// failure class (fd dup, superblock read) as a metadata READ error so the
/// run can never look clean, and let the caller downgrade to Unverifiable.
///
/// `reader` is only borrowed (`reopen` dups its backing fd), so callers can
/// read the superblock while the reader is otherwise idle.
pub(crate) fn read_live_superblock(
    reader: &FsReader,
    base_offset: u64,
    counters: Option<&StatusCounters>,
) -> Option<Superblock> {
    // The reader owns the File handle; dup it and read the primary superblock.
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

/// Walk the *current* root tree (the one the caller's superblock points
/// at) to find the EXTENT_TREE and CSUM_TREE ROOT_ITEMs.  Every metadata
/// failure class is counted (mirroring the per-leaf mapping in
/// [`crate::btrfs::csum::LazyCsumProvider`]), so a skipped branch that
/// hides a ROOT_ITEM still surfaces as a coverage gap.
///
/// **Early exit:** the walk aborts as soon as BOTH ROOT_ITEMs are found
/// (the leaf closure's `Err` propagates immediately out of
/// [`walk_leaves`]).  Per-candidate callers were re-walking the entire
/// root tree dozens of times per batch (M2/H4); with the early exit they
/// pay only for the leaves actually needed.
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
                if extent_root.is_some() && csum_root.is_some() {
                    // Early exit: both ROOT_ITEMs resolved — abort the walk
                    // (the leaf closure's Err propagates immediately; the
                    // remaining queue entries are dropped, and no further
                    // metadata errors can be discovered for branches we no
                    // longer need).  The sentinel kind is never surfaced.
                    return Err(io::Error::other(
                        "live root-tree walk complete (early exit)",
                    ));
                }
            }
            Ok(())
        },
        |_logical| {
            // A root-tree node with no good copy — the walk can't trust
            // this branch and skips it.  Count it as a metadata header
            // error (mirroring the per-leaf CSUM_TREE mapping), so a
            // ROOT_ITEM hidden behind it still surfaces.
            if let Some(c) = counters {
                c.metadata_header_errors.fetch_add(1, Ordering::Relaxed);
            }
        },
        |_logical| {
            // A root-tree node whose only verifiable copies are stale
            // (concurrent churn): normal churn, never an error — not
            // counted, matching the per-leaf `on_stale` no-op.
        },
        |_logical| {
            // Mirror divergence on the live root tree — count it, mirroring
            // the per-leaf mapping.
            if let Some(c) = counters {
                c.metadata_mirror_mismatches.fetch_add(1, Ordering::Relaxed);
            }
        },
        |_logical| {
            // A read (EIO) error on the live root tree — count it,
            // mirroring the per-leaf mapping.
            if let Some(c) = counters {
                c.metadata_read_errors.fetch_add(1, Ordering::Relaxed);
            }
        },
    );
    match walk {
        // `Err` here is ONLY the early-exit sentinel above (per-node
        // failure classes are delivered through the callbacks, never as a
        // walk error).  Both roots resolved ⇒ successful early exit.
        Err(_) => match (extent_root, csum_root) {
            (Some(ext), Some(csum)) => Some((ext, csum)),
            _ => {
                // Defensive: an unexpected walk error — the live trees are
                // unreadable.  Unverifiable.
                if let Some(c) = counters {
                    c.metadata_read_errors.fetch_add(1, Ordering::Relaxed);
                }
                None
            }
        },
        Ok(()) => match (extent_root, csum_root) {
            (Some(ext), Some(csum)) => Some((ext, csum)),
            _ => {
                // The walk finished but did not find both ROOT_ITEMs (a branch
                // carrying one was skipped by a metadata failure above, or the
                // live root tree is structurally incomplete).  Unverifiable.
                if let Some(c) = counters {
                    c.metadata_header_errors.fetch_add(1, Ordering::Relaxed);
                }
                None
            }
        },
    }
}
