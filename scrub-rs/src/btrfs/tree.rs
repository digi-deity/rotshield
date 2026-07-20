//! Generic btrfs B-tree walking.
//!
//! Mirrors `btrfs_recon/parsing.py::walk_btree` / `walk_leaves`: a BFS over
//! the tree that yields every leaf node, in tree order, starting from a root
//! logical address.  Callers inspect each leaf's slots to find items of
//! interest.

use super::chunk::ChunkMap;
use super::key::Key;
use super::node::{Leaf, Node};
use super::reader::FsReader;

/// Walk a btrfs B-tree from `root_logical`, calling `f` for every leaf.
///
/// `chunk_map` is passed through to every `read_node` call — the chunk map
/// is immutable after the chunk-tree walk and shared by reference so callers
/// can also hold a `&ChunkMap` independently (e.g. for inline recovery from
/// the scrub callback).
///
/// Stops early if `f` returns `Err`.  The closure receives the parsed leaf
/// and its logical address (handy for computing absolute item-data offsets
/// when the leaf's `buf` is sliced relative to the node start).
///
/// `on_metadata_error` is invoked once for every node whose *all* mirror
/// copies failed header-checksum verification (a mirrored/DUP node with no
/// good copy).  This lets the caller count unrecoverable metadata-header
/// corruption (e.g. fold it into [`crate::fs::ScrubStats::metadata_header_errors`])
/// instead of letting it pass silently.  It is only called when the node
/// could not be recovered via the DUP cross-check; a single corrupt copy
/// that has a good sibling is transparently skipped and never reported.
///
/// `on_mirror_mismatch` is invoked for every *mirrored* (DUP/RAID1/…) node
/// whose copies **disagree** — at least one mirror is header-valid (so the
/// block is still readable / self-healable) but not *every* mirror validated
/// (a copy is corrupt).  This is the self-heal-recoverable counterpart to
/// `on_metadata_error`: the filesystem can read the good copy, but a correct
/// scrub should *report* the divergence (as the kernel's `btrfs scrub`
/// does) rather than healing it silently.  It fires only for nodes that are
/// otherwise trustworthy (the walk descends into them normally); the
/// divergence is surfaced via this callback so the caller can count it as a
/// [`crate::fs::ScrubStats::metadata_mirror_mismatches`] without disturbing
/// traversal.
///
/// **Abort-on-unverifiable-node (per branch):** when a node's header
/// checksum cannot be verified against *any* mirror copy, that **branch**
/// of the tree is aborted — the node is skipped entirely (we do not descend
/// into its children, nor hand its items to the caller) — but the walk
/// **continues** with the rest of the queue.  We cannot trust a node whose
/// checksum we cannot verify: a corrupt internal node would otherwise
/// silently drop a whole subtree, and a corrupt leaf would feed garbage
/// items to the caller.  The DUP/RAID1 cross-check in
/// [`super::reader::FsReader::read_node`] already prefers a good copy when
/// one exists, so this skip is reached only when *no* good copy is
/// available.  The error is reported via `on_metadata_error` (so it
/// surfaces in the log / stats) and the other, still-trustworthy branches
/// of the tree are scrubbed normally.
/// One entry on the BFS descent queue: the logical address to read, plus the
/// expectations the *parent* node placed on it.  Carrying these lets us
/// detect a metadata block that was freed and reused by a later transaction
/// (a TOCTOU race on a live mounted filesystem) — something a header
/// checksum alone cannot catch.
struct NodeRef {
    /// Logical address of the child node.
    logical: u64,
    /// Expected header `generation` (from the parent's key pointer).  `None`
    /// only for the tree root, whose generation the caller does not know.
    exp_gen: Option<u64>,
    /// Expected header `level` (parent's `level - 1`).  `None` for the root.
    exp_level: Option<u8>,
    /// Expected header `owner` (the parent's `owner` / tree id).  `None` for
    /// the root.
    exp_owner: Option<u64>,
}

pub fn walk_leaves<F, E, M>(
    reader: &mut FsReader,
    chunk_map: &ChunkMap,
    root_logical: u64,
    f: F,
    on_metadata_error: E,
    on_mirror_mismatch: M,
) -> std::io::Result<()>
where
    F: FnMut(&mut FsReader, &Leaf, u64) -> std::io::Result<()>,
    E: FnMut(u64),
    M: FnMut(u64),
{
    walk_leaves_impl(
        reader,
        chunk_map,
        root_logical,
        None,
        f,
        on_metadata_error,
        on_mirror_mismatch,
    )
}

/// Like [`walk_leaves`], but prunes any subtree whose key range cannot
/// overlap the half-open window `[key_lo, key_hi)`.
///
/// Internal nodes store key pointers in ascending key order (a btrfs
/// invariant), so child `i`'s key range is `[ptrs[i].key, ptrs[i+1].key)`
/// (or `[ptrs[i].key, +inf)` for the last child). A child is skipped —
/// not queued, not read, not prefetched — when its range cannot possibly
/// overlap `[key_lo, key_hi)`.
///
/// This exists because a naive `walk_leaves` call re-walks the **entire**
/// tree from the root every time — fine for a once-per-open walk (CHUNK/
/// ROOT/DEV trees), but ruinous for [`crate::btrfs::csum::LazyCsumProvider::range`],
/// which is called once per dev-extent: on a fragmented filesystem with N
/// dev-extents, an unbounded walk costs O(N × tree_size) — the entire
/// CSUM_TREE gets re-read and re-parsed N times, dwarfing the actual data
/// read/checksum work and pinning a single CPU core in tree-parsing for
/// the whole scrub. Bounding the descent to the requested key window
/// turns each call into O(range_size + log(tree_size)), independent of N.
#[allow(clippy::too_many_arguments)]
pub fn walk_leaves_range<F, E, M>(
    reader: &mut FsReader,
    chunk_map: &ChunkMap,
    root_logical: u64,
    key_lo: Key,
    key_hi: Key,
    f: F,
    on_metadata_error: E,
    on_mirror_mismatch: M,
) -> std::io::Result<()>
where
    F: FnMut(&mut FsReader, &Leaf, u64) -> std::io::Result<()>,
    E: FnMut(u64),
    M: FnMut(u64),
{
    walk_leaves_impl(
        reader,
        chunk_map,
        root_logical,
        Some((key_lo, key_hi)),
        f,
        on_metadata_error,
        on_mirror_mismatch,
    )
}

fn walk_leaves_impl<F, E, M>(
    reader: &mut FsReader,
    chunk_map: &ChunkMap,
    root_logical: u64,
    bounds: Option<(Key, Key)>,
    mut f: F,
    mut on_metadata_error: E,
    mut on_mirror_mismatch: M,
) -> std::io::Result<()>
where
    F: FnMut(&mut FsReader, &Leaf, u64) -> std::io::Result<()>,
    E: FnMut(u64),
    M: FnMut(u64),
{
    // BFS queue of (logical, expectation) pairs to visit.  We use a Vec as
    // a deque.  The root has no parent, so its expectations are unknown.
    let mut queue: Vec<NodeRef> = vec![NodeRef {
        logical: root_logical,
        exp_gen: None,
        exp_level: None,
        exp_owner: None,
    }];
    while let Some(NodeRef {
        logical,
        exp_gen,
        exp_level,
        exp_owner,
    }) = queue.pop()
    {
        // The root's generation is unknown to the caller; for every other
        // node we pass the parent's expected generation so `read_node` can
        // reject a stale (freed/reused) block.
        let expected_generation = exp_gen.unwrap_or(super::reader::GEN_DONT_CHECK);
        let res = reader.read_node(
            chunk_map,
            logical,
            expected_generation,
            exp_level,
            exp_owner,
        )?;
        if res.all_mirrors_failed || res.generation_mismatch {
            // We cannot verify this node's header against any mirror copy,
            // or the only verifiable copy is stale (generation mismatch) — in
            // either case we cannot trust it.  Skip this branch: do NOT
            // descend into its children (an internal node would otherwise
            // silently drop a subtree) and do NOT hand its items to the
            // caller (a leaf would feed garbage).  The DUP cross-check already
            // preferred a good copy when one existed, so reaching here means
            // this branch is untrustworthy.  Report the error and continue
            // with the rest of the queue so the other, still-reachable
            // branches are still scrubbed.
            on_metadata_error(logical);
            continue;
        }
        // The node is trustworthy (a good copy exists).  If its mirror
        // copies disagree (≥1 valid but not all valid), report the
        // divergence as a self-heal-recoverable mirror mismatch — the
        // filesystem can still read the good copy, but the divergence should
        // surface rather than be silently healed by the DUP cross-check.
        if res.mirror_mismatch {
            on_mirror_mismatch(logical);
        }
        let node = res.node.unwrap();
        match node {
            Node::Leaf(leaf) => f(reader, &leaf, logical)?,
            Node::Internal(internal) => {
                let parent_level = internal.header.level;
                let parent_owner = internal.header.owner;
                // Push in reverse so pop() visits children in tree order.
                // Each child inherits the parent's expectation: its
                // generation comes from the parent's key pointer, its level
                // must be one below the parent's, and its owner must match
                // the parent's tree id.
                //
                // Pre-issue `POSIX_FADV_WILLNEED` for every child as it is
                // queued, so the disk starts prefetching the next-to-visit
                // metadata block while we are still processing *this*
                // internal node's siblings.  On HDD the bulk-data sweep is
                // already sequential (the DEV-tree-driven walk + the
                // `POSIX_FADV_SEQUENTIAL` hint cover it); the seek-dominated
                // part of a multi-TB scrub's idle time is these scattered
                // metadata reads.  Each `WILLNEED` is a single `nodesize`
                // block (≤64 KiB), well inside the kernel's 1-MiB hint cap.
                // For DUP / RAID1 chunks `prefetch_logical` issues a hint
                // for every stripe, so the mirror cross-check in
                // `read_node` later has both copies ready by the time it
                // asks.  Hints are advisory: ignored where unsupported and
                // simply wasted where the chunk map can't resolve `logical`
                // (the subsequent `read_node` will fail loudly anyway).
                let n = internal.ptrs.len();
                for (idx, ptr) in internal.ptrs.iter().enumerate().rev() {
                    // Bounded walk: skip any child whose key range cannot
                    // overlap the requested `[key_lo, key_hi)` window.
                    // Children are stored in ascending key order, so child
                    // `idx`'s range is `[ptr.key, next_ptr.key)` (or
                    // `[ptr.key, +inf)` for the last child).
                    if let Some((key_lo, key_hi)) = bounds {
                        let child_lo = ptr.key;
                        let child_hi = internal.ptrs.get(idx + 1).map(|p| p.key);
                        let below = child_hi.is_some_and(|hi| hi <= key_lo);
                        let above = child_lo >= key_hi;
                        if below || above {
                            continue;
                        }
                    }
                    // Prefetch only the *next* one or two children — not
                    // the whole level, which would queue tens of MiB of
                    // hints on a wide metadata leaf before we even get to
                    // visit them (and could blow the kernel's readahead
                    // window, evicting the cache for *this* walk's own
                    // earlier pages).  Two-deep ahead keeps the seek stall
                    // at most one node on HDD without spamming the disk.
                    if idx < 2 || n <= 4 {
                        reader.prefetch_logical(chunk_map, ptr.blockptr, reader.node_size());
                    }
                    queue.push(NodeRef {
                        logical: ptr.blockptr,
                        exp_gen: Some(ptr.generation),
                        exp_level: Some(parent_level.saturating_sub(1)),
                        exp_owner: Some(parent_owner),
                    });
                }
            }
        }
    }
    Ok(())
}
