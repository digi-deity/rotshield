//! Live-stripe parity computation for NonRAID/Unraid arrays.
//!
//! Reused parity syndrome math that previously lived duplicated in three
//! places: `bin/craft_corrupt.rs` (its own `compute_p`/`compute_q` over raw
//! rdev reads), `recovery::engine::tests` (synthetic `compute_p`/
//! `compute_q` over fabricated slices), and the kernel driver
//! (`nonraid/raid6/int.uc`).  Owning it here means there is one
//! implementation per concern:
//!
//! - this module: P/Q over the *live array disks*, doing raw-rdev reads via
//!   [`stripe::read_block_or_zeros`];
//! - the pure [`crate::recovery`] engine: P/Q *reconstruction* of a missing
//!   block from already-gathered chunks;
//! - the engine tests: call into here to compute reference syndromes for
//!   ``fabricate → corrupt → recover`` round-trips, instead of carrying
//!   their own copy.
//!
//! Imports nothing from `btrfs/` or `recovery/` — GF math lives in
//! [`crate::recovery::gf`], which this module *does* depend on (GF tables
//! are pure arithmetic, shared across the array and recovery duties;
//! `btrfs/` is the only module this boundary avoids).

use std::io;

use crate::array::config::ArrayConfig;
use crate::array::stripe;
use crate::recovery::gf;

/// Compute P = XOR of all data disks at `array_phys` (block_size bytes).
///
/// Reads each data disk's `block_size`-byte chunk at `array_phys`, in
/// raw-rdev space (the per-disk `rdevOffset` is added internally by
/// [`stripe::read_block_or_zeros`]), zero-substituting past a smaller
/// disk's end — the same convention [`stripe::gather_stripe`] uses, so
/// the result matches the on-disk P disk bit-for-bit on a consistent
/// array.
pub fn compute_p(config: &ArrayConfig, array_phys: u64, block_size: usize) -> io::Result<Vec<u8>> {
    let mut p = vec![0u8; block_size];
    for path in config.data_devs.values() {
        let block = stripe::read_block_or_zeros(config, path, array_phys, block_size)?;
        xor_inplace(&mut p, &block);
    }
    Ok(p)
}

/// Compute P with one slot's on-disk block replaced by an in-memory
/// `override_block` (block_size bytes).  Used by `bin/craft_corrupt`'s
/// `bake-p` / `bake-both` paths, which recompute P/Q from the *corrupt*
/// data — the override is the corrupt block, and the result is what P
/// would be if the array driver had accepted the corrupt write and
/// resynced parity.  Reading the failing disk's still-uncorrupted bytes
/// from disk would defeat the test scenario.
pub fn compute_p_with_override(
    config: &ArrayConfig,
    override_slot: u64,
    override_block: &[u8],
    array_phys: u64,
    block_size: usize,
) -> io::Result<Vec<u8>> {
    let mut p = vec![0u8; block_size];
    for (slot, path) in &config.data_devs {
        let block: &[u8] = if *slot == override_slot {
            override_block
        } else {
            &stripe::read_block_or_zeros(config, path, array_phys, block_size)?
        };
        xor_inplace(&mut p, block);
    }
    Ok(p)
}

/// Compute the Q syndrome: `Q = XOR_{slot=1..n} g^(slot-1) · D_slot`
/// (block_size bytes).  Same read convention as [`compute_p`] — raw-rdev
/// reads via [`stripe::read_block_or_zeros`], zero-substitution past a
/// smaller disk's end.
pub fn compute_q(config: &ArrayConfig, array_phys: u64, block_size: usize) -> io::Result<Vec<u8>> {
    let mut q = vec![0u8; block_size];
    for (slot, path) in &config.data_devs {
        let coef = gf::gf_exp(*slot as i32 - 1);
        let block = stripe::read_block_or_zeros(config, path, array_phys, block_size)?;
        let table = &gf::GFMUL[coef as usize];
        for (q_byte, d_byte) in q.iter_mut().zip(block.iter()) {
            *q_byte ^= table[*d_byte as usize];
        }
    }
    Ok(q)
}

/// Compute Q with one slot's on-disk block replaced by an in-memory
/// `override_block` (block_size bytes).  Counterpart to
/// [`compute_p_with_override`]; used by `bake-q` / `bake-both` in
/// `bin/craft_corrupt`.
pub fn compute_q_with_override(
    config: &ArrayConfig,
    override_slot: u64,
    override_block: &[u8],
    array_phys: u64,
    block_size: usize,
) -> io::Result<Vec<u8>> {
    let mut q = vec![0u8; block_size];
    for (slot, path) in &config.data_devs {
        let coef = gf::gf_exp(*slot as i32 - 1);
        let block_owned;
        let block: &[u8] = if *slot == override_slot {
            override_block
        } else {
            block_owned = stripe::read_block_or_zeros(config, path, array_phys, block_size)?;
            &block_owned
        };
        let table = &gf::GFMUL[coef as usize];
        for (q_byte, d_byte) in q.iter_mut().zip(block.iter()) {
            *q_byte ^= table[*d_byte as usize];
        }
    }
    Ok(q)
}

/// XOR `b` into `a` in place.  Panics on length mismatch.
fn xor_inplace(a: &mut [u8], b: &[u8]) {
    debug_assert_eq!(a.len(), b.len(), "xor_inplace: length mismatch");
    for (x, y) in a.iter_mut().zip(b.iter()) {
        *x ^= y;
    }
}
