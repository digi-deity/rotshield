//! Pure parity-recovery engine — I/O-free, checksum-agnostic.
//!
//! The single entry point [`recover_block`] takes a [`RecoveryInput`] (the
//! aligned chunks of one stripe plus a verifier closure) and runs the
//! full cascade — P-only → Q-only → PQ 2-disk brute-force — returning the
//! first candidate the verifier accepts.  It performs **no I/O whatsoever**:
//! every byte, including the corrupt block and the parity blocks, was
//! already gathered by the caller (the array layer).  This is what makes
//! the recovery math trivial to unit-test (see `tests/` below).
//!
//! The low-level functions [`recover_via_p`], [`recover_via_q`], and
//! [`solve_two_disk`] are also `pub` because tests exercise them in
//! isolation against fabricated syndromes — they're not part of the
//! stable public API though; the integration glue normally calls only
//! [`recover_block`].

use crate::recovery::gf;
use crate::recovery::model::{FailureReason, ParityPath, RecoveryInput, RecoveryResult};

/// Outcome of the Q-path reconstruction attempt (internal helper).
enum QOutcome {
    /// Reconstructed block the verifier accepted.
    Recovered { block: Vec<u8> },
    /// Q recomputed from the corrupt byte: the reconstruction equals the
    /// corrupt block and carries no new information.
    BakedIn,
    /// Reconstructed block did not pass the verifier.
    CsumMismatch,
}

/// Attempt to recover one corrupt block via parity, trying every path.
///
/// Order: P-only → Q-only → PQ 2-disk solve with each candidate partner.
/// Returns [`RecoveryResult::Recovered`] for the first candidate the
/// verifier accepts, [`RecoveryResult::NotCorrupt`] if the failing disk
/// already passes the verifier (raced with a kernel rewrite), or
/// [`RecoveryResult::Failed`] with the aggregated reasons otherwise.
///
/// `block_size` is the length of every block — passed explicitly so tests
/// can fabricate mini-stripes (the kernel uses 4096; tests might use 4 or
/// 16).  Every slice in `input` must be exactly `block_size` bytes or
/// the function panics (cheap guard at the seam).
pub fn recover_block(input: &RecoveryInput<'_>, block_size: usize) -> RecoveryResult {
    // 1. Cheap sanity: the failing block already passes the verifier?  Then
    //    the scrub raced with a kernel rewrite — nothing to recover.  This
    //    also establishes a baseline against the "baked in" check, which
    //    wants to see a corrupt block to compare against.
    let corrupt = input.corrupt_block;
    assert_eq!(corrupt.len(), block_size, "corrupt_block length mismatch");
    // When the source bytes were unreadable (EIO), `corrupt` is a zero
    // placeholder, NOT the on-disk data — the "already matches" early
    // return and the "parity baked in" detections are meaningless and must
    // be skipped (a legitimately all-zero block would otherwise be
    // misclassified as NotCorrupt / ParityBakedIn).
    let unreadable = input.unreadable_source;
    if !unreadable && (input.verifier)(corrupt) {
        return RecoveryResult::NotCorrupt;
    }

    // 2. P-only path (if P is present).  `recover_via_p` returns either a
    //    candidate or a BakedIn indicator; only check the verifier if P
    //    actually produced something new.
    let p_reason: Option<FailureReason> = match input.p_block {
        None => Some(FailureReason::ParityAbsent { via: ParityPath::P }),
        Some(p_block) => {
            assert_eq!(p_block.len(), block_size, "p_block length mismatch");
            // Reject the all-zero XOR-pre-image as baked-in early: when P
            // matches the (already-corrupted) data stripe, the (corrupt +
            // others + P) XOR collapse proves P was recomputed from the
            // corrupt byte.
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

    // 3. Q-only path (if Q is present).  Same structure as P, but using
    //    GF(2^8) reconstruction.
    let q_reason: Option<FailureReason> = match input.q_block {
        None => Some(FailureReason::ParityAbsent { via: ParityPath::Q }),
        Some(q_block) => {
            assert_eq!(q_block.len(), block_size, "q_block length mismatch");
            // For an unreadable source, pass an empty `corrupt` slice so
            // `recover_via_q`'s internal BakedIn comparison (recovered ==
            // corrupt) can never fire — the zero placeholder is not the
            // real data, and a legitimately all-zero recovered block must
            // not be misclassified.
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

    // 4. PQ 2-disk solve: only possible if both P and Q chunks are present.
    //    We don't know which other disk is the partner, so brute-force every
    //    candidate; the verifier tells us which guess was right.
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
            // Wrong partner — keep trying.
        }
        return RecoveryResult::Failed {
            reason: FailureReason::AllPathsFailed {
                p_reason: Box::new(p_singleton),
                q_reason: Box::new(q_singleton),
                pq_partners_tried: partners_tried,
            },
        };
    }

    // 5. No Q disk at all (single-parity array).  Report P failure as the
    //    single reason rather than packaging a phantom Q result.
    RecoveryResult::Failed {
        reason: if input.q_block.is_none() {
            FailureReason::NoQPathAndPFailed {
                p_reason: Box::new(p_singleton),
            }
        } else {
            // P chunk was None but Q present — Q path already ran above and
            // produced q_singleton.  Surface as exhausted-all-paths with an
            // empty PQ list (the PQ solve needs both P and Q).
            FailureReason::AllPathsFailed {
                p_reason: Box::new(p_singleton),
                q_reason: Box::new(q_singleton),
                pq_partners_tried: Vec::new(),
            }
        },
    }
}

/// Reconstruct one missing block via P-only XOR.
///
/// `D_k = P XOR XOR_{j≠k} D_j`.  The corrupt block itself doesn't enter
/// (its contribution is folded into P). Lengths are asserted; pass
/// `block_size`-byte slices only.
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

/// Reconstruct one missing block via Q-only GF(2^8) math.
///
/// `D_k = g^(-(k-1)) · (Q XOR XOR_{j≠k} g^(j-1) · D_j)`.  When the result
/// equals the corrupt block the caller passed, we report `BakedIn`: Q was
/// recomputed from the corrupt byte and carries no new information.
#[allow(private_interfaces)] // QOutcome is intentionally private
pub fn recover_via_q(
    other_blocks: &[(u64, Vec<u8>)],
    q_block: &[u8],
    failing_slot: u64,
    block_size: usize,
    verifier: &dyn Fn(&[u8]) -> bool,
    corrupt: &[u8],
) -> QOutcome {
    // Note: visibility of `QOutcome` is `pub(self)` but this fn is `pub`;
    // the wrapper `#[allow(private_interfaces)]` silences the lint since
    // callers can't name `QOutcome` anyway (it's an internal helper).  The
    // public callers go through `recover_block`.
    assert_eq!(q_block.len(), block_size);
    let m = gf::gf_exp(-((failing_slot as i32) - 1));

    let mut acc = q_block.to_vec();
    for (slot, block) in other_blocks {
        let coef = gf::gf_exp((*slot as i32) - 1);
        gf_mul_xor_inplace(&mut acc, block, coef);
    }
    let mut recovered = vec![0u8; block_size];
    gf_mul_into(&mut recovered, &acc, m);

    // "Baked in" detection: a recomputed Q gives back exactly the corrupt
    // bytes, so failure would look like a CsumMismatch — but the
    // distinction matters to the caller (Test B/C/F distinguish a
    // "burned Q" from a "Q returned garbage").
    if recovered.as_slice() == corrupt {
        return QOutcome::BakedIn;
    }
    if verifier(&recovered) {
        QOutcome::Recovered { block: recovered }
    } else {
        QOutcome::CsumMismatch
    }
}

/// Solve a 2-disk corruption system using P and Q simultaneously.
///
/// Direct port of `raid6_2data_recov_intx1` from `nonraid/raid6/recov.c`,
/// specialised to NonRAID's slot-based addressing.  We assume the failing
/// disk (`failing_slot`) and one `partner_slot` are both corrupt at this
/// offset; P and Q give two equations in two unknowns.
///
/// Returns the reconstructed block for the **failing** disk (never the
/// partner — the caller verifies only the failing block's csum and writes
/// only the failing disk; the partner's bytes never enter the math).
pub fn solve_two_disk(
    p_block: &[u8],
    q_block: &[u8],
    other_blocks: &[(u64, Vec<u8>)],
    partner_slot: u64,
    failing_slot: u64,
    block_size: usize,
) -> Option<Vec<u8>> {
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

    // P_zero = XOR of all good disks (everyone except failing & partner).
    // Q_zero = XOR_{j good} g^(j-1) · D_j.
    // ΔP = P ⊕ P_zero, ΔQ = Q ⊕ Q_zero.
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

/// XOR `b` into `a` in place.  Panics if lengths differ.
fn xor_inplace(a: &mut [u8], b: &[u8]) {
    debug_assert_eq!(a.len(), b.len(), "xor_inplace: length mismatch");
    for (x, y) in a.iter_mut().zip(b.iter()) {
        *x ^= *y;
    }
}

/// `a ^= coef * b` byte-wise in GF(2^8).  Panics if lengths differ.
fn gf_mul_xor_inplace(a: &mut [u8], b: &[u8], coef: u8) {
    debug_assert_eq!(a.len(), b.len(), "gf_mul_xor_inplace: length mismatch");
    let table = &gf::GFMUL[coef as usize];
    for (x, y) in a.iter_mut().zip(b.iter()) {
        *x ^= table[*y as usize];
    }
}

/// `dest = coef * src` byte-wise in GF(2^8).  Panics if lengths differ.
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

    /// Corrupt one byte of `chunk` (flip byte idx 1 by 0x5a).
    fn corrupt(chunk: &[u8]) -> Vec<u8> {
        let mut c = chunk.to_vec();
        c[1] ^= 0x5a;
        c
    }

    /// Reconpute P with the failing slot's chunk replaced by `corrupt`.
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

    /// Reompute Q with the failing slot's chunk replaced by `corrupt`.
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

    /// Assert we recovered via the given path and the block equals golden.
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
        // 3 data disks, no Q. P intact. P path must reconstruct D_2.
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
        // 4 data disks. Bake P from corrupt → P path fails (baked-in),
        // Q intact → Q path succeeds.
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
        // 4 data disks. Bake Q from corrupt → Q path fails (baked-in),
        // P intact → P path succeeds.
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
        // 4 data disks. Two disks silently corrupted at this offset
        // (failing=1, partner=3); P and Q are LEFT INTACT (still reflect
        // the original data — the "silent corruption, no parity sync"
        // case).  Single-parity P and Q each inherit the *partner's* bad
        // bytes and fail; the PQ 2-disk solve uses P and Q simultaneously
        // to isolate both unknowns and reconstruct D_1.
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
        // Original P and Q (computed from the golden, uncorrupted stripe)
        // — parity was NOT resynced after the silent corruption.
        let p = stripe.p.clone();
        let q = stripe.q.clone();
        // `other_blocks` for the failing disk includes the partner's
        // *corrupt* chunk (the recovery code doesn't know it's bad) and
        // the other slots' originals.
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
        // 4 data disks. Bake both P and Q from corrupt → no path can
        // reconstruct; PQ brute-force finds no verifiable partner (only the
        // failing disk is corrupt, so every candidate is wrong).
        // Expect AllPathsFailed.
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
        // Scrub said mismatch but on-disk now matches the verifier —
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
        // Single-parity array (no Q). P corrupted independently of the
        // failing disk's data → P path fails with CsumMismatch → expect
        // NoQPathAndPFailed (not AllPathsFailed).
        let stripe = Stripe::new(BLOCK, 3, 0xA1);
        let failing = 2;
        let golden = stripe.chunk(failing).to_vec();
        let corrupt = corrupt(&golden);
        // Independent P corruption: flip a byte of the real P.
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
        // The failing disk's bytes were unreadable (EIO), so the caller
        // passes a ZERO placeholder as corrupt_block with unreadable_source
        // set.  The underlying data is legitimately all-zero: without the
        // flag the engine would misclassify the reconstruction as
        // NotCorrupt / ParityBakedIn.  With it, P-only recovery must
        // reconstruct the all-zero block and return Recovered.
        let block_size = BLOCK;
        let n = 3u8;
        // Build a stripe where the failing slot's data is all zeros.
        let mut data: Vec<(u64, Vec<u8>)> = (1..=n as u64)
            .map(|slot| {
                let bytes = if slot == 2 {
                    vec![0u8; block_size]
                } else {
                    (0..block_size).map(|i| (slot as u8) * 7 + (i % 251) as u8).collect()
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
        // Even with unreadable_source, a candidate whose verifier happens to
        // accept the zero placeholder (e.g. a genuinely all-zero expected
        // block) must NOT take the NotCorrupt early-return — the placeholder
        // is not the real data, so "matches" proves nothing.  The engine
        // must still reconstruct from parity and return Recovered.
        let block_size = BLOCK;
        let n = 3u8;
        let mut data: Vec<(u64, Vec<u8>)> = (1..=n as u64)
            .map(|slot| {
                let bytes = (0..block_size).map(|i| (slot as u8) * 3 + (i % 199) as u8).collect();
                (slot, bytes)
            })
            .collect();
        // Failing slot 2, all-zero on disk.
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
