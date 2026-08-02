//! Early environment/config canary for the array — glue that composes the
//! array duty (gather) with the recovery duty (P-only reconstruction) and
//! leaves "what does good look like?" to the caller's filesystem probe.
//!
//! Before committing to a full scrub + recovery pass, we want a cheap,
//! early signal that the array is wired correctly: that the parity disk
//! and the *other* data disks can actually reconstruct a known block on
//! the disk we're about to scrub.  If they can't, we're looking at a
//! misconfigured array (wrong slot, stale/out-of-sync parity, offset
//! mismatch) and any recovery we attempt later would be built on sand.
//!
//! This module is deliberately **filesystem-agnostic**: it reconstructs raw
//! bytes from parity and returns them.  The caller (main) decides what
//! "good" means — e.g. checking for a btrfs magic via the filesystem's
//! `block_has_magic` probe — so no btrfs knowledge leaks in here.  It is
//! glue by design: it composes [`crate::array::stripe::gather_stripe`]
//! (the array duty) with [`crate::recovery::engine::recover_via_p`] (the
//! pure parity math), the same two pieces the real recovery path uses, so
//! the canary exercises the exact machinery recovery depends on.

use std::io;

use crate::array::config::ArrayConfig;
use crate::array::stripe;
use crate::recovery::engine::recover_via_p;

/// Reconstruct the `block_size`-byte block at `array_phys` on `failing_slot`
/// purely from the other data disks + primary parity, returning the bytes.
///
/// `array_phys` is in **array-partition space** (the byte offset on
/// `/dev/nmdNp1`), exactly as [`stripe::gather_stripe`] expects — the
/// per-disk `rdevOffset` is added internally.  The reconstruction is a
/// plain P-only XOR (`D_k = P ⊕ ⊕_{j≠k} D_j`); it needs no checksum
/// verifier because the caller validates the result by its own means
/// (e.g. a magic-number check).  Returns an error if the stripe can't be
/// gathered (a disk missing/unreadable) or if no primary parity is present
/// (the caller should already have loaded a config that guarantees parity,
/// but we fail loudly rather than silently producing a zero block).
///
/// The canonical use is the startup canary: pass the target disk's
/// superblock offset + a 4096-byte block size, then have the caller check
/// the returned bytes for a filesystem magic.  A match proves the array
/// config, slot, and parity are all consistent before the real scrub runs.
pub fn reconstruct_block(
    config: &ArrayConfig,
    failing_slot: u64,
    array_phys: u64,
    block_size: usize,
) -> io::Result<Vec<u8>> {
    let chunks = stripe::gather_stripe(config, failing_slot, array_phys, block_size)?;
    let p_block = chunks.p_block.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "no primary parity disk present; cannot run parity canary",
        )
    })?;
    // `recover_via_p` asserts the corrupt block's length for sanity; we
    // pass a zero block of the right size since we have no on-disk copy to
    // hand it (and the corrupt block is unused in the P-only math anyway).
    Ok(recover_via_p(
        &vec![0u8; block_size],
        &chunks.other_data,
        &p_block,
        block_size,
    ))
}
