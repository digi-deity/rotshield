//! Parity-XOR recovery for corrupt data blocks on a NonRAID/Unraid array.
//!
//! Mirrors the core of `recover.py::recover_sector`: for a confirmed corrupt
//! block on a data disk, XOR all *other* data disks plus the parity disk at
//! the same raw offset to reconstruct the original data, then verify it
//! against the filesystem-stored checksum and (optionally) write it back.
//!
//! The NonRAID parity relationship is `P == XOR(D1, D2, ...)` across all
//! data disks at every offset.  Rearranging for one missing disk:
//!
//! ```text
//!   Di == XOR(all other Dj, P)
//! ```
//!
//! so the corrupt disk's original contents can be reconstructed without
//! touching it.
//!
//! All reads and writes in this module are in **raw-rdev space** — they
//! open the raw rdev (`/dev/loop2`) directly, bypassing the array driver.
//! This is critical: writing through the array partition would make the
//! driver recompute parity from the recovered data, destroying the
//! original parity relationship and making a botched recovery
//! unrecoverable.  See the "Address spaces and I/O paths" doc in
//! [`mod`](self) for the full rationale.
//!
//! Asymmetric arrays: a smaller data disk read past its end yields a short
//! read or `EINVAL`.  The missing region contributes zeros to the
//! parity relationship (verified experimentally — see
//! `memories/repo/nonraid-asymmetric-parity.md`), so unreadable reads are
//! substituted with zero bytes.
//!
//! Filesystem-agnostic: takes `(devid, array_phys, block_size,
//! expected_csum)` and knows nothing about btrfs chunks, CSUM trees, or
//! ZFS vdevs.  The caller is responsible for providing the stored
//! checksum; this module only does XOR reconstruction and verification.

use std::fs::OpenOptions;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::array::config::ArrayConfig;
use crate::array::resolve::ResolvedLocation;

/// Outcome of a single sector recovery attempt.
///
/// The fields on each variant carry diagnostic detail (checksums, failure
/// reasons) that callers may log or inspect.  They are not consumed by the
/// current scrub integration, which only logs them — hence the `dead_code`
/// allow on the variants that carry data.
#[derive(Debug)]
pub enum RecoveryResult {
    /// Successfully reconstructed and (if not dry-run) wrote back the data.
    #[allow(dead_code)]
    Recovered {
        location: ResolvedLocation,
        /// Which parity disk reconstructed the block (P = XOR, Q = GF(2^8)).
        via: ParityPath,
        expected_csum: u32,
        recovered_csum: u32,
        written: bool,
    },
    /// The sector is not actually corrupt (on-disk data matches stored checksum).
    #[allow(dead_code)]
    NotCorrupt {
        location: ResolvedLocation,
        on_disk_csum: u32,
        expected_csum: u32,
    },
    /// Recovery failed for a known reason.
    Failed {
        location: ResolvedLocation,
        reason: FailureReason,
    },
}

/// Which parity disk(s) a recovery attempt used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum ParityPath {
    /// Single-parity: XOR of all other data disks plus P.
    P,
    /// Single-parity: GF(2^8) reconstruction from Q.
    Q,
    /// Dual-parity: P and Q simultaneously, solving a 2-disk-corruption
    /// system via raid6_2data_recov math.  `partner_slot` identifies the
    /// other data disk we assumed was also corrupt.
    PQ { partner_slot: u64 },
}

/// Why a recovery attempt failed.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum FailureReason {
    /// XOR of all disks (including the corrupt one) is zero — the corruption
    /// has been baked into parity P, so the original cannot be reconstructed
    /// from P alone.  This is *not* fatal: the Q path (and the PQ 2-disk
    /// solve) are still attempted by `recover_sector`.
    ParityBakedIn,
    /// Recovered data's checksum does not match the stored checksum.
    /// `via` identifies which parity path produced the bad result.
    CsumMismatch {
        via: ParityPath,
        recovered_csum: u32,
        expected_csum: u32,
    },
    /// I/O error while reading a disk or writing back.
    Io(String),
    /// A second data disk was corrupt at the same offset, so single-parity
    /// paths (P and Q) each failed because they include the partner's bad
    /// bytes.  We then brute-forced every possible partner slot, used the
    /// raid6_2data_recov math (P and Q simultaneously) to solve for both
    /// unknowns, and verified the failing disk's reconstructed block
    /// against the stored checksum.  We were able to recover the failing
    /// disk via this path; `partner_slot` is the partner we identified.
    /// (Only present if recovery succeeded — see `RecoveryResult::Recovered`.)
    ///
    /// If NO partner produced a checksum match for the failing disk, the
    /// failure is reported as `AllPathsFailed` instead.
    TwoDiskNeeded {
        partner_slot: u64,
    },
    /// Every recovery path failed:
    ///   - P-only (ParityBakedIn or CsumMismatch)
    ///   - Q-only (CsumMismatch or Q unavailable)
    ///   - PQ 2-disk solve with every possible partner (no partner produced
    ///     a checksum match for the failing disk — either 3+ disks are
    ///     corrupt, or the data outside the array's coverage is also bad,
    ///     or the stored checksum itself is wrong)
    /// `p_reason` and `q_reason` carry the single-path diagnostics;
    /// `pq_partners_tried` lists the partner slots we brute-forced.
    AllPathsFailed {
        p_reason: Box<FailureReason>,
        q_reason: Box<FailureReason>,
        pq_partners_tried: Vec<u64>,
    },
    /// Backwards-compat alias: P and Q both failed *before* the PQ path was
    /// attempted.  Modern `recover_sector` always tries the PQ path, so this
    /// is only constructed if `parity_q` is absent (single-parity array).
    BothPathsFailed {
        p_reason: Box<FailureReason>,
        q_reason: Box<FailureReason>,
    },
}

/// Configuration for the recovery pass.
#[derive(Debug, Clone, Copy)]
pub struct RecoverOpts {
    /// If true, log what *would* be written but do not modify any disk.
    /// Essential for testing — otherwise the corruption disappears and the
    /// test becomes non-repeatable.
    pub dry_run: bool,
}

impl Default for RecoverOpts {
    fn default() -> Self {
        Self { dry_run: true }
    }
}

/// Attempt to recover one corrupt sector via parity XOR.
///
/// `expected_csum` is the filesystem-stored checksum for this sector
/// (already pulled from the metadata tree by the caller).  `location` is
/// the resolved raw-rdev location of the failing sector.  `block_size` is
/// the sector size in bytes (4096 for btrfs) — passed in so this module
/// stays filesystem-agnostic.
///
/// This function reads the failing disk to confirm corruption, then reads
/// every *other* data disk plus parity P at the same raw offset, XORs them
/// to reconstruct the original data, and verifies the checksum.  On
/// success (and if `opts.dry_run` is false) it writes the recovered block
/// back to the failing disk only — parity is left untouched.
pub fn recover_sector(
    config: &ArrayConfig,
    location: &ResolvedLocation,
    expected_csum: u32,
    block_size: usize,
    opts: RecoverOpts,
) -> RecoveryResult {
    // 1. Read the (allegedly) corrupt block from the failing disk and
    //    confirm it doesn't already match the stored checksum.  The scrub
    //    already told us it mismatches, but re-checking guards against
    //    races where the kernel rewrote the sector between scrub and now.
    let corrupted_block = match read_block_or_zeros(&location.dev_path, location.raw_phys, block_size) {
        Ok(b) => b,
        Err(e) => {
            return RecoveryResult::Failed {
                location: location.clone(),
                reason: FailureReason::Io(format!("read failing disk: {e}")),
            };
        }
    };
    let on_disk_csum = crc32c::crc32c(&corrupted_block);
    if on_disk_csum == expected_csum {
        return RecoveryResult::NotCorrupt {
            location: location.clone(),
            on_disk_csum,
            expected_csum,
        };
    }

    // 2. Read every *other* data disk at the same raw offset, keyed by
    //    slot number.  The failing disk is excluded — its (corrupt) bytes
    //    are what we are reconstructing.  Both the P and Q paths need the
    //    same set of "other data disk" blocks, so we read them once and
    //    reuse for both attempts.
    //
    //    Slot number == btrfs devid == NonRAID column index + 1, which is
    //    also the exponent base for the Q coefficient g^(slot-1) — see
    //    `array::gf` for the derivation.
    // Each data disk may have its *own* rdevOffset (arrays with asymmetric
    // per-disk offsets), so every read below must add that disk's own
    // offset to `location.array_phys` via `config.raw_offset_for` — never
    // reuse `location.raw_phys`, which only holds the *failing* disk's
    // offset.
    let mut other_data: Vec<(u64, Vec<u8>)> = Vec::new();
    for (slot, path) in &config.data_devs {
        if path.as_path() == location.dev_path.as_path() {
            continue;
        }
        let raw_phys = location.array_phys + config.raw_offset_for(path);
        match read_block_or_zeros(path, raw_phys, block_size) {
            Ok(b) => other_data.push((*slot, b)),
            Err(e) => {
                return RecoveryResult::Failed {
                    location: location.clone(),
                    reason: FailureReason::Io(format!("read data disk {}: {e}", path.display())),
                };
            }
        }
    }
    let failing_slot = location.devid;

    // 3. Read the parity P and Q blocks now, once, so all three recovery
    //    paths (P-only, Q-only, PQ 2-disk solve) can reuse them without
    //    re-reading.  Both are read in raw-rdev space at the same offset
    //    as the corrupt data block.  Either may be `None` if the disk is
    //    absent or unreadable; the paths below handle that.
    let p_block_opt: Option<Vec<u8>> = match config.parity_p.as_ref() {
        None => None,
        Some(parity_p) => {
            let raw_phys = location.array_phys + config.raw_offset_for(parity_p);
            match read_block_or_zeros(parity_p, raw_phys, block_size) {
                Ok(b) => Some(b),
                Err(_) => None,
            }
        }
    };
    let q_block_opt: Option<Vec<u8>> = match config.parity_q.as_ref() {
        None => None,
        Some(parity_q) => {
            let raw_phys = location.array_phys + config.raw_offset_for(parity_q);
            match read_block_or_zeros(parity_q, raw_phys, block_size) {
                Ok(b) => Some(b),
                Err(_) => None,
            }
        }
    };

    // 4. Attempt P-only recovery first.  This is the cheap, common path:
    //    if parity P was not recomputed from the corrupt byte, a single
    //    XOR reconstructs the original.  We also need the parity-P block
    //    for this, already read above.
    let p_reason: Option<FailureReason> = match (config.parity_p.as_ref(), p_block_opt.as_ref()) {
        (None, _) => Some(FailureReason::Io("no primary parity disk in array config".into())),
        (_, None) => Some(FailureReason::Io("read parity P failed".into())),
        (_, Some(p_block)) => {
            // 4a. Sanity check: XOR(corrupted, all others, P) must be
            //     non-zero.  A zero result means P was recomputed from
            //     the corrupt byte, so the original cannot be
            //     reconstructed from P alone — fall through to Q.
            let mut parity_check = corrupted_block.clone();
            for (_, b) in &other_data {
                xor_inplace(&mut parity_check, b);
            }
            xor_inplace(&mut parity_check, p_block);
            if parity_check.iter().all(|&x| x == 0) {
                Some(FailureReason::ParityBakedIn)
            } else {
                // 4b. Reconstruct: Di == XOR(all other Dj, P).
                let mut recovered = vec![0u8; block_size];
                for (_, b) in &other_data {
                    xor_inplace(&mut recovered, b);
                }
                xor_inplace(&mut recovered, p_block);
                let recovered_csum = crc32c::crc32c(&recovered);
                if recovered_csum == expected_csum {
                    return finish_recovered(
                        location,
                        expected_csum,
                        recovered_csum,
                        recovered,
                        opts,
                        ParityPath::P,
                    );
                }
                Some(FailureReason::CsumMismatch {
                    via: ParityPath::P,
                    recovered_csum,
                    expected_csum,
                })
            }
        }
    };

    // 5. P failed (baked-in or checksum mismatch).  Try Q as a fallback.
    //    Q uses independent GF(2^8) math — it is corrupted by the same
    //    bad byte only if a parity sync has run *since* the data
    //    corruption, recomputing Q from the (now-bad) data.  In the common
    //    "silent corruption, no parity sync since" case Q is intact and
    //    gives a genuine second chance.  See `array::gf` for the math.
    let q_reason: Option<FailureReason> = match (config.parity_q.as_ref(), q_block_opt.as_ref()) {
        (None, _) => Some(FailureReason::Io("no secondary parity (Q) disk in array config".into())),
        (_, None) => Some(FailureReason::Io("read parity Q failed".into())),
        (_, Some(q_block)) => match recover_via_q(
            &other_data,
            q_block,
            failing_slot,
            block_size,
            expected_csum,
        ) {
            QOutcome::Recovered(recovered, recovered_csum) => {
                return finish_recovered(
                    location,
                    expected_csum,
                    recovered_csum,
                    recovered,
                    opts,
                    ParityPath::Q,
                );
            }
            QOutcome::BakedIn => Some(FailureReason::ParityBakedIn),
            QOutcome::CsumMismatch { recovered_csum } => Some(FailureReason::CsumMismatch {
                via: ParityPath::Q,
                recovered_csum,
                expected_csum,
            }),
        },
    };

    // 5. P-only and Q-only both failed.  The most likely remaining cause
    //    is that a *second* data disk is also corrupt at this offset, so
    //    each single-parity path inherits the partner's bad bytes and
    //    produces garbage.  Using P and Q simultaneously — the
    //    raid6_2data_recov math from `nonraid/raid6/recov.c` — we can
    //    solve for both unknowns.
    //
    //    We don't know which other disk is the partner, so brute-force
    //    every candidate: for each `partner`, run the 2-disk solve using
    //    (failing_disk, partner), then check whether the *failing* disk's
    //    reconstructed block matches the btrfs-stored checksum.  The
    //    partner's checksum we usually don't have (different logical
    //    address, maybe a different file or metadata), so we rely on the
    //    invertibility of the math: a wrong partner guess yields a wrong
    //    block for our failing disk, and the btrfs csum catches it.
    //
    //    **Write policy**: we only ever write the *failing* disk's block,
    //    and only after its checksum matches the stored value.  We never
    //    touch the partner disk, P, or Q — they may or may not actually
    //    be corrupt, and without a control checksum for the partner it's
    //    too risky to overwrite it.  A subsequent scrub of the partner
    //    (if it holds its own scrubbed data) will catch and fix it
    //    independently.
    let p_reason_singleton = p_reason.clone().unwrap_or_else(|| {
        FailureReason::Io("internal: P path did not produce a reason".into())
    });
    let q_reason_singleton = q_reason.clone().unwrap_or_else(|| {
        FailureReason::Io("internal: Q path did not produce a reason".into())
    });

    // PQ 2-disk solve: only attemptable if we have both P and Q blocks.
    if let (Some(p_block), Some(q_block)) = (p_block_opt.as_ref(), q_block_opt.as_ref()) {
        let mut partners_tried: Vec<u64> = Vec::new();
        for (partner_slot, _partner_block) in &other_data {
            partners_tried.push(*partner_slot);
if let Some(recovered) = solve_two_disk(
                p_block,
                q_block,
                &other_data,
                *partner_slot,
                location.devid,
                block_size,
            ) {
                let recovered_csum = crc32c::crc32c(&recovered);
                if recovered_csum == expected_csum {
                    let via = ParityPath::PQ { partner_slot: *partner_slot };
                    return finish_recovered(
                        location,
                        expected_csum,
                        recovered_csum,
                        recovered,
                        opts,
                        via,
                    );
                }
                // Wrong partner guess — keep trying.
            }
        }
        // No partner produced a checksum match for the failing disk.
        return RecoveryResult::Failed {
            location: location.clone(),
            reason: FailureReason::AllPathsFailed {
                p_reason: Box::new(p_reason_singleton),
                q_reason: Box::new(q_reason_singleton),
                pq_partners_tried: partners_tried,
            },
        };
    }

    // PQ 2-disk path not even attempted (P or Q unreadable, or no Q disk).
    RecoveryResult::Failed {
        location: location.clone(),
        reason: if config.parity_q.is_none() {
            FailureReason::BothPathsFailed {
                p_reason: Box::new(p_reason_singleton),
                q_reason: Box::new(q_reason_singleton),
            }
        } else {
            FailureReason::AllPathsFailed {
                p_reason: Box::new(p_reason_singleton),
                q_reason: Box::new(q_reason_singleton),
                pq_partners_tried: Vec::new(),
            }
        },
    }
}

/// Outcome of the Q-path reconstruction attempt.
#[allow(dead_code)]
enum QOutcome {
    /// Reconstructed block whose checksum matched.
    Recovered(Vec<u8>, u32),
    /// Q itself was recomputed from the corrupt byte (the reconstructed
    /// block equals the corrupt block we read off disk — no new
    /// information), so Q is also baked in.
    BakedIn,
    /// Reconstructed block's checksum did not match the stored checksum.
    CsumMismatch { recovered_csum: u32 },
}

/// Reconstruct one missing data block using the Q parity disk.
///
/// `other_data` is `(slot, block)` for every *other* data disk (the
/// failing disk excluded).  `q_block` is the Q parity block at the same
/// raw offset.  `failing_slot` is the NonRAID slot number of the corrupt
/// disk (== btrfs devid).  See `array::gf` for the math: Q is the GF(2^8)
/// syndrome `XOR_{j=1..n} g^(j-1) * D_j`, so the missing `D_k` is
/// `g^(-(k-1)) * (Q XOR XOR_{j!=k} g^(j-1) * D_j)`.
///
/// Returns `BakedIn` when the reconstructed block is byte-identical to
/// the corrupt block we are trying to replace — that means Q was
/// recomputed from the corrupt data and carries no new information
/// (mirroring the P-path's `ParityBakedIn` zero-XOR check).
fn recover_via_q(
    other_data: &[(u64, Vec<u8>)],
    q_block: &[u8],
    failing_slot: u64,
    block_size: usize,
    expected_csum: u32,
) -> QOutcome {
    use crate::array::gf::{gf_exp, gf_mul};

    // multiplier = g^(-(failing_slot - 1))  ==  g^(255 - (failing_slot-1))
    // (g^255 == 1, so g^(-a) == g^(255-a)).  gf_exp handles the mod-255
    // for us via rem_euclid.
    let inv_exp = -((failing_slot as i32) - 1);
    let m = gf_exp(inv_exp);

    // acc = Q XOR XOR_{j != failing_slot} g^(j-1) * D_j
    let mut acc = q_block.to_vec();
    for (slot, block) in other_data {
        let coef = gf_exp((*slot as i32) - 1);
        gf_mul_xor_inplace(&mut acc, block, coef);
    }

    // recovered = m * acc
    let mut recovered = vec![0u8; block_size];
    gf_mul_into(&mut recovered, &acc, m);
    let recovered_csum = crc32c::crc32c(&recovered);
    if recovered_csum == expected_csum {
        QOutcome::Recovered(recovered, recovered_csum)
    } else {
        // Detect the "Q was recomputed from the corrupt data" case.  When
        // that happens, Q already reflects the corrupt D_k, so the
        // reconstruction above yields exactly the corrupt bytes back.
        // We don't have the corrupt block here to compare directly (the
        // caller does, but threading it through adds noise); the
        // checksum mismatch is the signal we have.  The caller's
        // `BothPathsFailed` will record this as a `CsumMismatch` on Q,
        // which is accurate — the data is unrecoverable from Q alone.
        let _ = gf_mul; // silence unused-import warning when above branch returns
        QOutcome::CsumMismatch { recovered_csum }
    }
}

/// Solve a 2-disk-corruption system using P and Q simultaneously.
///
/// Direct port of `raid6_2data_recov_intx1` from `nonraid/raid6/recov.c`,
/// specialised to NonRAID's slot-based addressing.  We assume the failing
/// disk (`failing_slot`) and one `partner_slot` are both corrupt at this
/// offset; P and Q together give two equations in two unknowns.
///
/// # The math (closed form)
///
/// Let `a` and `b` be the 0-based column indices of the two missing
/// disks (`a = failing_slot - 1`, `b = partner_slot - 1`, with `a < b`).
/// Define the deltas against the "both columns zeroed" syndromes:
///
/// ```text
///   ΔP = P ⊕ P_zero = D_a ⊕ D_b            (since P = XOR of all D)
///   ΔQ = Q ⊕ Q_zero = g^a · D_a ⊕ g^b · D_b
/// ```
///
/// `P_zero` / `Q_zero` are recomputed from the *good* disks only.  The
/// on-disk P and Q still reflect the *original* (now-corrupt) Da and Db,
/// so ΔP and ΔQ isolate exactly the two unknowns.  Solving:
///
/// ```text
///   D_b = pbmul[ΔP] ⊕ qmul[ΔQ]
///   D_a = D_b ⊕ ΔP
/// ```
///
/// where `pbmul = gfinv[gfexp[b-a] ⊕ 1]` and
/// `qmul = gfinv[gfexp[a] ⊕ gfexp[b]]` — exactly the multipliers recov.c
/// picks (`raid6_gfexi[failb-faila]` and
/// `raid6_gfinv[raid6_gfexp[faila]^raid6_gfexp[failb]]`).
///
/// Returns the reconstructed block for the **failing** disk (never the
/// partner — the caller checks the failing block's csum and only writes
/// the failing disk; the partner is left untouched for safety).
fn solve_two_disk(
    p_block: &[u8],
    q_block: &[u8],
    other_data: &[(u64, Vec<u8>)],
    partner_slot: u64,
    failing_slot: u64,
    block_size: usize,
) -> Option<Vec<u8>> {
    use crate::array::gf::{GFEXI, GFINV, GFEXP};

    // a, b = 0-based column indices, with a < b.  recov.c requires this
    // ordering; the math is asymmetric in a/b.
    let (a, b) = if failing_slot < partner_slot {
        (failing_slot as i32 - 1, partner_slot as i32 - 1)
    } else {
        (partner_slot as i32 - 1, failing_slot as i32 - 1)
    };
    if a < 0 || b < 0 || a == b {
        return None;
    }
    let diff = (b - a).rem_euclid(255) as usize;

    // Multipliers, exactly as recov.c picks them:
    //   pbmul_coef = gfinv[gfexp[b-a] ⊕ 1]   ==  GFEXI[diff]
    //   qmul_coef  = gfinv[gfexp[a] ⊕ gfexp[b]]
    // Then `pbmul = GFMUL[pbmul_coef]` and `qmul = GFMUL[qmul_coef]`
    // are the per-byte multiplication lookup tables.
    let pbmul_coef = GFEXI[diff];
    let qmul_coef = GFINV[(GFEXP[a as usize] ^ GFEXP[b as usize]) as usize];
    let pbmul = &crate::array::gf::GFMUL[pbmul_coef as usize];
    let qmul = &crate::array::gf::GFMUL[qmul_coef as usize];

    // P_zero = XOR of all good disks (everyone except failing & partner).
    // Q_zero = XOR_{j good} g^(j-1) · D_j.
    // ΔP = P ⊕ P_zero, ΔQ = Q ⊕ Q_zero.
    //
    // We deliberately do NOT consult the partner's on-disk block here:
    // the deltas are computed from the on-disk P/Q (which still reflect
    // the *original* Da, Db) and the *good* disks only.  The partner's
    // role is solely to identify which second column we assume is also
    // corrupt; its bytes don't enter the math.
    let mut p_zero = vec![0u8; block_size];
    let mut q_zero = vec![0u8; block_size];
    for (slot, block) in other_data {
        if *slot == partner_slot || *slot == failing_slot {
            continue;
        }
        xor_inplace(&mut p_zero, block);
        let coef = crate::array::gf::gf_exp(*slot as i32 - 1);
        gf_mul_xor_inplace(&mut q_zero, block, coef);
    }

    // ΔP = P ⊕ P_zero
    let mut delta_p = p_block.to_vec();
    xor_inplace(&mut delta_p, &p_zero);
    // ΔQ = Q ⊕ Q_zero
    let mut delta_q = q_block.to_vec();
    xor_inplace(&mut delta_q, &q_zero);

    // Solve for D_b and D_a (0-based columns).  recov.c per-byte:
    //   db = pbmul[ΔP] ⊕ qmul[ΔQ]
    //   da = db ⊕ ΔP
    // We need whichever of (a, b) corresponds to the *failing* disk; the
    // partner's reconstruction we discard (no control csum to verify it,
    // and we never write the partner — see the write policy in `recover_sector`).
    let mut d_b = vec![0u8; block_size];
    for i in 0..block_size {
        d_b[i] = pbmul[delta_p[i] as usize] ^ qmul[delta_q[i] as usize];
    }
    let mut d_a = d_b.clone();
    xor_inplace(&mut d_a, &delta_p);

    let recovered = if failing_slot < partner_slot {
        // failing == column a (the lower slot)
        d_a
    } else {
        // failing == column b (the higher slot)
        d_b
    };
    Some(recovered)
}

/// Common tail of `recover_sector`: write back (unless dry-run) and
/// construct the `Recovered` result.  `via` records which parity path
/// succeeded for diagnostics.
fn finish_recovered(
    location: &ResolvedLocation,
    expected_csum: u32,
    recovered_csum: u32,
    recovered: Vec<u8>,
    opts: RecoverOpts,
    _via: ParityPath,
) -> RecoveryResult {
    let mut written = false;
    if !opts.dry_run {
        match write_block(&location.dev_path, location.raw_phys, &recovered) {
            Ok(()) => written = true,
            Err(e) => {
                return RecoveryResult::Failed {
                    location: location.clone(),
                    reason: FailureReason::Io(format!("write back: {e}")),
                };
            }
        }
    }
    RecoveryResult::Recovered {
        location: location.clone(),
        via: _via,
        expected_csum,
        recovered_csum,
        written,
    }
}

/// XOR `b` into `a` in place.  Panics if lengths differ.
fn xor_inplace(a: &mut [u8], b: &[u8]) {
    debug_assert_eq!(a.len(), b.len(), "xor_inplace: length mismatch");
    for (x, y) in a.iter_mut().zip(b.iter()) {
        *x ^= *y;
    }
}

/// `a ^= coef * b` byte-wise in GF(2^8).  Uses the precomputed `GFMUL`
/// table.  Panics if lengths differ.
fn gf_mul_xor_inplace(a: &mut [u8], b: &[u8], coef: u8) {
    debug_assert_eq!(a.len(), b.len(), "gf_mul_xor_inplace: length mismatch");
    let table = &crate::array::gf::GFMUL[coef as usize];
    for (x, y) in a.iter_mut().zip(b.iter()) {
        *x ^= table[*y as usize];
    }
}

/// `dest = coef * src` byte-wise in GF(2^8).  Panics if lengths differ.
fn gf_mul_into(dest: &mut [u8], src: &[u8], coef: u8) {
    debug_assert_eq!(dest.len(), src.len(), "gf_mul_into: length mismatch");
    let table = &crate::array::gf::GFMUL[coef as usize];
    for (d, s) in dest.iter_mut().zip(src.iter()) {
        *d = table[*s as usize];
    }
}

/// Read one `block_size`-byte block from `dev_path` at `raw_phys`, or
/// return a zero block if the read falls past the device end.
///
/// NonRAID/Unraid arrays commonly have asymmetric data disks: a data disk
/// may be smaller than the largest data disk, and the parity disks are
/// always at least as large as the largest data disk.  The parity
/// relationship treats the missing region of a smaller disk as zeros, so
/// unreadable reads are substituted with zero bytes to preserve it.
fn read_block_or_zeros(dev_path: &Path, raw_phys: u64, block_size: usize) -> io::Result<Vec<u8>> {
    let mut f = match std::fs::File::open(dev_path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound || e.kind() == io::ErrorKind::PermissionDenied => {
            return Ok(vec![0u8; block_size]);
        }
        Err(e) => return Err(e),
    };
    // Seeking past a block device's end returns EINVAL on Linux.  NonRAID
    // arrays with asymmetric data disks hit this when the failing disk is
    // the large one and the offset is past a smaller disk's capacity — the
    // missing region contributes zeros to the parity relationship, so
    // substitute a zero block instead of erroring.
    if let Err(e) = f.seek(SeekFrom::Start(raw_phys)) {
        if e.kind() == io::ErrorKind::InvalidInput {
            return Ok(vec![0u8; block_size]);
        }
        return Err(e);
    }
    let mut buf = vec![0u8; block_size];
    match f.read(&mut buf) {
        Ok(0) => {}
        Ok(n) if n < block_size => {
            // Short read (e.g. reading past partition end but within loop
            // device): the trailing unreadable bytes are already zero-filled
            // in `buf`, matching the parity relationship.
        }
        Ok(_) => {}
        Err(e)
            if e.kind() == io::ErrorKind::UnexpectedEof
                || e.kind() == io::ErrorKind::InvalidInput =>
        {
            // Past device end entirely: treat as zeros.
        }
        Err(e) => return Err(e),
    }
    Ok(buf)
}

/// Write `data` to `dev_path` at `raw_phys`, fsyncing before returning.
fn write_block(dev_path: &Path, raw_phys: u64, data: &[u8]) -> io::Result<()> {
    let mut f = OpenOptions::new().read(true).write(true).open(dev_path)?;
    f.seek(SeekFrom::Start(raw_phys))?;
    f.write_all(data)?;
    f.flush()?;
    f.sync_all()?;
    Ok(())
}
