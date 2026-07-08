//! Parity-based block recovery engine — pure, I/O-free, checksum-agnostic.
//!
//! This module is the **recovery duty** in scrub-rs's separation of
//! responsibilities:
//!
//! ```text
//!   filesystem (btrfs/)  →  array (array/)  →  recovery (recovery/)
//!   scrub + offset +       gather aligned       given the aligned chunks
//!   checksum               chunks across disks  and a checksum-verifier,
//!                          for one offset        reconstruct the missing
//!                                                block via P / Q / PQ and
//!                                                verify the candidate.
//! ```
//!
//! `recovery/` knows nothing about disks, files, btrfs, ZFS, or any
//! specific checksum algorithm. It takes plain byte slices (the aligned
//! chunk for every disk at one offset, plus an optional P and Q chunk)
//! and a **verifier closure** that decides whether a candidate block is
//! "good". Recovery cannot tell correct from garbage on its own — the
//! parity math only produces candidates; the caller's verifier (typically
//! `crc32c::crc32c(block) == expected_csum` for btrfs, or `block ==
//! &golden[..]` in tests) is what confirms success. Keeping the verifier
//! pluggable means a drop-in ZFS edonr/sha checksum later replaces one
//! closure instead of touching the recovery math, and it makes the math
//! fully unit-testable: tests fabricate random chunks, compute their
//! XOR/GF syndromes, corrupt one, and assert the reconstructed candidate
//! passes their verifier — without touching a single block device.
//!
//! # The NonRAID parity relationship
//!
//! For `n` data disks at slots `1..=n` (slot `s` → 0-based column
//! `c = s - 1`):
//!
//! ```text
//!   P = XOR_{j=1..n} D_j
//!   Q = XOR_{j=1..n} g^(j-1) · D_j          (g = 2, generator of GF(2^8))
//! ```
//!
//! ## Single-failure recovery
//!
//! If only `D_k` is unknown and the parity disks still reflect the
//! *original* data (i.e. they were not recomputed from the corrupt bytes):
//!
//! ```text
//!   P-only:   D_k = P XOR XOR_{j≠k} D_j
//!   Q-only:   D_k = g^(-(k-1)) · (Q XOR XOR_{j≠k} g^(j-1) · D_j)
//! ```
//!
//! If parity *has* been resynced from the corrupt byte (we call this
//! "parity baked in"), the corresponding formula yields a block that
//! fails the verifier, and we fall back to the next path.
//!
//! ## Double-failure recovery
//!
//! If a *second* data disk `D_m` is also corrupt at the same offset,
//! the single-parity formulae each inherit the partner's bad bytes and
//! fail. Using P and Q simultaneously (the `raid6_2data_recov` math from
//! `nonraid/raid6/recov.c`) gives two equations in the two unknowns and
//! isolates each. We don't know which disk is the partner ahead of time,
//! so [`engine::recover_block`] brute-forces every candidate partner and
//! keeps the first candidate for the failing disk that the verifier
//! accepts.
//!
//! # Baked-in detection
//!
//! When parity has been recomputed from the *corrupt* block, the single-
//! parity reconstruction collapses to the corrupt bytes themselves and
//! carries no new information. [`engine::recover_block`] detects this
//! explicitly (the reconstructed candidate is byte-identical to the
//! on-disk corrupt block) and reports
//! [`model::FailureReason::ParityBakedIn`] rather than wasting a verifier
//! call. The call site still owns the corrupt block, so it's the one that
//! decides "this reconstruction added nothing".

pub mod engine;
pub mod gf;
pub mod model;

pub use engine::{recover_block, recover_via_p, recover_via_q, solve_two_disk};
pub use model::{
    FailureReason, ParityPath, RecoveryInput, RecoveryResult,
};