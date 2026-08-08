//! Pure parity-recovery engine: I/O-free and checksum-agnostic.
//!
//! Recover a corrupt block from one stripe's aligned chunks via P-only,
//! Q-only, then a P/Q two-disk solve; a verifier closure picks the first
//! candidate that checks out.

use crate::recovery::gf;
use crate::recovery::model::{FailureReason, ParityPath, RecoveryInput, RecoveryResult};

/// Result of the Q-only reconstruction attempt (internal).
enum QOutcome {
    /// Reconstruction passed the verifier.
    Recovered { block: Vec<u8> },
    /// Reconstruction equals the corrupt block: Q was recomputed from it.
    BakedIn,
    /// Reconstruction did not pass the verifier.
    CsumMismatch,
}

/// Recover one corrupt block, trying every parity path in order:
/// P-only, Q-only, then a P/Q two-disk solve against each other disk.
///
/// Returns `Recovered` for the first candidate the verifier accepts,
/// `NotCorrupt` if the failing block already verifies, or `Failed` with
/// the aggregated reasons. All slices in `input` must be exactly
/// `block_size` bytes; lengths are asserted where the math reads them.
pub fn recover_block(input: &RecoveryInput<'_>, block_size: usize) -> RecoveryResult {
    // Failing block already passes the verifier — the scrub raced a
    // rewrite; nothing to recover.
    let corrupt = input.corrupt_block;
    assert_eq!(corrupt.len(), block_size, "corrupt_block length mismatch");
    // With an unreadable source, `corrupt` is a zero placeholder, not the
    // on-disk bytes, so this early return and the baked-in detections are
    // meaningless and must be skipped.
    let unreadable = input.unreadable_source;
    if !unreadable && (input.verifier)(corrupt) {
        return RecoveryResult::NotCorrupt;
    }

    // P-only path.
    let p_reason: Option<FailureReason> = match input.p_block {
        None => Some(FailureReason::ParityAbsent { via: ParityPath::P }),
        Some(p_block) => {
            assert_eq!(p_block.len(), block_size, "p_block length mismatch");
            // If P XOR (corrupt XOR all other blocks) is all-zero, P was
            // recomputed from the corrupt bytes — it carries no new info.
            let mut zero_acc = corrupt.to_vec();
            for (_, b) in input.other_blocks {
                xor_inplace(&mut zero_acc, b);
            }
            xor_inplace(&mut zero_acc, p_block);
            if !unreadable && zero_acc.iter().all(|&x| x == 0) {
                Some(FailureReason::ParityBakedIn { via: ParityPath::P })
            } else {
                let candidate = recover_via_p(corrupt, input.other_blocks, p_block, block_size);
                if (input.verifier)(&candidate) {
                    return RecoveryResult::Recovered {
                        via: ParityPath::P,
                        block: candidate,
                    };
                }
                Some(FailureReason::CsumMismatch { via: ParityPath::P })
            }
        }
    };

    // Q-only path.
    let q_reason: Option<FailureReason> = match input.q_block {
        None => Some(FailureReason::ParityAbsent { via: ParityPath::Q }),
        Some(q_block) => {
            assert_eq!(q_block.len(), block_size, "q_block length mismatch");
            // Empty corrupt slice for unreadable sources so the baked-in
            // comparison inside recover_via_q can never fire on the
            // placeholder.
            let q_corrupt: &[u8] = if unreadable { &[] } else { corrupt };
            match recover_via_q(
                input.other_blocks,
                q_block,
                input.failing_slot,
                block_size,
                input.verifier,
                q_corrupt,
            ) {
                QOutcome::Recovered { block } => {
                    return RecoveryResult::Recovered {
                        via: ParityPath::Q,
                        block,
                    };
                }
                QOutcome::BakedIn => Some(FailureReason::ParityBakedIn { via: ParityPath::Q }),
                QOutcome::CsumMismatch => Some(FailureReason::CsumMismatch { via: ParityPath::Q }),
            }
        }
    };

    // Two-disk solve: both blocks unknown, so brute-force each other disk
    // as the partner; the verifier identifies the right one.
    let p_singleton = p_reason.clone().unwrap_or_else(|| {
        FailureReason::InternalInconsistency("internal: P path returned no reason".into())
    });
    let q_singleton = q_reason.clone().unwrap_or_else(|| {
        FailureReason::InternalInconsistency("internal: Q path returned no reason".into())
    });

    if let (Some(p_block), Some(q_block)) = (input.p_block, input.q_block) {
        let mut partners_tried: Vec<u64> = Vec::new();
        for (partner_slot, _) in input.other_blocks {
            partners_tried.push(*partner_slot);
            if let Some(candidate) = solve_two_disk(
                p_block,
                q_block,
                input.other_blocks,
                *partner_slot,
                input.failing_slot,
                block_size,
            ) && (input.verifier)(&candidate)
            {
                return RecoveryResult::Recovered {
                    via: ParityPath::PQ {
                        partner_slot: *partner_slot,
                    },
                    block: candidate,
                };
            }
            // Wrong partner — try the next one.
        }
        return RecoveryResult::Failed {
            reason: FailureReason::AllPathsFailed {
                p_reason: Box::new(p_singleton),
                q_reason: Box::new(q_singleton),
                pq_partners_tried: partners_tried,
            },
        };
    }

    // No Q disk (single-parity array): report the P failure directly
    // instead of a phantom Q result.
    RecoveryResult::Failed {
        reason: if input.q_block.is_none() {
            FailureReason::NoQPathAndPFailed {
                p_reason: Box::new(p_singleton),
            }
        } else {
            // P absent but Q present: PQ was never possible, so surface
            // both single-path reasons with an empty partner list.
            FailureReason::AllPathsFailed {
                p_reason: Box::new(p_singleton),
                q_reason: Box::new(q_singleton),
                pq_partners_tried: Vec::new(),
            }
        },
    }
}

/// Reconstruct one block via P-only XOR: D_k = P XOR XOR_{j != k} D_j.
/// All slices must be `block_size` bytes (asserted).
pub fn recover_via_p(
    _corrupt: &[u8],
    other_blocks: &[(u64, Vec<u8>)],
    p_block: &[u8],
    block_size: usize,
) -> Vec<u8> {
    assert_eq!(_corrupt.len(), block_size);
    assert_eq!(p_block.len(), block_size);
    let mut recovered = vec![0u8; block_size];
    for (_, b) in other_blocks {
        assert_eq!(b.len(), block_size);
        xor_inplace(&mut recovered, b);
    }
    xor_inplace(&mut recovered, p_block);
    recovered
}

// QOutcome is intentionally private; the allow silences the lint for this
// pub fn's signature.
/// Reconstruct one block via Q-only GF(2^8) math:
/// D_k = g^-(k-1) * (Q XOR XOR_{j != k} g^(j-1) * D_j).
#[allow(private_interfaces)]
pub fn recover_via_q(
    other_blocks: &[(u64, Vec<u8>)],
    q_block: &[u8],
    failing_slot: u64,
    block_size: usize,
    verifier: &dyn Fn(&[u8]) -> bool,
    corrupt: &[u8],
) -> QOutcome {
    assert_eq!(q_block.len(), block_size);
    let m = gf::gf_exp(-((failing_slot as i32) - 1));

    let mut acc = q_block.to_vec();
    for (slot, block) in other_blocks {
        let coef = gf::gf_exp((*slot as i32) - 1);
        gf_mul_xor_inplace(&mut acc, block, coef);
    }
    let mut recovered = vec![0u8; block_size];
    gf_mul_into(&mut recovered, &acc, m);

    // Recovered == corrupt means Q was recomputed from the corrupt byte —
    // the reconstruction carries no new information.
    if recovered.as_slice() == corrupt {
        return QOutcome::BakedIn;
    }
    if verifier(&recovered) {
        QOutcome::Recovered { block: recovered }
    } else {
        QOutcome::CsumMismatch
    }
}

/// Solve a two-disk corruption from P and Q: two equations in the two
/// unknown blocks (failing disk + one partner). Returns the failing
/// disk's block only.
pub fn solve_two_disk(
    p_block: &[u8],
    q_block: &[u8],
    other_blocks: &[(u64, Vec<u8>)],
    partner_slot: u64,
    failing_slot: u64,
    block_size: usize,
) -> Option<Vec<u8>> {
    // The two failed slots as 0-based column exponents, ascending.
    let (a, b) = if failing_slot < partner_slot {
        (failing_slot as i32 - 1, partner_slot as i32 - 1)
    } else {
        (partner_slot as i32 - 1, failing_slot as i32 - 1)
    };
    if a < 0 || b < 0 || a == b {
        return None;
    }
    let diff = (b - a).rem_euclid(255) as usize;

    let pbmul_coef = gf::GFEXI[diff];
    let qmul_coef = gf::GFINV[(gf::GFEXP[a as usize] ^ gf::GFEXP[b as usize]) as usize];
    let pbmul = &gf::GFMUL[pbmul_coef as usize];
    let qmul = &gf::GFMUL[qmul_coef as usize];

    // Syndromes of the good disks only; XORing them out of P and Q
    // isolates the two unknowns.
    let mut p_zero = vec![0u8; block_size];
    let mut q_zero = vec![0u8; block_size];
    for (slot, block) in other_blocks {
        if *slot == partner_slot || *slot == failing_slot {
            continue;
        }
        xor_inplace(&mut p_zero, block);
        let coef = gf::gf_exp(*slot as i32 - 1);
        gf_mul_xor_inplace(&mut q_zero, block, coef);
    }

    let mut delta_p = p_block.to_vec();
    xor_inplace(&mut delta_p, &p_zero);
    let mut delta_q = q_block.to_vec();
    xor_inplace(&mut delta_q, &q_zero);

    // Solve for d_b (column b); d_a follows since delta_p = d_a XOR d_b.
    let mut d_b = vec![0u8; block_size];
    for i in 0..block_size {
        d_b[i] = pbmul[delta_p[i] as usize] ^ qmul[delta_q[i] as usize];
    }
    let mut d_a = d_b.clone();
    xor_inplace(&mut d_a, &delta_p);

    Some(if failing_slot < partner_slot {
        d_a
    } else {
        d_b
    })
}

fn xor_inplace(a: &mut [u8], b: &[u8]) {
    debug_assert_eq!(a.len(), b.len(), "xor_inplace: length mismatch");
    for (x, y) in a.iter_mut().zip(b.iter()) {
        *x ^= *y;
    }
}

// `a ^= coef * b` byte-wise in GF(2^8).
fn gf_mul_xor_inplace(a: &mut [u8], b: &[u8], coef: u8) {
    debug_assert_eq!(a.len(), b.len(), "gf_mul_xor_inplace: length mismatch");
    let table = &gf::GFMUL[coef as usize];
    for (x, y) in a.iter_mut().zip(b.iter()) {
        *x ^= table[*y as usize];
    }
}

// `dest = coef * src` byte-wise in GF(2^8).
fn gf_mul_into(dest: &mut [u8], src: &[u8], coef: u8) {
    debug_assert_eq!(dest.len(), src.len(), "gf_mul_into: length mismatch");
    let table = &gf::GFMUL[coef as usize];
    for (d, s) in dest.iter_mut().zip(src.iter()) {
        *d = table[*s as usize];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recovery::model::{FailureReason, ParityPath, RecoveryInput, RecoveryResult};

    const BLOCK: usize = 8;

    /// A `block_size`-byte stripe of `n` data disks with computed P and Q.
    #[derive(Clone)]
    struct Stripe {
        data: Vec<(u64, Vec<u8>)>,
        p: Vec<u8>,
        q: Vec<u8>,
    }

    impl Stripe {
        fn new(block_size: usize, n: u8, seed: u64) -> Self {
            use std::num::Wrapping;
            let mut s = Wrapping(seed.max(1));
            let mut lcg = move || {
                s ^= s >> 12;
                s ^= s << 25;
                s ^= s >> 27;
                (s.0.wrapping_mul(0x2545F4914F6CDD1D)) as u8
            };
            let data: Vec<(u64, Vec<u8>)> = (1..=n)
                .map(|slot| (slot as u64, (0..block_size).map(|_| lcg()).collect()))
                .collect();
            Stripe {
                p: compute_p(&data, block_size),
                q: compute_q(&data, block_size),
                data,
            }
        }

        fn chunk(&self, slot: u64) -> &[u8] {
            &self.data.iter().find(|(s, _)| *s == slot).unwrap().1
        }

        fn others(&self, failing_slot: u64) -> Vec<(u64, Vec<u8>)> {
            self.data
                .iter()
                .filter(|(s, _)| *s != failing_slot)
                .cloned()
                .collect()
        }
    }

    fn compute_p(data: &[(u64, Vec<u8>)], block_size: usize) -> Vec<u8> {
        let mut p = vec![0u8; block_size];
        for (_, b) in data {
            for (x, y) in p.iter_mut().zip(b.iter()) {
                *x ^= y;
            }
        }
        p
    }

    fn compute_q(data: &[(u64, Vec<u8>)], block_size: usize) -> Vec<u8> {
        let mut q = vec![0u8; block_size];
        for (slot, b) in data {
            let coef = gf::gf_exp(*slot as i32 - 1);
            let table = &gf::GFMUL[coef as usize];
            for (q_byte, d_byte) in q.iter_mut().zip(b.iter()) {
                *q_byte ^= table[*d_byte as usize];
            }
        }
        q
    }

    // Flip byte 1 of `chunk` by 0x5a to corrupt it.
    fn corrupt(chunk: &[u8]) -> Vec<u8> {
        let mut c = chunk.to_vec();
        c[1] ^= 0x5a;
        c
    }

    // Recompute P with the failing slot's chunk replaced by `corrupt`.
    fn bake_p(stripe: &Stripe, failing: u64, corrupt: &[u8]) -> Vec<u8> {
        let data: Vec<(u64, Vec<u8>)> = stripe
            .data
            .iter()
            .map(|(s, b)| {
                (
                    *s,
                    if *s == failing {
                        corrupt.to_vec()
                    } else {
                        b.clone()
                    },
                )
            })
            .collect();
        compute_p(&data, BLOCK)
    }

    // Recompute Q with the failing slot's chunk replaced by `corrupt`.
    fn bake_q(stripe: &Stripe, failing: u64, corrupt: &[u8]) -> Vec<u8> {
        let data: Vec<(u64, Vec<u8>)> = stripe
            .data
            .iter()
            .map(|(s, b)| {
                (
                    *s,
                    if *s == failing {
                        corrupt.to_vec()
                    } else {
                        b.clone()
                    },
                )
            })
            .collect();
        compute_q(&data, BLOCK)
    }

    // Assert the recovery used the expected path and returned `golden`.
    fn expect_recovered(r: RecoveryResult, expect_via: ParityPath, golden: &[u8]) {
        match r {
            RecoveryResult::Recovered { via, block } => {
                assert_eq!(via, expect_via, "wrong parity path");
                assert_eq!(block, golden, "recovered block != original");
            }
            other => panic!("expected Recovered via {expect_via:?}, got {other:?}"),
        }
    }

    #[test]
    fn p_path_recovers_when_q_unavailable() {
        // 3 data disks, no Q, P intact: the P path must reconstruct D2.
        let stripe = Stripe::new(BLOCK, 3, 0xA1);
        let failing = 2;
        let golden = stripe.chunk(failing).to_vec();
        let corrupt = corrupt(&golden);
        let others = stripe.others(failing);
        let v = |b: &[u8]| b == golden;
        let input = RecoveryInput {
            failing_slot: failing,
            corrupt_block: &corrupt,
            unreadable_source: false,
            other_blocks: &others,
            p_block: Some(&stripe.p),
            q_block: None,
            verifier: &v,
        };
        expect_recovered(recover_block(&input, BLOCK), ParityPath::P, &golden);
    }

    #[test]
    fn q_path_recovers_when_p_baked_in() {
        // P recomputed from the corrupt bytes; Q still reflects the
        // original data, so the Q path must succeed.
        let stripe = Stripe::new(BLOCK, 4, 0xB2);
        let failing = 2;
        let golden = stripe.chunk(failing).to_vec();
        let corrupt = corrupt(&golden);
        let p_baked = bake_p(&stripe, failing, &corrupt);
        let q_orig = stripe.q.clone();
        let others = stripe.others(failing);
        let v = |b: &[u8]| b == golden;
        let input = RecoveryInput {
            failing_slot: failing,
            corrupt_block: &corrupt,
            unreadable_source: false,
            other_blocks: &others,
            p_block: Some(&p_baked),
            q_block: Some(&q_orig),
            verifier: &v,
        };
        expect_recovered(recover_block(&input, BLOCK), ParityPath::Q, &golden);
    }

    #[test]
    fn p_path_recovers_when_q_baked_in() {
        // Q baked in from the corrupt bytes; P intact: the P path must
        // succeed.
        let stripe = Stripe::new(BLOCK, 4, 0xC3);
        let failing = 3;
        let golden = stripe.chunk(failing).to_vec();
        let corrupt = corrupt(&golden);
        let p_orig = stripe.p.clone();
        let q_baked = bake_q(&stripe, failing, &corrupt);
        let others = stripe.others(failing);
        let v = |b: &[u8]| b == golden;
        let input = RecoveryInput {
            failing_slot: failing,
            corrupt_block: &corrupt,
            unreadable_source: false,
            other_blocks: &others,
            p_block: Some(&p_orig),
            q_block: Some(&q_baked),
            verifier: &v,
        };
        expect_recovered(recover_block(&input, BLOCK), ParityPath::P, &golden);
    }

    #[test]
    fn pq_two_disk_solve_when_both_corrupt() {
        // Two disks silently corrupt; P and Q still reflect the original
        // stripe (no parity resync). Single paths inherit the partner's
        // bad bytes and fail; the PQ solve must isolate both unknowns.
        let stripe = Stripe::new(BLOCK, 4, 0xD4);
        let failing = 1;
        let partner = 3;
        let golden = stripe.chunk(failing).to_vec();
        let golden_partner = stripe.chunk(partner).to_vec();
        let corrupt = corrupt(&golden);
        let partner_corrupt = {
            let mut c = golden_partner.clone();
            c[2] ^= 0xa5;
            c
        };
        // Original P and Q, computed from the uncorrupted stripe.
        let p = stripe.p.clone();
        let q = stripe.q.clone();
        // Other data includes the partner's corrupt chunk — the engine
        // does not know it is bad.
        let others: Vec<(u64, Vec<u8>)> = stripe
            .data
            .iter()
            .map(|(s, b)| {
                let chunk = if *s == partner {
                    partner_corrupt.clone()
                } else {
                    b.clone()
                };
                (*s, chunk)
            })
            .filter(|(s, _)| *s != failing)
            .collect();
        let v = |b: &[u8]| b == golden;
        let input = RecoveryInput {
            failing_slot: failing,
            corrupt_block: &corrupt,
            unreadable_source: false,
            other_blocks: &others,
            p_block: Some(&p),
            q_block: Some(&q),
            verifier: &v,
        };
        expect_recovered(
            recover_block(&input, BLOCK),
            ParityPath::PQ {
                partner_slot: partner,
            },
            &golden,
        );
    }

    #[test]
    fn all_paths_fail_when_both_parity_baked() {
        // Both P and Q recomputed from the corrupt bytes: every path
        // reproduces the corruption, so recovery must fail.
        let stripe = Stripe::new(BLOCK, 4, 0xE5);
        let failing = 2;
        let golden = stripe.chunk(failing).to_vec();
        let corrupt = corrupt(&golden);
        let p_baked = bake_p(&stripe, failing, &corrupt);
        let q_baked = bake_q(&stripe, failing, &corrupt);
        let others = stripe.others(failing);
        let v = |b: &[u8]| b == golden;
        let input = RecoveryInput {
            failing_slot: failing,
            corrupt_block: &corrupt,
            unreadable_source: false,
            other_blocks: &others,
            p_block: Some(&p_baked),
            q_block: Some(&q_baked),
            verifier: &v,
        };
        match recover_block(&input, BLOCK) {
            RecoveryResult::Failed {
                reason:
                    FailureReason::AllPathsFailed {
                        pq_partners_tried, ..
                    },
            } => {
                assert!(!pq_partners_tried.is_empty(), "should have tried partners");
            }
            other => panic!("expected AllPathsFailed, got {other:?}"),
        }
    }

    #[test]
    fn not_corrupt_when_verifier_accepts_on_disk() {
        // The on-disk block already passes the verifier (raced rewrite):
        // return NotCorrupt without consulting parity.
        let stripe = Stripe::new(BLOCK, 3, 0xF6);
        let failing = 1;
        let golden = stripe.chunk(failing).to_vec();
        let others = stripe.others(failing);
        let v = |b: &[u8]| b == golden;
        // Pass the golden (uncorrupted) block as corrupt_block.
        let input = RecoveryInput {
            failing_slot: failing,
            corrupt_block: &golden,
            unreadable_source: false,
            other_blocks: &others,
            p_block: Some(&stripe.p),
            q_block: Some(&stripe.q),
            verifier: &v,
        };
        assert!(matches!(
            recover_block(&input, BLOCK),
            RecoveryResult::NotCorrupt
        ));
    }

    #[test]
    fn no_q_and_p_fails_reports_no_q_path_and_p_failed() {
        // Single-parity array; P corrupted independently of the data:
        // expect NoQPathAndPFailed, not AllPathsFailed.
        let stripe = Stripe::new(BLOCK, 3, 0xA1);
        let failing = 2;
        let golden = stripe.chunk(failing).to_vec();
        let corrupt = corrupt(&golden);
        // Independent P corruption: flip one byte of the real P.
        let mut p_bad = stripe.p.clone();
        p_bad[3] ^= 0x11;
        let others = stripe.others(failing);
        let v = |b: &[u8]| b == golden;
        let input = RecoveryInput {
            failing_slot: failing,
            corrupt_block: &corrupt,
            unreadable_source: false,
            other_blocks: &others,
            p_block: Some(&p_bad),
            q_block: None,
            verifier: &v,
        };
        match recover_block(&input, BLOCK) {
            RecoveryResult::Failed {
                reason: FailureReason::NoQPathAndPFailed { .. },
            } => {}
            other => panic!("expected NoQPathAndPFailed, got {other:?}"),
        }
    }

    #[test]
    fn unreadable_source_recovers_all_zero_block_via_p() {
        // Unreadable source whose real data is all-zero: without the flag
        // the engine would misclassify it as NotCorrupt/ParityBakedIn;
        // with it, P-only recovery must return Recovered.
        let block_size = BLOCK;
        let n = 3u8;
        // Slot 2's data is all zeros.
        let mut data: Vec<(u64, Vec<u8>)> = (1..=n as u64)
            .map(|slot| {
                let bytes = if slot == 2 {
                    vec![0u8; block_size]
                } else {
                    (0..block_size)
                        .map(|i| (slot as u8) * 7 + (i % 251) as u8)
                        .collect()
                };
                (slot, bytes)
            })
            .collect();
        let p = {
            let mut acc = vec![0u8; block_size];
            for (_, b) in &data {
                for (x, y) in acc.iter_mut().zip(b.iter()) {
                    *x ^= y;
                }
            }
            acc
        };
        // Slot 2 (failing) is excluded from other_blocks.
        data.retain(|(s, _)| *s != 2);
        let golden: Vec<u8> = vec![0u8; block_size];
        let placeholder: Vec<u8> = vec![0u8; block_size];
        let v = |b: &[u8]| b == golden;
        let input = RecoveryInput {
            failing_slot: 2,
            corrupt_block: &placeholder,
            unreadable_source: true,
            other_blocks: &data,
            p_block: Some(&p),
            q_block: None,
            verifier: &v,
        };
        expect_recovered(recover_block(&input, block_size), ParityPath::P, &golden);
    }

    #[test]
    fn unreadable_source_skips_not_corrupt_when_verifier_accepts_placeholder() {
        // The verifier accepts zeros here, but the placeholder proves
        // nothing — the engine must still reconstruct from parity.
        let block_size = BLOCK;
        let n = 3u8;
        let mut data: Vec<(u64, Vec<u8>)> = (1..=n as u64)
            .map(|slot| {
                let bytes = (0..block_size)
                    .map(|i| (slot as u8) * 3 + (i % 199) as u8)
                    .collect();
                (slot, bytes)
            })
            .collect();
        // Failing slot 2 is all-zero on disk.
        data = data
            .iter()
            .map(|(s, b)| {
                if *s == 2 {
                    (*s, vec![0u8; block_size])
                } else {
                    (*s, b.clone())
                }
            })
            .collect();
        let p = {
            let mut acc = vec![0u8; block_size];
            for (_, b) in &data {
                for (x, y) in acc.iter_mut().zip(b.iter()) {
                    *x ^= y;
                }
            }
            acc
        };
        data.retain(|(s, _)| *s != 2);
        let placeholder: Vec<u8> = vec![0u8; block_size];
        // Verifier accepts zeros (would-be NotCorrupt trap).
        let v = |b: &[u8]| b == vec![0u8; block_size];
        let input = RecoveryInput {
            failing_slot: 2,
            corrupt_block: &placeholder,
            unreadable_source: true,
            other_blocks: &data,
            p_block: Some(&p),
            q_block: None,
            verifier: &v,
        };
        match recover_block(&input, block_size) {
            RecoveryResult::Recovered { block, .. } => {
                assert_eq!(block, vec![0u8; block_size]);
            }
            other => panic!("expected Recovered via P, got {other:?}"),
        }
    }
}
