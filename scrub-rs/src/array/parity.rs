//! P/Q syndrome computation over the array's data disks (used by the
//! corruption-crafting tool).

use std::io;

use crate::array::config::ArrayConfig;
use crate::array::stripe;
use crate::recovery::gf;

/// P = XOR of every data disk's block at `array_phys`.
pub fn compute_p(config: &ArrayConfig, array_phys: u64, block_size: usize) -> io::Result<Vec<u8>> {
    let mut p = vec![0u8; block_size];
    for path in config.data_devs.values() {
        let block = stripe::read_block_or_zeros(config, path, array_phys, block_size)?;
        xor_inplace(&mut p, &block);
    }
    Ok(p)
}

/// P with one slot's block replaced by `override_block` (simulating a
/// corrupted disk).
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

/// Q = XOR over slots of g^(slot-1) · D_slot at `array_phys` (GF(2^8)).
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

/// Q with one slot's block replaced by `override_block`.
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

/// XOR `b` into `a` in place (lengths must match; asserted in debug builds).
fn xor_inplace(a: &mut [u8], b: &[u8]) {
    debug_assert_eq!(a.len(), b.len(), "xor_inplace: length mismatch");
    for (x, y) in a.iter_mut().zip(b.iter()) {
        *x ^= y;
    }
}
