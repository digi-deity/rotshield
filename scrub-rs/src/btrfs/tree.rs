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
pub fn walk_leaves<F>(
    reader: &mut FsReader,
    chunk_map: &ChunkMap,
    root_logical: u64,
    mut f: F,
) -> std::io::Result<()>
where
    F: FnMut(&mut FsReader, &Leaf, u64) -> std::io::Result<()>,
{
    // BFS queue of (logical) addresses to visit.  We use a Vec as a deque.
    let mut queue: Vec<u64> = vec![root_logical];
    while let Some(logical) = queue.pop() {
        let node = reader.read_node(chunk_map, logical)?;
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