//! Generic btrfs B-tree walking.
//!
//! Mirrors `btrfs_recon/parsing.py::walk_btree` / `walk_leaves`: a BFS over
//! the tree that yields every leaf node, in tree order, starting from a root
//! logical address.  Callers inspect each leaf's slots to find items of
//! interest.

use super::chunk::ChunkMap;
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
pub fn walk_leaves<F, E>(
    reader: &mut FsReader,
    chunk_map: &ChunkMap,
    root_logical: u64,
    mut f: F,
    mut on_metadata_error: E,
) -> std::io::Result<()>
where
    F: FnMut(&mut FsReader, &Leaf, u64) -> std::io::Result<()>,
    E: FnMut(u64),
{
    // BFS queue of (logical) addresses to visit.  We use a Vec as a deque.
    let mut queue: Vec<u64> = vec![root_logical];
    while let Some(logical) = queue.pop() {
        let res = reader.read_node(chunk_map, logical)?;
        if res.all_mirrors_failed {
            // We cannot verify this node's header against any mirror copy,
            // so we cannot trust it.  Skip this branch: do NOT descend into
            // its children (an internal node would otherwise silently drop a
            // subtree) and do NOT hand its items to the caller (a leaf would
            // feed garbage).  The DUP cross-check already preferred a good
            // copy if one existed, so reaching here means this branch is
            // untrustworthy.  Report the error and continue with the rest of
            // the queue so the other, still-reachable branches are still
            // scrubbed.
            on_metadata_error(logical);
            continue;
        }
        let node = res.node;
        match node {
            Node::Leaf(leaf) => f(reader, &leaf, logical)?,
            Node::Internal(internal) => {
                // Push in reverse so pop() visits children in tree order.
                for ptr in internal.ptrs.iter().rev() {
                    queue.push(ptr.blockptr);
                }
            }
        }
    }
    Ok(())
}