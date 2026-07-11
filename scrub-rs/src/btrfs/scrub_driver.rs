//! `BtrfsScrub` — the btrfs-specific implementation of the
//! [`crate::fs::FilesystemScrub`] trait.
//!
//! This encapsulates all the btrfs-specific metadata setup that used to
//! live loose in `main.rs`: read the superblock, walk the chunk tree to
//! populate the chunk map, walk the root tree to find the FS_TREE and
//! CSUM_TREE roots, build the CSUM map, then run the per-sector scrub loop
//! and emit [`crate::fs::ScrubEvent`]s.
//!
//! The data scrub is driven directly off the **global CSUM tree** (see
//! [`crate::btrfs::scrub::scrub_csum_tree`]): the csum tree enumerates
//! every checksummed data sector exactly once, keyed by logical address,
//! so the scrub is both exhaustive across all subvolumes/snapshots and
//! automatically deduplicated for shared (COW / reflink) extents — no
//! per-subvolume FS-tree walk required.
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
use crate::btrfs::csum_strategy::CsumStrategy;
use crate::btrfs::reader::FsReader;
use crate::btrfs::scrub::scrub_csum_tree;
use crate::btrfs::superblock::Superblock;
use crate::fs::{FilesystemScrub, ScrubEvent, ScrubStats};

/// A btrfs filesystem scrub.
///
/// Holds all the long-lived state the scrub needs:
/// - the backing `File` opened twice (`fp` for the superblock peek,
///   `reader` for tree walks and data reads — btrfs's `FsReader` owns its
///   own `File` handle, so we keep both);
/// - the parsed superblock;
/// - the populated chunk map (logical → physical stripe);
/// - the CSUM tree materialised as a `CsumMap` — this is the scrub source
///   of truth (see module docs);
/// - the checksum strategy (algorithm + sector size) taken from the
///   superblock — the scrub no longer assumes CRC32C over 4096-byte
///   sectors.
///
/// Construct via [`BtrfsScrub::open`] and drive via the
/// [`FilesystemScrub::run`] impl.
pub struct BtrfsScrub {
    reader: FsReader,
    chunk_map: ChunkMap,
    csum_map: CsumMap,
    strategy: CsumStrategy,
    superblock: Superblock,
    /// Metadata nodes whose *all* mirror copies failed header-checksum
    /// verification, counted during the chunk/root-tree walks in `open.rs`.
    /// Folded into the scrub stats so a DUP metadata node with no good copy
    /// surfaces as a hard error rather than a silent skip.
    metadata_header_errors: u64,
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
    /// this constructor takes the resulting [`BtrfsContext`] and builds the
    /// csum map.  All three callers in the crate (`BtrfsScrub`,
    /// `bin/craft_corrupt`, `main::resolve_cmd`) go through the same
    /// `btrfs::open` so the chunk-map build never drifts.
    pub fn open(dev: &str, base_offset: u64) -> io::Result<Self> {
        let ctx = crate::btrfs::open(dev, base_offset)?;
        let crate::btrfs::BtrfsContext {
            mut reader,
            chunk_map,
            superblock,
            roots,
            strategy,
            metadata_header_errors,
        } = ctx;

        // Build the checksum map from the CSUM tree.  The strategy (csum
        // algorithm + sector size) comes from the superblock (built once in
        // `btrfs::open` and threaded through `BtrfsContext`) so the scrub
        // honours what the filesystem actually uses.  The csum map is the
        // scrub's source of truth: it enumerates every checksummed data
        // sector exactly once, across all subvolumes/snapshots.
        let mut csum_map = CsumMap::new();
        build_csum_map(&mut reader, &chunk_map, roots.csum_root, &strategy, &mut csum_map)?;

        Ok(Self {
            reader,
            chunk_map,
            csum_map,
            strategy,
            superblock,
            metadata_header_errors,
        })
    }

    /// Borrow the parsed superblock — exposed for diagnostic / `--dump`
    /// style commands that want to print fs-level info without re-opening
    /// the device separately.
    pub fn superblock(&self) -> &Superblock {
        &self.superblock
    }

    /// Number of checksummed data sectors the CSUM tree advertised — useful
    /// for the caller's progress log.  This is the deduplicated, exhaustive
    /// scrub set (covers all subvolumes/snapshots).
    pub fn num_sectors(&self) -> usize {
        self.csum_map.len()
    }

    /// Human-readable name of the filesystem's checksum algorithm (e.g.
    /// "crc32c", "xxhash", "sha256", "blake2"), taken from the superblock.
    pub fn csum_name(&self) -> &'static str {
        self.strategy.name
    }

    /// Total bytes of checksummed data sectors — useful for the caller's
    /// progress log.
    pub fn csum_bytes(&self) -> u64 {
        self.csum_map.len() as u64 * self.strategy.sector_size
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
        let block_size = self.strategy.sector_size as usize;
        let mut emit = |r: &crate::btrfs::scrub::SectorResult| {
            // 1. Log line (btrfs-owned format).
            let stored_tag = match &r.stored_csum {
                None => format!("actual=0x{} (no stored csum)", crate::btrfs::util::hex(&r.actual_csum)),
                Some(stored) => format!(
                    "stored=0x{} actual=0x{}",
                    crate::btrfs::util::hex(stored),
                    crate::btrfs::util::hex(&r.actual_csum),
                ),
            };
            let line = format!(
                "  MISMATCH logical=0x{:x} devid={} array_phys=0x{:x} ino={} off=0x{:x} {stored_tag}",
                r.logical, r.devid, r.array_phys, r.inode, r.file_offset,
            );
            callbacks.on_log(&line);

            // 2. Recovery-only event.  The verifier is built from the
            //    filesystem's csum strategy (algorithm + hash length) bound
            //    together with the stored bytes — recovery never learns
            //    which checksum btrfs used.
            let verify = r.stored_csum.as_ref().map(|stored| {
                let stored = stored.clone();
                let strategy = self.strategy;
                Box::new(move |b: &[u8]| strategy.compute(b) == stored)
                    as Box<dyn Fn(&[u8]) -> bool + Send + Sync>
            });
            callbacks.on_event(&ScrubEvent {
                array_phys: r.array_phys,
                block_size,
                verify,
            });
        };

        let local = scrub_csum_tree(
            &mut self.reader,
            &self.chunk_map,
            &self.csum_map,
            &self.strategy,
            &mut emit,
        );

        // btrfs's `scrub_csum_tree` doesn't surface an io::Error today — it
        // logs read-errors inline and folds them into the stats — so we
        // return Ok here.  A future failure that should abort the scrub
        // can be propagated via the explicit `io::Result` return.
        //
        // `metadata_header_errors` comes from the chunk/root-tree walks in
        // `open.rs` (DUP metadata nodes with no good copy).  It is a
        // distinct failure class from data-sector mismatches: a non-zero
        // value means the scrub could not trust metadata it needed to
        // traverse, so some data may have been silently skipped.  main.rs
        // treats it as a hard error (non-zero → non-zero exit).
        Ok(ScrubStats {
            sectors_checked: local.sectors_checked,
            sectors_ok: local.sectors_ok,
            sectors_mismatch: local.sectors_mismatch,
            sectors_no_csum: local.sectors_no_csum,
            sectors_read_error: local.sectors_read_error,
            bytes_checked: local.bytes_checked,
            metadata_header_errors: self.metadata_header_errors,
        })
    }
}

