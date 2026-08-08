//! Filesystem-scrub contract: what a filesystem implementation (btrfs,
//! a future zfs) must fulfil so the CLI can drive scrubbing and parity
//! recovery without knowing the on-disk format.
use std::sync::Arc;

/// Returns true iff `candidate` is the correct original data for a sector.
/// Captured per event by the filesystem implementation (e.g. crc32c of the
/// block equals the stored checksum).
pub type SectorVerifier = Arc<dyn Fn(&[u8]) -> bool + Send + Sync>;

/// Recovery payload for one mismatched sector, emitted by the scrub.
pub struct ScrubEvent {
    /// Byte offset on the failing disk's array partition (/dev/nmdNp1 space).
    pub array_phys: u64,

    /// Sector size in bytes (4096 for btrfs).
    pub block_size: usize,

    /// Verifier for this sector; None when it has no stored checksum
    /// (recovery must be skipped).
    pub verify: Option<SectorVerifier>,

    /// Opaque re-confirmation request for write-time re-checking; None when
    /// there is no stored checksum.
    pub reconfirm: Option<ReconfirmRequest>,

    /// The sector was unreadable (device EIO): the corrupt bytes are unknown,
    /// so recovery uses a zero placeholder and must not re-read the disk.
    pub unreadable: bool,
}

/// Filesystem-opaque re-confirmation request; interpreted only by the
/// filesystem's own Reconfirmer at write time.
#[derive(Clone, Debug)]
pub struct ReconfirmRequest {
    /// Filesystem-internal location token (btrfs: the logical address).
    pub token: u64,

    /// Expected checksum bytes from the scrub's snapshot.
    pub stored_csum: Vec<u8>,
}

/// Verdict of a deferred re-confirmation at write time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reconfirm {
    /// Still genuinely corrupt — recover and write.
    Corruption,

    /// Sector freed or rewritten since the scan — nothing to write.
    Stale,

    /// Live metadata unreadable — skip just this sector.
    Unverifiable,
}

/// Filesystem-owned handle that re-confirms deferred mismatches against live
/// metadata at write time; independent of the scrub's reader.
pub trait Reconfirmer: Send {
    fn reconfirm(&mut self, req: &ReconfirmRequest) -> Reconfirm;
}

/// Rollup of one scrub run, equivalent to btrfs scrub's per-device summary.
#[derive(Debug, Default, Clone)]
pub struct ScrubStats {
    pub sectors_checked: u64,
    pub sectors_ok: u64,
    pub sectors_mismatch: u64,
    pub sectors_no_csum: u64,
    pub sectors_read_error: u64,

    /// Mismatches whose extent is no longer live (freed/rewritten or written
    /// nodatasum) — benign churn, not corruption.
    pub sectors_stale: u64,

    /// CSUM-tree branches skipped as stale mid-scrub; those sectors were never
    /// verified this run. Expected churn on a live filesystem — informational
    /// only, not an error.
    pub stale_csum_branches: u64,

    /// Read runs truncated when the EIO isolation budget was exhausted.
    pub isolation_truncated: u64,
    pub bytes_checked: u64,

    /// Metadata nodes whose every mirror copy failed header-checksum
    /// verification — traversal coverage was lost.
    pub metadata_header_errors: u64,

    /// Mirrored metadata nodes whose copies disagreed (a good copy existed) —
    /// reported, not silently healed.
    pub metadata_mirror_mismatches: u64,

    /// Metadata nodes that failed with a read (EIO) error — hardware, not
    /// corruption.
    pub metadata_read_errors: u64,
}

/// Sink for the two streams a scrub produces: log lines and recovery events.
pub trait ScrubCallbacks {
    /// Receive a free-form, fully formatted diagnostic line.
    fn on_log(&mut self, line: &str);

    /// Receive the recovery payload for one mismatched sector.
    fn on_event(&mut self, ev: &ScrubEvent);

    /// True when the sink classifies mismatches itself (deferred
    /// re-confirmation); the scrub then emits every mismatch raw.
    fn wants_raw_candidates(&self) -> bool {
        false
    }
}

/// The contract a filesystem implementation fulfils so the CLI can drive
/// scrubbing and parity recovery without knowing the on-disk format.
pub trait FilesystemScrub {
    /// Run the scrub, driving callbacks per mismatched sector; returns
    /// aggregate stats or the first fatal error.
    fn run(&mut self, callbacks: &mut dyn ScrubCallbacks) -> std::io::Result<ScrubStats>;

    /// Build an independent re-confirmation handle for a concurrent writer
    /// thread.
    fn reconfirmer(&self) -> std::io::Result<Box<dyn Reconfirmer>>;

    /// Multi-line header describing the filesystem for the pre-scrub dump.
    fn describe(&self) -> Vec<String>;

    /// Byte offset of the primary superblock within the partition (for the
    /// parity canary).
    fn superblock_offset(&self) -> u64;

    /// Whether a raw block carries this filesystem's magic (canary
    /// recognition).
    fn block_has_magic(&self, block: &[u8]) -> bool;
}
