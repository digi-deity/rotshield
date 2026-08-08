//! Iterative walks over btrfs trees: visit every leaf (optionally pruned
//! to a key range), with metadata-failure callbacks.

use super::chunk::ChunkMap;
use super::key::Key;
use super::node::{Leaf, Node};
use super::reader::FsReader;

/// A child pointer awaiting descent, with the header fields it must match.
struct NodeRef {
    logical: u64,

    /// Expected generation from the parent's key pointer.
    exp_gen: Option<u64>,

    exp_level: Option<u8>,

    exp_owner: Option<u64>,
}

/// Visit every leaf of the tree rooted at `root_logical`, calling `f` per
/// leaf. Failures are reported via the callbacks, which decide what to
/// count: on_metadata_error (no valid copy), on_stale (freed/repurposed
/// node), on_mirror_mismatch (copies disagreed), on_read_error (EIO).
#[allow(clippy::too_many_arguments)]
pub fn walk_leaves<F, E, S, M, R>(
    reader: &mut FsReader,
    chunk_map: &ChunkMap,
    root_logical: u64,
    f: F,
    on_metadata_error: E,
    on_stale: S,
    on_mirror_mismatch: M,
    on_read_error: R,
) -> std::io::Result<()>
where
    F: FnMut(&mut FsReader, &Leaf, u64) -> std::io::Result<()>,
    E: FnMut(u64),
    S: FnMut(u64),
    M: FnMut(u64),
    R: FnMut(u64),
{
    walk_leaves_impl(
        reader,
        chunk_map,
        root_logical,
        None,
        f,
        on_metadata_error,
        on_stale,
        on_mirror_mismatch,
        on_read_error,
    )
}

/// Like walk_leaves, but prunes any subtree whose key range cannot overlap
/// `[key_lo, key_hi)`.
#[allow(clippy::too_many_arguments)]
pub fn walk_leaves_range<F, E, S, M, R>(
    reader: &mut FsReader,
    chunk_map: &ChunkMap,
    root_logical: u64,
    key_lo: Key,
    key_hi: Key,
    f: F,
    on_metadata_error: E,
    on_stale: S,
    on_mirror_mismatch: M,
    on_read_error: R,
) -> std::io::Result<()>
where
    F: FnMut(&mut FsReader, &Leaf, u64) -> std::io::Result<()>,
    E: FnMut(u64),
    S: FnMut(u64),
    M: FnMut(u64),
    R: FnMut(u64),
{
    walk_leaves_impl(
        reader,
        chunk_map,
        root_logical,
        Some((key_lo, key_hi)),
        f,
        on_metadata_error,
        on_stale,
        on_mirror_mismatch,
        on_read_error,
    )
}

#[allow(clippy::too_many_arguments)]
fn walk_leaves_impl<F, E, S, M, R>(
    reader: &mut FsReader,
    chunk_map: &ChunkMap,
    root_logical: u64,
    bounds: Option<(Key, Key)>,
    mut f: F,
    mut on_metadata_error: E,
    mut on_stale: S,
    mut on_mirror_mismatch: M,
    mut on_read_error: R,
) -> std::io::Result<()>
where
    F: FnMut(&mut FsReader, &Leaf, u64) -> std::io::Result<()>,
    E: FnMut(u64),
    S: FnMut(u64),
    M: FnMut(u64),
    R: FnMut(u64),
{
    // Iterative descent: the queue holds child pointers with the header
    // fields they must match.
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
        let expected_generation = exp_gen.unwrap_or(super::reader::GEN_DONT_CHECK);
        let res = match reader.read_node(
            chunk_map,
            logical,
            expected_generation,
            exp_level,
            exp_owner,
        ) {
            Ok(r) => r,
            // Read (EIO) failure: the subtree is unreadable; the
            // callback records it and the walk moves on.
            Err(_e) => {
                on_read_error(logical);
                continue;
            }
        };
        // No copy passed header verification: metadata error; the
        // subtree is skipped.
        if res.all_mirrors_failed {
            on_metadata_error(logical);
            continue;
        }
        // Only stale copies: the node was freed and repurposed by a
        // live transaction — skip it.
        if res.generation_mismatch {
            on_stale(logical);
            continue;
        }

        // Copies disagreed but a good one was read: report, keep walking.
        if res.mirror_mismatch {
            on_mirror_mismatch(logical);
        }
        let node = res.node.unwrap();
        match node {
            Node::Leaf(leaf) => f(reader, &leaf, logical)?,
            Node::Internal(internal) => {
                let parent_level = internal.header.level;
                let parent_owner = internal.header.owner;

                let n = internal.ptrs.len();
                for (idx, ptr) in internal.ptrs.iter().enumerate().rev() {
                    // Prune children whose key range lies entirely outside
                    // [key_lo, key_hi).
                    if let Some((key_lo, key_hi)) = bounds {
                        let child_lo = ptr.key;
                        let child_hi = internal.ptrs.get(idx + 1).map(|p| p.key);
                        let below = child_hi.is_some_and(|hi| hi <= key_lo);
                        let above = child_lo >= key_hi;
                        if below || above {
                            continue;
                        }
                    }

                    // Prefetch the upcoming siblings while the current
                    // read is still in flight.
                    if idx < 2 || n <= 4 {
                        reader.prefetch_logical(chunk_map, ptr.blockptr, reader.node_size());
                    }
                    // Children inherit level-1 and owner from the parent;
                    // the generation comes from the key pointer.
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
