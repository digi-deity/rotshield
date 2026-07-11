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

/// Result of [`FsReader::read_node`]: the parsed node plus a flag indicating
/// whether *every* mirror copy of a mirrored (DUP/RAID1/…) metadata node
/// failed header-checksum verification.
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
pub struct ReadNodeResult {
    pub node: Node,
    pub all_mirrors_failed: bool,
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
    /// The checksum strategy (algorithm + sector size) taken from the
    /// superblock.  Used to verify every metadata node/leaf header
    /// checksum on read, and — for mirrored (DUP/RAID1/…) metadata — to
    /// cross-check the copies and prefer a good one over a corrupt header.
    /// `None` means "no verification" (e.g. a caller that doesn't have a
    /// strategy yet); the reader then behaves as before, trusting the
    /// first copy it reads.
    strategy: Option<CsumStrategy>,
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
    pub fn new(fp: File, node_size: usize, base_offset: u64, strategy: Option<CsumStrategy>) -> Self {
        Self { fp, node_size, base_offset, strategy }
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
    /// With `strategy: None` the reader trusts the first stripe, exactly as
    /// before verification was added, and `all_mirrors_failed` is `false`.
    pub fn read_node(
        &mut self,
        chunk_map: &ChunkMap,
        logical: u64,
    ) -> std::io::Result<ReadNodeResult> {
        let strategy = match &self.strategy {
            Some(s) => s,
            None => {
                let buf = self.read_logical(chunk_map, logical, self.node_size)?;
                return Ok(ReadNodeResult {
                    node: super::node::Node::parse(buf),
                    all_mirrors_failed: false,
                });
            }
        };

        // Gather every mirror copy of this node (one entry for single-stripe
        // chunks; two+ for DUP/RAID1/…).
        let stripes = chunk_map
            .lookup_stripes(logical)
            .unwrap_or_else(|| vec![(0u64, 0u64)]);
        let mut good: Option<Vec<u8>> = None;
        for (_devid, phys) in &stripes {
            let buf = match read_at(&mut self.fp, self.base_offset + phys, self.node_size) {
                Ok(b) => b,
                Err(_) => continue,
            };
            if strategy.verify_node_header(&buf) {
                good = Some(buf);
                break;
            }
            // Remember the first copy as a fallback if none validate.
            if good.is_none() {
                good = Some(buf);
            }
        }
        let buf = good.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("could not read any stripe for node at logical 0x{logical:x}"),
            )
        })?;
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
        }
        Ok(ReadNodeResult {
            node: super::node::Node::parse(buf),
            all_mirrors_failed,
        })
    }
}