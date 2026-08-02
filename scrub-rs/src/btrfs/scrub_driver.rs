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

use std::sync::Arc;

use crate::btrfs::chunk::ChunkMap;
use crate::btrfs::csum::LazyCsumProvider;
use crate::btrfs::csum_strategy::CsumStrategy;
use crate::btrfs::dev_extent::build_dev_extents;
use crate::btrfs::extent::reconfirm_mismatch;
use crate::btrfs::open::live_data_tree_roots;
use crate::btrfs::reader::FsReader;
use crate::btrfs::scrub::scrub_dev_tree;
use crate::btrfs::superblock::Superblock;
use crate::fs;
use crate::fs::{Reconfirm, ReconfirmRequest, Reconfirmer, ScrubEvent, ScrubStats, SectorVerifier};

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
    /// Bounded-memory CSUM lookup — walks the CSUM_TREE on demand for each
    /// dev-extent's logical range, instead of materialising every sector's
    /// csum into a `BTreeMap` at open.  This is the fix for the `OOM
    /// (rc=137)` bug on multi-TB disks: peak csum memory stays bounded by
    /// the largest single block group's csum span regardless of disk size.
    /// See [`crate::btrfs::csum::LazyCsumProvider`] for the contract + the
    /// bound; the eager `CsumMap` path is kept only for `craft-corrupt`
    /// back-compat (the scrub never materialises it).
    csum_provider: LazyCsumProvider,
    /// Dev-extents enumerated from the DEV_TREE, in ascending physical order.
    /// The drive set for the physical-order scrub ([`scrub_dev_tree`]); built
    /// once in [`BtrfsScrub::open`] so the caller can pick either scrub path.
    dev_extents: Vec<crate::btrfs::dev_extent::DevExtent>,
    strategy: CsumStrategy,
    superblock: Superblock,
    /// Metadata nodes whose *all* mirror copies failed header-checksum
    /// verification, counted during the chunk/root-tree walks in `open.rs`.
    /// Folded into the scrub stats so a DUP metadata node with no good copy
    /// surfaces as a hard error rather than a silent skip.
    metadata_header_errors: u64,
    /// Mirrored (DUP/RAID1/…) metadata nodes whose copies disagree — at
    /// least one copy is header-valid but the copies are not byte-identical.
    /// Counted by [`crate::btrfs::open::verify_metadata_mirrors`] during
    /// `open()`; surfaced as `metadata_mirror_mismatches` so a single
    /// corrupt DUP metadata copy is reported (self-heal-recoverable) rather
    /// than silently healed by the good-copy cross-check.
    metadata_mirror_mismatches: u64,
    /// Metadata nodes that failed with a READ (EIO) error during the
    /// chunk/root/DEV-tree walks — the bytes could not be fetched at all.
    /// Folded into the scrub stats so a hardware-faulting metadata node
    /// surfaces as a hard error rather than a silent skip.
    metadata_read_errors: u64,
    /// The device path this filesystem was opened from, for the CLI header
    /// ([`FilesystemScrub::describe`]).
    dev: String,
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
            mut metadata_header_errors,
            mut metadata_mirror_mismatches,
            mut metadata_read_errors,
        } = ctx;

        // Build the lazy CSUM-tree walker up front (cheap — it owns an
        // independent file handle + a clone of the chunk map, but performs
        // no walk until `range()` is called per dev-extent in the scrub).
        // This replaces the eager whole-disk `CsumMap` materialisation and
        // is the fix for the `OOM (rc=137)` bug on multi-TB disks: peak
        // csum memory is bounded by the largest single block group's csum
        // span, independent of disk size.  See [`LazyCsumProvider`] for the
        // bound + the metadata-error-counter fold-up below.
        //
        // `reader.reopen()` dups the backing fd so the lazy walker's seek
        // position never races the main reader's metadata walks.
        let lazy_file = reader.reopen()?;
        let csum_provider = LazyCsumProvider::new(
            lazy_file,
            superblock.node_size as usize,
            reader.base_offset(),
            strategy,
            superblock.devid,
            superblock.fsid,
            chunk_map.clone(),
            roots.csum_root,
        );

        // Enumerate the device's dev-extents from the DEV_TREE, in ascending
        // physical order.  This is the drive set for the physical-order
        // scrub ([`scrub_dev_tree`]); it is built eagerly here (cheap — a
        // single tree walk) so the caller can choose either scrub path
        // without re-opening the device.  `dev_tree_root` is always present
        // on a real filesystem; `expect` keeps the contract tight.
        //
        // As with the lazy csum walk above, a corrupted DEV_TREE leaf (no
        // good mirror copy) would make the walk skip that branch, enumerate
        // fewer/no dev-extents, and the scrub would run with an effectively
        // empty drive set and report 0 mismatches with exit 0.  Count it as
        // metadata_header_errors / metadata_mirror_mismatches so the gap
        // surfaces instead of silently under-scrubbing the device.
        let dev_tree_root = roots
            .dev_tree_root
            .expect("DEV_TREE root missing from btrfs root tree");
        let dev_extents = build_dev_extents(
            &mut reader,
            &chunk_map,
            dev_tree_root,
            superblock.devid,
            &mut metadata_header_errors,
            &mut metadata_mirror_mismatches,
            &mut metadata_read_errors,
        )?;

        Ok(Self {
            reader,
            chunk_map,
            csum_provider,
            dev_extents,
            strategy,
            superblock,
            metadata_header_errors,
            metadata_mirror_mismatches,
            metadata_read_errors,
            dev: dev.to_string(),
        })
    }

}

impl fs::FilesystemScrub for BtrfsScrub {
    /// Run the scrub: drive reads off the DEV_TREE (ascending physical
    /// order) for a single front-to-back pass over the disk, while still
    /// consulting the CSUM tree (via the lazy csum provider) for the
    /// per-sector expected checksum.  This is the sole scrub path — the
    /// earlier per-inode (`scrub_extents`) and logical-order CSUM-tree
    /// (`scrub_csum_tree`) variants were removed.  See
    /// [`crate::btrfs::scrub::scrub_dev_tree`] for the `ScrubStats`
    /// semantics (notably `sectors_no_csum` stays 0 here).
    ///
    /// Whether mismatches are emitted raw (deferred re-confirmation by the
    /// recovery sink) or re-confirmed inline is decided by
    /// [`ScrubCallbacks::wants_raw_candidates`].
    fn run(&mut self, callbacks: &mut dyn crate::fs::ScrubCallbacks) -> io::Result<ScrubStats> {
        let batch = callbacks.wants_raw_candidates();
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
        //                  disk, a verifier closure, and an opaque
        //                  re-confirm request.  No checksum bytes, no
        //                  algorithm name, no diagnostic context.  This
        //                  is the stable seam.
        //
        // The verifier is built *here* from the btrfs checksum algorithm
        // bound together with the stored bytes, and handed straight to
        // the caller via the event.  Recovery never learns which checksum
        // btrfs used — exactly the seam we want for a future ZFS
        // sha256/blake3 impl, which would build its own closure here.
        // The re-confirm request is equally opaque: the recovery sink
        // hands it back to `self.reconfirmer()` at write time, so btrfs's
        // EXTENT_TREE/CSUM_TREE walk never leaks into the glue.
        let block_size = self.strategy.sector_size as usize;
        let mut emit = |r: &crate::btrfs::scrub::SectorResult| {
            // 1. Log line (btrfs-owned format).
            let stored_tag = match &r.stored_csum {
                None => format!(
                    "actual=0x{} (no stored csum)",
                    crate::btrfs::util::hex(&r.actual_csum)
                ),
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
            //    which checksum btrfs used.  `Arc` so a batched writer
            //    thread can clone it onto its channel.  The re-confirm
            //    request carries the logical address + stored csum as an
            //    opaque pair for the writer's [`Reconfirmer`].
            let (verify, reconfirm) = match r.stored_csum.as_ref() {
                None => (None, None),
                Some(stored) => {
                    let stored = stored.clone();
                    let strategy = self.strategy;
                    let verify = {
                        let s = stored.clone();
                        Arc::new(move |b: &[u8]| strategy.compute(b) == s) as SectorVerifier
                    };
                    let reconfirm = ReconfirmRequest {
                        token: r.logical,
                        stored_csum: stored,
                    };
                    (Some(verify), Some(reconfirm))
                }
            };
            callbacks.on_event(&ScrubEvent {
                array_phys: r.array_phys,
                block_size,
                verify,
                reconfirm,
                // A sector whose source bytes were unreadable (EIO) is
                // flagged so the recovery sink skips its self-heal
                // re-read and uses a zero placeholder instead.
                unreadable: r.unreadable,
            });
        };

        let local = scrub_dev_tree(
            &mut self.reader,
            &self.chunk_map,
            &mut self.csum_provider,
            &self.dev_extents,
            &self.strategy,
            batch,
            &mut emit,
        );

        // `scrub_dev_tree` doesn't surface an io::Error today — it logs
        // read-errors inline and folds them into the stats — so we return
        // Ok here.  A future failure that should abort the scrub can be
        // propagated via the explicit `io::Result` return.
        //
        // `metadata_header_errors` comes from the chunk/root-tree walks in
        // `open.rs` (DUP metadata nodes with no good copy) PLUS any
        // CSUM_TREE nodes the lazy csum provider had to skip during the
        // scrub (it folds its own per-walk counter via
        // [`LazyCsumProvider::metadata_errors`]).  Both classes signal that
        // the scrub could not trust metadata it needed, so some data may
        // have been silently skipped — surfaced here as a single combined
        // counter so main.rs treats it as a hard error (non-zero →
        // non-zero exit).
        Ok(ScrubStats {
            sectors_checked: local.sectors_checked,
            sectors_ok: local.sectors_ok,
            sectors_mismatch: local.sectors_mismatch,
            sectors_no_csum: local.sectors_no_csum,
            sectors_read_error: local.sectors_read_error,
            sectors_stale: local.sectors_stale,
            bytes_checked: local.bytes_checked,
            metadata_header_errors: self.metadata_header_errors
                + self.csum_provider.metadata_errors(),
            metadata_mirror_mismatches: self.metadata_mirror_mismatches,
            metadata_read_errors: self.metadata_read_errors
                + self.csum_provider.metadata_read_errors(),
        })
    }

    fn reconfirmer(&self) -> io::Result<Box<dyn Reconfirmer>> {
        BtrfsReconfirmer::new(self).map(|r| Box::new(r) as Box<dyn Reconfirmer>)
    }

    fn describe(&self) -> Vec<String> {
        let sb = &self.superblock;
        let strategy = &self.strategy;
        // Upper bound on checksummed data sectors, computed without
        // materialising the lazy csum provider (see the old `num_sectors`
        // doc for why it is an upper bound, not an exact count).
        let num_sectors: u64 = self
            .dev_extents
            .iter()
            .map(|e| e.length / strategy.sector_size)
            .sum();
        vec![
            format!("device        : {}", self.dev),
            format!(
                "base offset   : 0x{:x} ({})",
                self.reader.base_offset(),
                self.reader.base_offset()
            ),
            format!("magic         : {:?}", sb.magic),
            format!("fsid          : {}", crate::btrfs::util::hex(&sb.fsid)),
            format!("bytenr        : 0x{:x}", sb.bytenr),
            format!("generation   : {}", sb.generation),
            format!("root          : 0x{:x}", sb.root),
            format!("chunk_root    : 0x{:x}", sb.chunk_root),
            format!("total_bytes   : {}", sb.total_bytes),
            format!("bytes_used    : {}", sb.bytes_used),
            format!("num_devices   : {}", sb.num_devices),
            format!("sector_size   : {}", sb.sector_size),
            format!("node_size     : {}", sb.node_size),
            format!("stripesize    : {}", sb.stripesize),
            format!("csum_type     : {} ({})", sb.csum_type, strategy.name),
            format!(
                "csum sectors  : {} ({} bytes)",
                num_sectors,
                num_sectors * strategy.sector_size
            ),
            format!("dev extents   : {}", self.dev_extents.len()),
        ]
    }

    fn superblock_offset(&self) -> u64 {
        crate::btrfs::superblock::SUPERBLOCK_OFFSET
    }

    fn block_has_magic(&self, block: &[u8]) -> bool {
        crate::btrfs::superblock::has_magic_at(block, crate::btrfs::superblock::OFF_MAGIC)
    }
}

/// btrfs's implementation of the seam's [`Reconfirmer`] — an independent
/// handle (own `FsReader`, own `ChunkMap`) that re-checks a deferred csum
/// mismatch against the *live* EXTENT_TREE + CSUM_TREE at write time.
///
/// Constructed via [`FilesystemScrub::reconfirmer`] from the scrub's own
/// reader (a dup'd fd + the already-built chunk map), so the writer thread
/// never shares the scrub's reader and the two can run concurrently.
struct BtrfsReconfirmer {
    reader: FsReader,
    chunk_map: ChunkMap,
    strategy: CsumStrategy,
}

impl BtrfsReconfirmer {
    fn new(scrub: &BtrfsScrub) -> io::Result<Self> {
        let f = scrub.reader.reopen()?;
        let reader = FsReader::new(
            f,
            scrub.reader.node_size(),
            scrub.reader.base_offset(),
            Some(scrub.strategy),
        )
        .with_devid(scrub.superblock.devid)
        .with_fsid(scrub.superblock.fsid);
        Ok(Self {
            reader,
            chunk_map: scrub.chunk_map.clone(),
            strategy: scrub.strategy,
        })
    }
}

impl Reconfirmer for BtrfsReconfirmer {
    fn reconfirm(&mut self, req: &ReconfirmRequest) -> Reconfirm {
        let base_offset = self.reader.base_offset();
        match live_data_tree_roots(&mut self.reader, &self.chunk_map, base_offset) {
            // Couldn't re-read the live trees — be conservative and treat
            // as real corruption (never hide a possible mismatch).
            None => Reconfirm::Corruption,
            Some((ext_root, csum_root)) => reconfirm_mismatch(
                &mut self.reader,
                &self.chunk_map,
                ext_root,
                csum_root,
                req.token,
                &req.stored_csum,
                self.strategy.hash_len,
                self.strategy.sector_size,
            ),
        }
    }
}
