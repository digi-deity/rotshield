//! Recovery result and failure-reason models.
//!
//! View layer for [`crate::recovery::engine`] — enums describing *what*
//! happened during a recovery attempt, without any I/O locations or
//! filesystem coupling.  These are produced by [`recover_block`] and
//! inspected by the caller (the integration glue in `main.rs`, which
//! knows about array device paths, the failing slot, and how to format a
//! diagnostic line).  Recovery stays out of the "verdict book-keeping"
//! business — the caller inspects [`RecoveryResult`] directly and keeps
//! its own counters; there is no `ScrubVerdict` enum here.
//!
//! [`recover_block`]: crate::recovery::engine::recover_block

/// All the chunks of one aligned stripe, plus the corrupt block itself.
///
/// Slots are 1-based NonRAID data-disk slot numbers (`1..=n`); the failing
/// disk is identified by `failing_slot` and its on-disk (corrupt) bytes
/// are `corrupt_block`.  `p_block` / `q_block` are the parity **on disk at
/// the same offset** — they may or may not still reflect original data
/// (the recovery engine detects when they have been recomputed from the
/// corrupt bytes — the "baked in" case — via
/// [`FailureReason::ParityBakedIn`]).  `None` for either parity indicates
/// the array has no such parity disk (e.g. single-parity arrays omit Q).
///
/// `other_blocks` is the `(slot, block)` pair for every *other* data disk
/// — the ones the corrupt disk's reconstructed block XORs/multiplies
/// against.  For asymmetric arrays a block past a smaller disk's end is
/// represented by an all-zero slice (the array-level parity convention;
/// the array layer that assembles this struct is responsible for the
/// substitution — see `array/`).
#[derive(Clone)]
pub struct RecoveryInput<'a> {
    /// 1-based NonRAID slot of the disk we are trying to recover.
    pub failing_slot: u64,
    /// The on-disk bytes of the corrupt block at this offset (size ==
    /// `block_size`, every block in this struct shares that length).  Used
    /// both to confirm corruption (the verifier should reject it) and to
    /// detect the "parity baked from the corrupt byte" case.
    pub corrupt_block: &'a [u8],
    /// `block_size`-byte chunk for each other data disk (`failing_slot`
    /// excluded), in arbitrary order.  All entries must be `block_size`
    /// bytes long.
    pub other_blocks: &'a [(u64, Vec<u8>)],
    /// Primary parity (`P = XOR of all data disks`) at this offset, or
    /// `None` if the array has no P disk.
    pub p_block: Option<&'a [u8]>,
    /// Secondary parity (`Q = XOR g^(slot-1) · D_slot`) at this offset,
    /// or `None` if the array has no Q disk (single-parity array).
    pub q_block: Option<&'a [u8]>,
    /// Verifier: returns `true` iff `block` is the correct original data
    /// for this offset (e.g. `crc32c::crc32c(block) == expected_csum` for
    /// btrfs, or `block == &golden[..]` in unit tests).  Recovery has no
    /// other way to confirm success — parity math only produces
    /// candidates; the verifier is what rules the bad ones out.  The
    /// caller owns the checksum algorithm so ZFS edonr/sha just replaces
    /// this closure.
    pub verifier: &'a dyn Fn(&[u8]) -> bool,
}

impl<'a> std::fmt::Debug for RecoveryInput<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecoveryInput")
            .field("failing_slot", &self.failing_slot)
            .field("corrupt_block_len", &self.corrupt_block.len())
            .field("other_blocks_n", &self.other_blocks.len())
            .field("p_block_len", &self.p_block.map(|b| b.len()))
            .field("q_block_len", &self.q_block.map(|b| b.len()))
            .field("verifier", &"<closure>")
            .finish()
    }
}

/// Which parity disk(s) a successful recovery attempt used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParityPath {
    /// Single-parity: XOR of all other data disks plus P.
    P,
    /// Single-parity: GF(2^8) reconstruction from Q.
    Q,
    /// Dual-parity: P and Q used simultaneously to solve a 2-disk
    /// corruption.  `partner_slot` is the *other* slot we assumed was
    /// also corrupt at this offset (we brute-force every candidate partner
    /// and the verifier confirms which one was right).
    PQ { partner_slot: u64 },
}

/// Outcome of a single block recovery attempt.
#[derive(Debug)]
pub enum RecoveryResult {
    /// Reconstructed a block the verifier accepted.  `block` is the
    /// recovered bytes (already `block_size` long); the caller writes it
    /// back to the failing disk if it wants to.
    Recovered { via: ParityPath, block: Vec<u8> },
    /// The failing disk already matches the verifier — it was not
    /// actually corrupt when we got to it (the scrub may have raced with
    /// a kernel rewrite).  No write is needed.
    NotCorrupt,
    /// Recovery failed; `reason` says which paths were tried and why each
    /// declined.  Strictly diagnostic — the caller logs it.
    Failed { reason: FailureReason },
}

/// Why a recovery attempt failed.
#[derive(Debug, Clone)]
pub enum FailureReason {
    /// Attempted to recover via the single-parity path `via`, but the
    /// reconstructed block's verifier returned `false`.  Pure engine stays
    /// checksum-agnostic — no recovered value is stored here.
    CsumMismatch { via: ParityPath },
    /// Parity has been recomputed from the corrupt byte (the "baked in"
    /// case) — this path yields a block byte-identical to the corrupt one
    /// and carries no new information.  Distinct from `CsumMismatch`
    /// because the caller may want to log it differently ("Q is burned,
    /// try the PQ path" vs "we got something different but wrong").
    ParityBakedIn { via: ParityPath },
    /// I/O failure inside the array layer that assembled
    /// [`RecoveryInput::other_blocks`] / parity — we surface the message
    /// here so the caller can log it even though the pure engine itself
    /// performs no I/O.  In practice this is filled in by the integration
    /// glue in `main.rs` when its [`crate::array::stripe::gather_stripe`]
    /// call fails to read a disk.
    Io(String),
    /// Both single-parity paths (P and Q) failed *and* the PQ 2-disk
    /// solve with every candidate partner failed to verify.  `p_reason` /
    /// `q_reason` carry the single-path diagnostics; `pq_partners_tried`
    /// lists the partner slots actually attempted (empty if the PQ solve
    /// was impossible, e.g. P or Q absent).
    AllPathsFailed {
        p_reason: Box<FailureReason>,
        q_reason: Box<FailureReason>,
        pq_partners_tried: Vec<u64>,
    },
    /// The array has no Q disk and the P path failed — no PQ fallback was
    /// even attemptable.  Reported instead of [`AllPathsFailed`] when Q is
    /// absent, so callers can distinguish "exhausted all paths" from
    /// "Q unavailable, would have tried PQ".
    NoQPathAndPFailed { p_reason: Box<FailureReason> },
}

