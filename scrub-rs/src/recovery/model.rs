//! Recovery input and outcome model.

#[derive(Clone)]
pub struct RecoveryInput<'a> {
    /// 1-based NonRAID data-disk slot of the corrupt disk.
    pub failing_slot: u64,
    /// On-disk bytes of the corrupt block; a zero placeholder when
    /// `unreadable_source` is set.
    pub corrupt_block: &'a [u8],
    /// The source read failed with EIO, so `corrupt_block` is not the
    /// real data — the engine skips the checks that compare against it.
    pub unreadable_source: bool,
    /// `(slot, block)` for every other data disk (failing slot excluded).
    pub other_blocks: &'a [(u64, Vec<u8>)],
    /// P parity (XOR of all data disks) at this offset; `None` if the
    /// array has no P disk.
    pub p_block: Option<&'a [u8]>,
    /// Q parity (GF(2^8) row syndrome) at this offset; `None` if the
    /// array has no Q disk.
    pub q_block: Option<&'a [u8]>,
    /// Returns `true` iff `block` is the correct original data for this
    /// offset (e.g. a checksum match). The only way recovery confirms a
    /// reconstructed candidate.
    pub verifier: &'a dyn Fn(&[u8]) -> bool,
}

impl<'a> std::fmt::Debug for RecoveryInput<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecoveryInput")
            .field("failing_slot", &self.failing_slot)
            .field("corrupt_block_len", &self.corrupt_block.len())
            .field("unreadable_source", &self.unreadable_source)
            .field("other_blocks_n", &self.other_blocks.len())
            .field("p_block_len", &self.p_block.map(|b| b.len()))
            .field("q_block_len", &self.q_block.map(|b| b.len()))
            .field("verifier", &"<closure>")
            .finish()
    }
}

/// Which parity path reconstructed the block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParityPath {
    /// P-only XOR path.
    P,
    /// Q-only GF(2^8) path.
    Q,
    /// Two-disk solve; `partner_slot` is the other disk reconstructed
    /// simultaneously from the same P/Q equations.
    PQ { partner_slot: u64 },
}

/// Outcome of one recovery attempt.
#[derive(Debug)]
pub enum RecoveryResult {
    /// A candidate passed the verifier; `block` is the reconstructed bytes.
    Recovered { via: ParityPath, block: Vec<u8> },
    /// The failing block already passes the verifier — no recovery needed.
    NotCorrupt,
    /// No parity path produced a verifiable block.
    Failed { reason: FailureReason },
}

/// Why recovery failed (aggregated from every path that was tried).
#[derive(Debug, Clone)]
pub enum FailureReason {
    /// Reconstruction via `via` did not pass the verifier.
    CsumMismatch { via: ParityPath },
    /// Parity was recomputed from the corrupt bytes after the fact, so
    /// reconstructing via `via` just reproduces the corruption.
    ParityBakedIn { via: ParityPath },
    /// The array has no such parity disk (`P` or `Q`).
    ParityAbsent { via: ParityPath },
    /// Internal invariant violation in the recovery engine.
    InternalInconsistency(String),
    /// Both single paths failed; the two-disk solve (when both P and Q are
    /// present) tried each other disk as the partner (`pq_partners_tried`)
    /// without a verifiable result.
    AllPathsFailed {
        p_reason: Box<FailureReason>,
        q_reason: Box<FailureReason>,
        pq_partners_tried: Vec<u64>,
    },
    /// Single-parity array (no Q) and the P path failed.
    NoQPathAndPFailed { p_reason: Box<FailureReason> },
}
