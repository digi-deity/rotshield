//! Startup array probe: reconstruct a known block from parity and let the
//! caller verify it, to catch a miswired array before recovery.

use std::io;

use crate::array::config::ArrayConfig;
use crate::array::stripe;
use crate::recovery::engine::recover_via_p;

/// Reconstruct the `block_size` bytes at `array_phys` on `failing_slot` from
/// the other data disks plus primary parity (P-only XOR). The caller decides
/// whether the result is correct (e.g. byte-compare against the expected
/// superblock block).
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

    // The corrupt argument is unused by the P-only math; pass zeros.
    Ok(recover_via_p(
        &vec![0u8; block_size],
        &chunks.other_data,
        &p_block,
        block_size,
    ))
}
