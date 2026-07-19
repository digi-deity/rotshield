//! A logical-address reader for a single-device btrfs filesystem.
//!
//! Owns the open backing store (a regular file or a block device — both are
//! just seekable byte streams to `std::fs::File`), knows the node size, and
//! is given a `&ChunkMap` for logical→physical translation on every read.
//!
//! Reads go through the backing store the caller opened — typically an
//! array-partition device like `/dev/nmd1p1`.  This means the NonRAID
//! array driver is in the read path: for a present but corrupt disk the
//! corruption is visible, but for a *missing* disk the driver
//! transparently reconstructs from parity and the scrub cannot detect
//! it.  This is a known limitation, not a bug — see the "Address spaces
//! and I/O paths" doc in `array::mod` for details.
//!
//! The chunk map is deliberately **not** owned here.  It is immutable after
//! the chunk-tree walk and shared by reference with anything that needs
//! logical→physical resolution — including the scrub loop's caller, which
//! may want to resolve a mismatch's physical location for inline recovery.
//! Keeping the map as a separate `&` borrow from `&mut FsReader` lets both
//! happen without cloning or buffering.

use std::fs::File;

use super::chunk::ChunkMap;
use super::csum_strategy::CsumStrategy;
use super::node::Node;
use super::util::read_at;

/// Sentinel `expected_generation` meaning "do not generation-check".  Passed
/// by callers that do not yet know the correct generation (e.g. the
/// filesystem-global tree roots before they are wired to real generations).
pub const GEN_DONT_CHECK: u64 = u64::MAX;

/// Result of [`FsReader::read_node`]: the parsed node plus flags.
///
/// `all_mirrors_failed` is `false` when at least one copy validated (or the
/// chunk is single-stripe, where there is nothing to cross-check against).
/// It is `true` only when the node lives in a mirrored chunk and *none* of
/// its copies passed header verification — i.e. the corruption is
/// unrecoverable by the DUP cross-check and the returned node is the first
/// copy (which will likely fail to parse or, worse, parse into garbage that
/// silently drops a subtree).  Callers should count this as a
/// [`crate::fs::ScrubStats::metadata_header_errors`] rather than swallowing
/// it.
///
/// `generation_mismatch` is `true` iff a csum-valid mirror was found but its
/// header `generation` did not match the caller's `expected_generation`
/// (i.e. the copy is stale, not corrupt — the block was freed and reused by
/// a later transaction).  Callers should treat this as untrustworthy and
/// skip the branch, exactly as with `all_mirrors_failed`.
pub struct ReadNodeResult {
    /// The parsed node.  `None` when no mirror copy could be verified (i.e.
    /// `all_mirrors_failed` is `true`) — we deliberately do **not** parse a
    /// corrupt buffer, because its `nritems`/slot offsets are untrustworthy
    /// and the unchecked indexing in `Node::parse` would panic on garbage.
    /// Callers only ever read this on the `!all_mirrors_failed` path (see
    /// `walk_leaves`), so a failed node is correctly left unparsed.
    pub node: Option<Node>,
    pub all_mirrors_failed: bool,
    pub generation_mismatch: bool,
    /// `true` iff the node lives in a mirrored (DUP/RAID1/…) chunk and at
    /// least one mirror copy is header-checksum valid (so the block is
    /// still readable / self-healable) but *not every* copy is valid — i.e.
    /// one or more mirrors are corrupt.  This is the self-heal-recoverable
    /// counterpart to `all_mirrors_failed`: the filesystem can read the good
    /// copy, but a correct scrub should *report* the divergence (as the
    /// kernel's `btrfs scrub` does) rather than healing it silently.  Only
    /// meaningful for mirrored chunks; `false` for single-stripe chunks
    /// (where there is nothing to cross-check against).  Callers surface
    /// this as a `metadata_mirror_mismatches` counter.
    pub mirror_mismatch: bool,
}

/// A logical-address reader for a single-device btrfs filesystem.
///
/// Owns the open backing store (a regular file or a block device — both are
/// just seekable byte streams to `std::fs::File`), knows the node size, and
/// is given a `&ChunkMap` for logical→physical translation on every read.
///
/// Reads go through the backing store the caller opened — typically an
/// array-partition device like `/dev/nmd1p1`.  This means the NonRAID
/// array driver is in the read path: for a present but corrupt disk the
/// corruption is visible, but for a *missing* disk the driver
/// transparently reconstructs from parity and the scrub cannot detect
/// it.  This is a known limitation, not a bug — see the "Address spaces
/// and I/O paths" doc in `array::mod` for details.
///
/// The chunk map is deliberately **not** owned here.  It is immutable after
/// the chunk-tree walk and shared by reference with anything that needs
/// logical→physical resolution — including the scrub loop's caller, which
/// may want to resolve a mismatch's physical location for inline recovery.
/// Keeping the map as a separate `&` borrow from `&mut FsReader` lets both
/// happen without cloning or buffering.
///
/// Fields are private: callers build an `FsReader` via [`FsReader::new`]
/// (called from [`crate::btrfs::open`]); they cannot reach inside and
/// touch `fp`/`node_size`/`base_offset` directly, which keeps the
/// construction of a reader in one place.
pub struct FsReader {
    fp: File,
    node_size: usize,
    /// Byte offset added to every physical read.  0 for a bare btrfs image
    /// or an array partition (/dev/nmd1p1); the partition start (e.g.
    /// rdevOffset*512) for a whole-disk image or a raw rdev.  File and
    /// device paths share this single offset — there is no separate
    /// code path per backing-store kind.
    base_offset: u64,
    /// The btrfs device ID of the backing store this reader opened.  Each
    /// NonRAID slot is its own single-device filesystem, so there is exactly
    /// one device; [`FsReader::read_physical`] asserts the requested `devid`
    /// matches it rather than silently reading the wrong disk.  `None` when
    /// the reader was constructed without a known devid (e.g. a bare image
    /// whose devid hasn't been resolved yet) — in that case the guard is
    /// skipped and the single handle is used unconditionally.
    devid: Option<u64>,
    /// The checksum strategy (algorithm + sector size) taken from the
    /// superblock.  Used to verify every metadata node/leaf header
    /// checksum on read, and — for mirrored (DUP/RAID1/…) metadata — to
    /// cross-check the copies and prefer a good one over a corrupt header.
    /// `None` means "no verification" (e.g. a caller that doesn't have a
    /// strategy yet); the reader then behaves as before, trusting the
    /// first copy it reads.
    strategy: Option<CsumStrategy>,
    /// The filesystem UUID (`superblock.fsid`).  Used by
    /// [`FsReader::validate_header`] to reject a metadata block whose
    /// `fsid` does not match the filesystem we opened (a misdirected read or
    /// a block from a different filesystem).  `None` until
    /// [`FsReader::with_fsid`] is called (the open path always sets it).
    fsid: Option<[u8; 16]>,
}

impl FsReader {
    /// Open `dev` (a block device or image file) and wrap it as a reader
    /// with the given `node_size`, `base_offset`, and optional
    /// [`CsumStrategy`].  This is the single sanctioned construction site —
    /// call sites go through [`crate::btrfs::open`], which already knows
    /// `node_size` and the strategy from the superblock.
    ///
    /// Pass `strategy: None` to disable metadata-header verification (the
    /// reader then trusts the first stripe it reads, as it used to).
    pub fn new(
        fp: File,
        node_size: usize,
        base_offset: u64,
        strategy: Option<CsumStrategy>,
    ) -> Self {
        Self {
            fp,
            node_size,
            base_offset,
            strategy,
            devid: None,
            fsid: None,
        }
    }

    /// Set the btrfs device ID of the backing store.  Enables the devid
    /// guard in [`FsReader::read_physical`] so a physical read can never
    /// silently target the wrong disk.  Returns `&mut self` for chaining.
    pub fn with_devid(mut self, devid: u64) -> Self {
        self.devid = Some(devid);
        self
    }

    /// Set the filesystem UUID so [`FsReader::validate_header`] can reject
    /// metadata blocks whose `fsid` does not match.  Returns `&mut self`
    /// for chaining.  Called from [`crate::btrfs::open`] after the
    /// superblock is parsed.
    pub fn with_fsid(mut self, fsid: [u8; 16]) -> Self {
        self.fsid = Some(fsid);
        self
    }

    /// Duplicate the underlying file handle so a caller can re-read a
    /// *fresh* on-disk structure (e.g. the live superblock / current tree
    /// roots) without disturbing this reader's seek position or borrowing
    /// it mutably for the whole re-read.  Uses `File::try_clone` (dup of the
    /// fd), so it works for both regular files and block devices and needs
    /// no stored path.  The returned handle shares the same `base_offset`
    /// semantics via the `read_at` helper used by [`Superblock::read`].
    pub fn reopen(&self) -> std::io::Result<File> {
        self.fp.try_clone()
    }

    /// The partition byte offset this reader was opened with (0 for a bare
    /// image / array partition; the partition start for a whole-disk image
    /// or raw rdev).  Exposed so callers that re-read live on-disk
    /// structures (e.g. [`crate::btrfs::open::live_data_tree_roots`]) can
    /// pass the correct offset to [`crate::btrfs::superblock::Superblock::read`].
    pub fn base_offset(&self) -> u64 {
        self.base_offset
    }

    /// The filesystem node size (the size of every metadata tree node on
    /// disk).  Exposed so callers that read raw metadata blocks directly
    /// (e.g. the DUP-mirror cross-check in [`crate::btrfs::open`]) know how
    /// many bytes to request per node.
    pub fn node_size(&self) -> usize {
        self.node_size
    }

    /// Read `n` bytes starting at logical address `logical`, using
    /// `chunk_map` for the logical→physical translation.
    ///
    /// For now this only handles the simple case where `n` lies entirely
    /// within a single chunk's first stripe — sufficient for single-device
    /// and mirrored (DUP/RAID1) filesystems, which is what this tool targets.
    pub fn read_logical(
        &mut self,
        chunk_map: &ChunkMap,
        logical: u64,
        n: usize,
    ) -> std::io::Result<Vec<u8>> {
        let (_devid, phys) = chunk_map.lookup(logical).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("no chunk mapping for logical 0x{logical:x}"),
            )
        })?;
        // The single point where file-vs-device is irrelevant: `read_at` is
        // just seek+read on a `std::fs::File`, which works on either.
        read_at(&mut self.fp, self.base_offset + phys, n)
    }

    /// Read `n` bytes at **physical** offset `phys` on device `devid`,
    /// bypassing the chunk map entirely.
    ///
    /// Unlike [`FsReader::read_logical`], this does not translate a logical
    /// address — the caller has already computed the physical location
    /// (e.g. from a dev-extent walk).  This is the read primitive the
    /// physical-order scrub ([`crate::btrfs::scrub::scrub_dev_tree`]) uses:
    /// it drives reads off the DEV_TREE, which is keyed by physical offset,
    /// so the logical→physical lookup has already been done by the caller.
    ///
    /// `devid` must match the device this reader was opened for (each
    /// NonRAID slot is a single-device filesystem, so there is exactly one
    /// valid devid).  If a devid was registered via
    /// [`FsReader::with_devid`], a mismatch is a hard error rather than a
    /// silent read of the wrong disk.  `base_offset` is still applied, so
    /// the same reader works for a bare image, an array partition, or a
    /// whole-disk raw rdev.
    pub fn read_physical(&mut self, devid: u64, phys: u64, n: usize) -> std::io::Result<Vec<u8>> {
        if let Some(expected) = self.devid
            && devid != expected
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("read_physical devid {devid} does not match opened device {expected}"),
            ));
        }
        read_at(&mut self.fp, self.base_offset + phys, n)
    }

    /// Read `n` raw bytes at physical offset `phys` (relative to the
    /// device's start, before `base_offset`), applying `base_offset` but
    /// **without** the devid guard that [`FsReader::read_physical`] enforces.
    ///
    /// Used by the DUP-mirror cross-check ([`crate::btrfs::open`]), which
    /// reads every mirror copy of a metadata node by its absolute physical
    /// stripe offset — the devid is always the opened device, so the guard
    /// would only add noise.  The bytes returned are the raw on-disk node
    /// (callers verify the header checksum themselves).
    pub fn read_physical_raw(&mut self, phys: u64, n: usize) -> std::io::Result<Vec<u8>> {
        read_at(&mut self.fp, self.base_offset + phys, n)
    }

    /// Borrow the checksum strategy (algorithm + sector size) this reader
    /// uses to verify metadata node headers.  `None` if the reader was
    /// constructed without a strategy (verification disabled).
    pub fn strategy(&self) -> Option<&CsumStrategy> {
        self.strategy.as_ref()
    }

    /// Result of [`FsReader::read_node`]: the parsed node plus a flag
    /// indicating whether *every* mirror copy of a mirrored (DUP/RAID1/…)
    /// metadata node failed header-checksum verification.
    ///
    /// `all_mirrors_failed` is `false` when at least one copy validated (or
    /// the chunk is single-stripe, where there is nothing to cross-check
    /// against).  It is `true` only when the node lives in a mirrored chunk
    /// and *none* of its copies passed header verification — i.e. the
    /// corruption is unrecoverable by the DUP cross-check and the returned
    /// node is the first copy (which will likely fail to parse or, worse,
    /// parse into garbage that silently drops a subtree).  Callers should
    /// count this as a [`crate::fs::ScrubStats::metadata_header_errors`]
    /// rather than swallowing it.
    ///
    /// Read and parse the B-tree node at logical address `logical`.
    ///
    /// If a [`CsumStrategy`] is set, the node's 32-byte header checksum is
    /// verified against the rest of the node (the same algorithm as the data
    /// csum, selected by `csum_type`).  For mirrored metadata chunks (DUP /
    /// RAID1 / RAID1C3 / RAID1C4) every stripe is a full copy, so we read
    /// *all* stripes via [`ChunkMap::lookup_stripes`] and prefer the first
    /// one whose header checksum validates — a corrupt copy (e.g. a flipped
    /// bit in one DUP mirror) is transparently skipped in favour of the good
    /// copy instead of silently poisoning the traversal.  If no copy
    /// validates, the first stripe's buffer is returned anyway (the parse
    /// will surface the corruption as a read/parse error downstream) so the
    /// failure mode degrades to the old behaviour rather than inventing
    /// data; `ReadNodeResult::all_mirrors_failed` is set so the caller can
    /// count it as a metadata-header error instead of letting it pass
    /// silently.
    ///
    /// `expected_generation` is the generation the caller (the parent node's
    /// key pointer, or the superblock for a tree root) asserts this child
    /// must have.  Pass [`GEN_DONT_CHECK`] when the caller does not know the
    /// expected generation (e.g. a tree root reached directly from the
    /// superblock).  `expected_level` / `expected_owner` are the parent's
    /// `level - 1` and `owner` (tree id); pass `None` when unknown (the
    /// root).  All three are cross-checked against the child header in
    /// [`FsReader::validate_header`], alongside the always-known `bytenr`
    /// and `fsid`.
    ///
    /// With `strategy: None` the reader trusts the first stripe, exactly as
    /// before verification was added, and `all_mirrors_failed` is `false`.
    pub fn read_node(
        &mut self,
        chunk_map: &ChunkMap,
        logical: u64,
        expected_generation: u64,
        expected_level: Option<u8>,
        expected_owner: Option<u64>,
    ) -> std::io::Result<ReadNodeResult> {
        let strategy = match &self.strategy {
            Some(s) => s,
            None => {
                let buf = self.read_logical(chunk_map, logical, self.node_size)?;
                return Ok(ReadNodeResult {
                    node: Some(super::node::Node::parse(buf)),
                    all_mirrors_failed: false,
                    generation_mismatch: false,
                    mirror_mismatch: false,
                });
            }
        };

        // Gather every mirror copy of this node (one entry for single-stripe
        // chunks; two+ for DUP/RAID1/…).  Three-tier priority for the buffer
        // to return, mirroring the kernel's mirror cross-check:
        //   1. `good`   — csum-valid AND generation matches (best)
        //   2. `stale`  — csum-valid but generation differs (freed/reused
        //                 block; still readable, preferred over corrupt)
        //   3. `corrupt`— csum-invalid (last-resort fallback)
        let stripes = chunk_map
            .lookup_stripes(logical)
            .unwrap_or_else(|| vec![(0u64, 0u64)]);

        let mut good: Option<Vec<u8>> = None;
        let mut stale: Option<Vec<u8>> = None;
        let mut corrupt: Option<Vec<u8>> = None;
        // Count how many stripes passed header-csum verification.  Used to
        // detect a diverged mirror (≥1 valid but not all valid) — the
        // self-heal-recoverable case we must *report*, not silently heal.
        // IMPORTANT: we must inspect *every* stripe before deciding, not
        // `break` after the first good copy — otherwise `valid_count` would
        // only ever reach 1 on a clean DUP node and we'd falsely report a
        // mirror mismatch on every block.  So we walk all stripes, counting
        // valid copies and recording the best buffer, without early exit.
        let mut valid_count: usize = 0;
        for (_devid, phys) in &stripes {
            let buf = match read_at(&mut self.fp, self.base_offset + phys, self.node_size) {
                Ok(b) => b,
                Err(_) => continue,
            };
            if !strategy.verify_node_header(&buf) {
                // Corrupt copy — keep only as last-resort fallback.
                if corrupt.is_none() {
                    corrupt = Some(buf);
                }
                continue;
            }
            valid_count += 1;
            // csum valid — now check the header fields (bytenr/fsid/level/
            // owner/generation).  A copy that fails these is treated like a
            // corrupt copy: it is not the block the parent expected.
            let hdr = super::node::Header::parse(&buf);
            if !self.validate_header(
                &hdr,
                logical,
                expected_generation,
                expected_level,
                expected_owner,
            ) {
                if stale.is_none() {
                    stale = Some(buf);
                }
                continue;
            }
            // A good copy: prefer the first one we find (deterministic), but
            // keep scanning the remaining stripes so `valid_count` reflects
            // the whole mirror set.
            if good.is_none() {
                good = Some(buf);
            }
        }
        let generation_mismatch = good.is_none() && stale.is_some();
        // A mirrored node whose copies disagree: at least one stripe is
        // header-valid (so the block is readable) but not *every* stripe
        // validated.  Single-stripe chunks (stripes.len() == 1) can never
        // diverge, so this is only meaningful for mirrored chunks.
        let mirror_mismatch = stripes.len() > 1 && valid_count > 0 && valid_count < stripes.len();
        let buf = match good.or(stale).or(corrupt) {
            Some(b) => b,
            None => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("could not read any stripe for node at logical 0x{logical:x}"),
                ));
            }
        };
        // `all_mirrors_failed` reflects whether the buffer we are about to
        // hand back actually verifies.  For a mirrored (DUP/RAID1/…) chunk
        // the cross-check above already preferred a good copy when one
        // existed, so this is `false` whenever *any* copy validated.  For a
        // single-stripe chunk there is only one copy, and if *its* header
        // does not verify we cannot trust it either — so this is `true` and
        // the caller aborts the walk (we cannot trust a node whose checksum
        // we cannot verify).  The `strategy: None` path never reaches here,
        // so verification is only enforced when we actually have a strategy.
        let all_mirrors_failed = !strategy.verify_node_header(&buf);
        if all_mirrors_failed {
            eprintln!(
                "metadata header csum mismatch at logical 0x{logical:x} \
                 (no verifiable copy; {} mirror(s) read)",
                stripes.len()
            );
            // Do NOT parse the corrupt buffer: its `nritems`/slot offsets are
            // unverified and `Node::parse`'s unchecked indexing would panic
            // on garbage.  The caller (`walk_leaves`) only reads `node` on
            // the `!all_mirrors_failed` path, so leaving it `None` is safe
            // and turns a would-be crash into a clean "skip this branch".
            return Ok(ReadNodeResult {
                node: None,
                all_mirrors_failed,
                generation_mismatch,
                mirror_mismatch,
            });
        }
        Ok(ReadNodeResult {
            node: Some(super::node::Node::parse(buf)),
            all_mirrors_failed,
            generation_mismatch,
            mirror_mismatch,
        })
    }

    /// Cross-check a parsed node header against what the caller expects,
    /// mirroring the kernel's `btrfs_check_node` / `btrfs_check_leaf`.
    ///
    /// This is the defence against following a metadata block that was freed
    /// and reused by a later transaction (a TOCTOU race on a live mounted
    /// filesystem).  A valid header checksum only proves the block is
    /// *internally consistent* — it does NOT prove the block is the one the
    /// parent expected.  We therefore verify, in addition to the checksum:
    ///
    /// * `bytenr`  — must equal the logical address we read it from.  A
    ///   mismatch means the block belongs to a different bytenr (e.g. a
    ///   stale copy from a different tree location).
    /// * `fsid`    — must equal the filesystem's `fsid` (taken from the
    ///   superblock).  Catches blocks from a different filesystem / a
    ///   misdirected read.
    /// * `level`   — when `expected_level` is known (i.e. `Some`), must
    ///   equal `parent_level - 1`.  A mismatch means the block is not the
    ///   child of the node we descended from.
    /// * `owner`   — when `expected_owner` is known (i.e. `Some`), must
    ///   equal the parent's `owner`.  Catches a block from a different tree
    ///   (e.g. a CSUM_TREE block reached while descending the EXTENT_TREE).
    /// * `generation` — when `expected_generation != GEN_DONT_CHECK`, must
    ///   equal it.  This is the core stale-block check: a freed/reused block
    ///   carries a later generation than the parent's key pointer asserts.
    ///
    /// Returns `true` iff every known expectation is satisfied.  `bytenr`
    /// and `fsid` are always checked (they are always known); `level`,
    /// `owner`, and `generation` are checked only when their expected value
    /// is supplied.  The caller passes `GEN_DONT_CHECK` for the generation
    /// when it does not yet know it (tree roots reached from the superblock).
    pub fn validate_header(
        &self,
        hdr: &super::node::Header,
        logical: u64,
        expected_generation: u64,
        expected_level: Option<u8>,
        expected_owner: Option<u64>,
    ) -> bool {
        // bytenr must match the address we read this block from.
        if hdr.bytenr != logical {
            return false;
        }
        // fsid must match the filesystem we opened.  The superblock's fsid
        // is carried on the reader via the strategy-independent path; we
        // stash it on the reader at construction time (see `with_fsid`).
        if let Some(fsid) = self.fsid
            && hdr.fsid != fsid
        {
            return false;
        }
        // level must be exactly one below the parent's level.
        if let Some(lvl) = expected_level
            && hdr.level != lvl
        {
            return false;
        }
        // owner (tree id) must match the parent's owner.
        if let Some(owner) = expected_owner
            && hdr.owner != owner
        {
            return false;
        }
        // generation check (the core stale-block guard).
        if expected_generation != GEN_DONT_CHECK && hdr.generation != expected_generation {
            return false;
        }
        true
    }
}
