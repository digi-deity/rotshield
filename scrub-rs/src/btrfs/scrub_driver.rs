//! `BtrfsScrub` — the btrfs-specific implementation of the
//! [`crate::fs::FilesystemScrub`] trait.
//!
//! This encapsulates all the btrfs-specific metadata setup that used to
//! live loose in `main.rs`: read the superblock, walk the chunk tree to
//! populate the chunk map, walk the root tree to find the FS_TREE and
//! CSUM_TREE roots, build the CSUM map, walk the FS tree to collect
//! REGULAR data extents, then run the per-sector scrub loop and emit
//! [`crate::fs::ScrubEvent`]s.
//!
//! `main.rs` is left doing only what's filesystem-agnostic: it
//! instantiates a scrub implementation (chosen via a `--fstype` flag in
//! the future; today hard-coded btrfs), runs it with a recovery callback,
//! and drives parity recovery against `array/` + `recovery/`.  The btrfs
//! tree shapes, inode layout, and CSUM-tree conventions never leak out
//! of this module — exactly the separation we already achieved for
//! `array/` (chunk gathering) and `recovery/` (parity math).

use std::io;

use crate::btrfs::chunk::ChunkMap;
use crate::btrfs::csum::{build_csum_map, CsumMap};
use crate::btrfs::extent::FileExtent;
use crate::btrfs::key;
use crate::btrfs::reader::FsReader;
use crate::btrfs::scrub::scrub_extents;
use crate::btrfs::superblock::{BTRFS_SECTOR_SIZE, Superblock};
use crate::btrfs::tree::walk_leaves;
use crate::fs::{FilesystemScrub, ScrubEvent, ScrubStats};

/// A btrfs filesystem scrub.
///
/// Holds all the long-lived state the scrub needs:
/// - the backing `File` opened twice (`fp` for the superblock peek,
///   `reader` for tree walks and data reads — btrfs's `FsReader` owns its
///   own `File` handle, so we keep both);
/// - the parsed superblock;
/// - the populated chunk map (logical → physical stripe);
/// - the CSUM tree materialised as a `CsumMap`;
/// - the collected list of REGULAR data extents to walk.
///
/// Construct via [`BtrfsScrub::open`] and drive via the
/// [`FilesystemScrub::run`] impl.
pub struct BtrfsScrub {
    reader: FsReader,
    chunk_map: ChunkMap,
    csum_map: CsumMap,
    extents: Vec<FileExtent>,
    superblock: Superblock,
}

impl BtrfsScrub {
    /// Open a btrfs filesystem living at `base_offset` inside `dev` and
    /// prepare for scrubbing.
    ///
    /// `base_offset` is 0 for a bare btrfs image or an array partition
    /// (`/dev/nmd1p1`); the per-disk `rdevOffset` (e.g. 64 sectors = 32
    /// KiB) for a whole-disk raw rdev like `/dev/loop2`.  All btrfs
    /// metadata addressing happens inside this constructor — the caller
    /// never sees a logical address or chunk record.
    ///
    /// The chunk-map/root-tree preamble is owned by [`crate::btrfs::open`];
    /// this constructor takes the resulting [`BtrfsContext`], builds the
    /// csum map, and collects FS-tree extents.  All three callers in the
    /// crate (`BtrfsScrub`, `bin/craft_corrupt`, `main::resolve_cmd`) go
    /// through the same `btrfs::open` so the chunk-map build never drifts.
    pub fn open(dev: &str, base_offset: u64) -> io::Result<Self> {
        let ctx = crate::btrfs::open(dev, base_offset)?;
        let crate::btrfs::BtrfsContext {
            mut reader,
            chunk_map,
            superblock,
            roots,
        } = ctx;

        // Build the checksum map from the CSUM tree.
        let mut csum_map = CsumMap::new();
        build_csum_map(&mut reader, &chunk_map, roots.csum_root, &mut csum_map)?;

        // Walk the FS tree and collect all REGULAR data extents.
        let mut extents: Vec<FileExtent> = Vec::new();
        walk_leaves(&mut reader, &chunk_map, roots.fs_root, |_r, leaf, _logical| {
            for i in 0..leaf.slots.len() {
                let slot = leaf.slots[i];
                if slot.key.ty == key::key_type::EXTENT_DATA {
                    if let Some(ext) = FileExtent::parse(
                        leaf.item_data(i),
                        slot.key.objectid,
                        slot.key.offset,
                    ) {
                        extents.push(ext);
                    }
                }
            }
            Ok(())
        })?;

        Ok(Self {
            reader,
            chunk_map,
            csum_map,
            extents,
            superblock,
        })
    }

    /// Borrow the parsed superblock — exposed for diagnostic / `--dump`
    /// style commands that want to print fs-level info without re-opening
    /// the device separately.
    pub fn superblock(&self) -> &Superblock {
        &self.superblock
    }

    /// Number of REGULAR data extents the FS tree advertised — useful for
    /// the caller's progress log.
    pub fn num_extents(&self) -> usize {
        self.extents.len()
    }

    /// Total bytes of REGULAR data extents — useful for the caller's
    /// progress log.
    pub fn extent_bytes(&self) -> u64 {
        self.extents.iter().map(|e| e.num_bytes).sum()
    }
}

impl FilesystemScrub for BtrfsScrub {
    fn run(&mut self, callbacks: &mut dyn crate::fs::ScrubCallbacks) -> io::Result<ScrubStats> {
        // Adapt btrfs's per-sector `SectorResult` callback into the two
        // contract streams:
        //
        //   * `on_log`  — owns the human-readable line, in btrfs's own
        //                 diagnostic vocabulary.  No structured fields
        //                 leave this closure, so a future ZFS impl can
        //                 format totally differently (DVA / blkptr /
        //                 edonr vs crc32c) without touching the trait.
        //
        //   * `on_event` — only the recovery-correct fields: where on
        //                  disk + the verifier closure.  No checksum
        //                  bytes, no algorithm name, no diagnostic
        //                  context.  This is the stable seam.
        //
        // The verifier is built *here* from the btrfs crc32c algorithm
        // bound together with the stored bytes, and handed straight to
        // the caller via the event.  Recovery never learns which
        // checksum btrfs used — exactly the seam we want for a future
        // ZFS sha256/blake3 impl, which would build its own closure here.
        let block_size = BTRFS_SECTOR_SIZE as usize;
        let mut emit = |r: &crate::btrfs::scrub::SectorResult| {
            // 1. Log line (btrfs-owned format).
            let stored_tag = match r.stored_csum {
                None => format!("actual=0x{} (no stored csum)", hex(&r.actual_csum)),
                Some(stored) => format!(
                    "stored=0x{} actual=0x{}",
                    hex(&stored),
                    hex(&r.actual_csum),
                ),
            };
            let line = format!(
                "  MISMATCH logical=0x{:x} devid={} array_phys=0x{:x} ino={} off=0x{:x} {stored_tag}",
                r.logical, r.devid, r.array_phys, r.inode, r.file_offset,
            );
            callbacks.on_log(&line);

            // 2. Recovery-only event.
            let verify = r.stored_csum.map(|stored| {
                Box::new(move |b: &[u8]| crc32c::crc32c(b).to_le_bytes() == stored)
                    as Box<dyn Fn(&[u8]) -> bool + Send + Sync>
            });
            callbacks.on_event(&ScrubEvent {
                array_phys: r.array_phys,
                block_size,
                verify,
            });
        };

        let local = scrub_extents(
            &mut self.reader,
            &self.chunk_map,
            &self.csum_map,
            &self.extents,
            &mut emit,
        );

        // btrfs's `scrub_extents` doesn't surface an io::Error today — it
        // logs read-errors inline and folds them into the stats — so we
        // return Ok here.  A future failure that should abort the scrub
        // can be propagated via the explicit `io::Result` return.
        Ok(ScrubStats {
            sectors_checked: local.sectors_checked,
            sectors_ok: local.sectors_ok,
            sectors_mismatch: local.sectors_mismatch,
            sectors_no_csum: local.sectors_no_csum,
            sectors_read_error: local.sectors_read_error,
            bytes_checked: local.bytes_checked,
        })
    }
}

/// Hex-encode `bytes` lowercase (4-byte btrfs crc32c becomes 8 hex chars;
/// a future ZFS 32-byte sha256 becomes 64).
fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut s, "{b:02x}");
    }
    s
}