//! Filesystem-scrub contract — the interface a filesystem implementation
//! (`btrfs/`, a future `zfs/`) fulfils so `main` can drive parity recovery
//! without knowing which on-disk format it's reading.
//!
//! The design intent: keep the filesystem specifics (btrfs tree walks,
//! checksum formats) inside the filesystem implementation, and pass only
//! what recovery actually needs across the interface.
//!
//! ## Two output streams
//!
//! A scrub produces two distinct kinds of output, kept on separate
//! callbacks because they have *different lifetimes and different
//! audiences*:
//!
//! - **Log lines** ([`ScrubCallbacks::on_log`]) — human-readable text,
//!   free-form, fully owned by the filesystem implementation.  btrfs emits
//!   `MISMATCH logical=0x… devid=N ino=N off=0x… stored=0x… actual=0x…`;
//!   ZFS will emit `DVA vdev=N offset=0x… blkptr=… stored=edonr:…`.  The
//!   CLI's logger just `eprintln!`s whatever string arrives — it has no
//!   fields to decode, no width assumptions, no algorithm policy to
//!   enforce.  This is where "checksums are 4-byte little-endian" or
//!   "checksums are 32-byte sha256 truncated to 8 chars for log brevity"
//!   lives, in the implementation that actually knows.
//!
//! - **Recovery events** ([`ScrubCallbacks::on_event`], carrying
//!   [`ScrubEvent`]) — the narrow correctness-only payload: which disk
//!   byte offset, what block size, a `verify` closure that decides "is
//!   this candidate the original data?", and a filesystem-opaque
//!   [`ReconfirmRequest`] for deferred re-checking at write time.  No
//!   checksum bytes, no algorithm name, no log context.  The CLI's
//!   `on_event` hands the closure straight to
//!   [`crate::recovery::RecoveryInput::verifier`] and never inspects it.
//!   This is the seam that has to be stable across filesystems; the log
//!   seam is allowed to be format-flavoured.
//!
//! Splitting them keeps the recovery payload small and algorithm-agnostic,
//! while logging — which is genuinely format-specific (every filesystem
//! has its own diagnostic vocabulary) — is owned by the filesystem
//! implementation that has context to format it well.  A future ZFS
//! implementation never has to ask "what hex width does the recovery layer
//! expect?" because the answer is "none, format your own log string".
//!
//! ## Why a callback and not `Iterator`
//!
//! `Iterator` would be idiomatic but pushes state management into the
//! implementation (closures, generators, or manual state machines) and
//! forces the caller into `for` loops where they can't easily
//! short-circuit or mutate shared recovery counters in the same scope.
//! The callback shape (`run(&mut dyn ScrubCallbacks)`) matches what the
//! scrub already does internally (it has to walk B-trees in order anyway)
//! and gives the caller one obvious place for logging + recovery glue.
//!
//! ## The verifier is built by the event, not the caller
//!
//! Different filesystems use different checksum algorithms (btrfs crc32c,
//! 4 bytes little-endian; ZFS fletcher2/fletcher4/sha256/blake3/edonr,
//! 4–48 bytes).  Rather than pass the stored csum through a fixed-width
//! field and ask the caller to re-derive the verifier, the scrub builds
//! **the verifier itself** per event.  The btrfs implementation captures
//! `|b| crc32c::crc32c(b) == stored`; a ZFS implementation captures
//! `|b| sha256(b).as_slice() == stored`.  The recovery layer takes a
//! `&dyn Fn(&[u8]) -> bool` and never asks what algorithm produced it —
//! broad enough to cover any checksum.
//!
//! ## Deferred re-confirmation
//!
//! Re-confirming "is this sector *still* corrupt right before I write?" is
//! filesystem-specific (btrfs checks the live EXTENT_TREE/CSUM_TREE at the
//! logical address; ZFS would consult its own live metadata).  So the event
//! carries a filesystem-opaque [`ReconfirmRequest`], and the filesystem
//! implementation exposes a [`Reconfirmer`] handle the batched writer calls
//! at write time.  The recovery glue never interprets the request — it just
//! carries it from the event back to the [`Reconfirmer`] the same
//! filesystem produced.
/// Recovery-only payload emitted by a filesystem scrub per mismatched
/// sector.
///
/// Kept small: just `array_phys`, `block_size`, a `verify` closure, and a
/// filesystem-opaque re-confirm request.  No checksum bytes, no algorithm
/// name, no log context — those live on the [`ScrubCallbacks::on_log`]
/// stream, which the filesystem implementation owns end-to-end.  Recovery
/// only needs to know *where on disk*, *is this candidate right*, and *how
/// to re-check the mismatch at write time*.
///
/// `verify` carries `Send + Sync` so the event (and the closure it holds)
/// can be moved across threads if the caller ever wants a parallel
/// scrub — btrfs's `crc32c` and ZFS's sha256 closures are both trivially
/// thread-safe since they only read their captured bytes.
use std::sync::Arc;

/// A thread-safe closure that returns `true` iff `candidate` is the correct
/// original data for a given sector.  Captured per-event by the filesystem
/// implementation together with the right checksum algorithm, so the
/// recovery layer can verify a reconstructed block without knowing which
/// on-disk format produced it.
pub type SectorVerifier = Arc<dyn Fn(&[u8]) -> bool + Send + Sync>;

pub struct ScrubEvent {
    /// Byte offset on the failing disk's **array partition**
    /// (`/dev/nmd1p1`-space).  The array layer adds `rdevOffset` to reach
    /// the raw-rdev location for the recovery read.
    pub array_phys: u64,
    /// Sector size in bytes — btrfs is always 4096 here.  Passed per-event
    /// so a future mixed-record-size filesystem (ZFS variable records)
    /// can emit events of different `block_size` from a single scrub.
    pub block_size: usize,
    /// Integrity verifier for this sector: returns `true` iff `candidate`
    /// is the correct original data for this offset.  `None` for a sector
    /// that has no stored checksum — recovery cannot verify a candidate
    /// against `None`, so the caller should skip recovery for this event.
    /// Otherwise the filesystem implementation has already captured the
    /// stored value and bound it together with the right algorithm into
    /// this closure; the caller passes it straight into
    /// [`crate::recovery::RecoveryInput::verifier`].
    ///
    /// Stored as `Arc` (not `Box`) so a recovery sink can clone it onto a
    /// channel for a batched writer thread without re-deriving the
    /// algorithm.
    pub verify: Option<SectorVerifier>,
    /// Filesystem-opaque re-confirmation request.  Carried so a recovery
    /// sink can defer re-confirmation to write time: the batched writer
    /// hands it back to the filesystem-owned [`Reconfirmer`], which knows
    /// how to check whether this sector is *still* corrupt right before
    /// the write.  `None` when the sector has no stored checksum (nothing
    /// to re-confirm against).
    pub reconfirm: Option<ReconfirmRequest>,
    /// The sector's source bytes were **unreadable** (the device returned
    /// `EIO`) when the scrub tried to read them — as opposed to a sector
    /// that read fine but whose checksum mismatched.  This is the clearest
    /// "the disk is broken" signal, and exactly the case parity recovery
    /// was built for, so the recovery sink must NOT re-read the failing
    /// disk for its self-heal pre-check (it will just `EIO` again); it
    /// recovers from parity with a zero placeholder for the corrupt block.
    ///
    /// When `true`, `verify` is still populated (built from the stored
    /// csum), which is what makes parity recovery verifiable despite the
    /// source bytes being unreadable.  Recovery is skipped entirely when
    /// `true` AND `verify` is `None` (no stored csum to confirm against).
    pub unreadable: bool,
}

/// Filesystem-opaque re-confirmation request, captured by the filesystem
/// implementation per mismatched sector and handed back to its own
/// [`Reconfirmer`] at write time.
///
/// The fields are deliberately opaque to the seam: `token` is whatever the
/// filesystem needs to locate the live metadata for this sector (btrfs:
/// the logical address), and `stored_csum` is the expected checksum from
/// the scrub's snapshot that must be compared against the *live* expected
/// checksum to decide stale-vs-corrupt.  Only the filesystem
/// implementation interprets these; the recovery glue just carries them.
#[derive(Clone, Debug)]
pub struct ReconfirmRequest {
    /// Filesystem-internal location token for the sector (btrfs logical).
    pub token: u64,
    /// The stored (expected) checksum bytes from the scrub's snapshot.
    pub stored_csum: Vec<u8>,
}

/// Verdict of a deferred re-confirmation at write time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reconfirm {
    /// Still genuine corruption: the live metadata still expects the
    /// stored checksum and the sector is still owned.  Recover + write.
    Corruption,
    /// Benign churn: the sector was freed / rewritten / written without a
    /// checksum since the scan.  Nothing to write.
    Stale,
    /// The metadata covering this sector could not be read, so we cannot
    /// safely re-confirm it.  Skip the write for just this sector.
    Unverifiable,
}

/// A filesystem-owned handle that re-confirms a deferred mismatch against
/// the *live* metadata at write time.
///
/// The scrub walker runs on one thread with its own reader; a batched
/// recovery writer runs concurrently and must not share that reader, so
/// the filesystem implementation hands the recovery glue an *independent*
/// re-confirmation handle (own file handles, own chunk map) via
/// [`FilesystemScrub::reconfirmer`].  The glue never interprets the
/// [`ReconfirmRequest`] — it just calls back into the filesystem that
/// produced the event.
///
/// `Send` because the writer owns it on its own thread.
pub trait Reconfirmer: Send {
    /// Re-confirm one deferred mismatch against the live metadata.
    fn reconfirm(&mut self, req: &ReconfirmRequest) -> Reconfirm;
}

/// Rollup result returned by [`FilesystemScrub::run`] when scrubbing is
/// complete — equivalent to btrfs-scrub's per-device summary.
#[derive(Debug, Default, Clone)]
pub struct ScrubStats {
    pub sectors_checked: u64,
    pub sectors_ok: u64,
    pub sectors_mismatch: u64,
    pub sectors_no_csum: u64,
    pub sectors_read_error: u64,
    /// Sectors whose stored csum did NOT match the on-disk data, but which
    /// the EXTENT_TREE shows are no longer owned by a live data extent (the
    /// csum entry is orphaned/freed, or the extent was written `nodatasum`).
    /// These are benign churn artifacts, NOT corruption, so they are NOT
    /// counted in `sectors_mismatch` and do NOT trigger recovery.  Surfaced
    /// as a distinct counter so a mismatch tally stays honest and an
    /// operator can see how much stale noise was filtered out.  Does not
    /// affect the exit code (only mismatch / read-error / metadata-header
    /// errors do).
    pub sectors_stale: u64,
    /// CSUM_TREE branches skipped as **stale mid-scrub** (freed/rewritten
    /// by a live transaction while the run was in progress): their sectors
    /// were NEVER verified this run.  This is a *coverage* counter, not a
    /// metadata error — stale is normal churn (see `csum.rs`), so it does
    /// NOT trigger METADATA FATAL — but a non-zero value means the run did
    /// not check everything, so `main` refuses exit 0 while it is non-zero
    /// (the operator reruns the scrub to cover those sectors).
    pub stale_csum_branches: u64,
    /// Read-runs where the EIO divide-and-conquer isolation budget was
    /// exhausted (see `scrub.rs` `IsolationBudget`): the remaining sectors
    /// of the run were marked unreadable without further probing.  They are
    /// counted as `sectors_read_error` (so exit 0 is already blocked) and
    /// flow through the unreadable parity-recovery path, never silently
    /// skipped.  This counter just says *how many runs* were truncated, so
    /// the summary explains why the read-error count is large.
    pub isolation_truncated: u64,
    pub bytes_checked: u64,
    /// Metadata nodes whose *all* mirror copies failed header-checksum
    /// verification (DUP/RAID1 metadata with no good copy).  A single
    /// corrupt copy that has a good sibling is transparently recovered by
    /// the DUP cross-check and is *not* counted here.  This is a distinct
    /// failure class from data-sector mismatches: it means the scrub could
    /// not trust the metadata it needed to traverse, so some data may have
    /// been silently skipped.  Surfaced as a hard error (non-zero → non-zero
    /// exit) so it can't be mistaken for a clean scrub.
    pub metadata_header_errors: u64,
    /// Metadata nodes in a *mirrored* (DUP/RAID1/…) chunk whose copies
    /// DISAGREE with each other: at least one copy is header-checksum
    /// valid, but the copies are not byte-identical.  This is the
    /// self-heal-recoverable counterpart to `metadata_header_errors` — the
    /// filesystem can still read the good copy (so traversal succeeds), but
    /// a correct scrub should *report* the divergence the way the kernel's
    /// `btrfs scrub` does, rather than healing it silently.  Counted
    /// separately so an operator can see how many mirrored metadata blocks
    /// are out of sync without conflating them with unrecoverable header
    /// errors.  Does not affect the exit code on its own (the good copy
    /// means the data is intact), but is surfaced for visibility.
    pub metadata_mirror_mismatches: u64,
    /// Metadata nodes that failed with a **read (device `EIO`) error** — as
    /// opposed to a header-checksum failure (which is `metadata_header_errors`).
    /// An `EIO` is hardware (the disk cannot return the bytes); a checksum
    /// failure is corruption.  Both mean the scrub could not trust the
    /// metadata it needed, so some data may have been left unverified — but
    /// the operator response differs ("check the disk hardware" vs "run
    /// `btrfs check --repair`"), so they are counted separately and the
    /// scrub continues past the unreadable leaf rather than aborting the
    /// whole tree walk.
    ///
    /// Surfaced as a hard error (non-zero → non-zero exit) so a scrub that
    /// lost metadata coverage can never report clean.
    pub metadata_read_errors: u64,
}

/// Sink for the two streams a scrub produces (see module docs).
///
/// Implementations are expected to be trivial: in `main`, one `eprintln`
/// for `on_log` and the inline recovery glue for `on_event`.  Treating it
/// as a trait (rather than two `&mut dyn FnMut(...)` arguments) lets the
/// caller keep its mutable counters in a single struct, and lets future
/// callers (a TUI, a JSON exporter) add a third method without rewriting
/// every scrub site.
pub trait ScrubCallbacks {
    /// Receive a free-form, fully-formatted log line.  No structured
    /// fields — the filesystem implementation has already decided the
    /// format and (where relevant) the checksum-abbreviation policy.
    /// Implementations typically `eprintln!("{line}")` or stash the line
    /// into a richer sink.
    fn on_log(&mut self, line: &str);

    /// Receive a recovery-only [`ScrubEvent`] for one mismatched sector.
    /// Only carries what recovery needs; no checksum bytes or log context.
    fn on_event(&mut self, ev: &ScrubEvent);

    /// Whether this sink consumes **raw** (un-reconfirmed) mismatch
    /// candidates, so it can defer re-confirmation to write time (batched
    /// recovery).  When `true`, the filesystem implementation emits every
    /// csum mismatch as a raw [`ScrubEvent`] and does *not* classify it —
    /// the sink owns mismatch/stale accounting.  When `false` (default),
    /// the implementation re-confirms inline and only emits events it has
    /// already classified as corruption.
    ///
    /// The batched recovery driver in `main` returns `true` whenever it
    /// has a writer thread running; plain (array-less) scrubs keep the
    /// default `false` so the filesystem's inline accounting is used.
    fn wants_raw_candidates(&self) -> bool {
        false
    }
}

/// The contract a filesystem implementation fulfils so `main` can drive
/// parity recovery without knowing which on-disk format it's reading.
///
/// Implementations are constructed with the backing store they'll read
/// (a `File` over a block device or image) and a `base_offset` for where
/// the filesystem's partition starts inside that backing store (0 for a
/// bare image or an array partition like `/dev/nmd1p1`; the per-disk
/// `rdevOffset` for a whole-disk raw rdev like `/dev/loop2`).  They parse
/// their format-specific superblock / uberblock, walk their metadata
/// trees, verify each data sector's checksum, and — for every mismatched
/// or no-csum sector — call [`ScrubCallbacks::on_log`] with a
/// filesystem-formatted diagnostic line and
/// [`ScrubCallbacks::on_event`] with the narrow recovery payload.
///
/// `main` is then a thin driver:
///
/// 1. instantiate the right filesystem scrub (today hard-coded to btrfs —
///    a future `--fstype` flag chooses between `BtrfsScrub` and
///    `ZfsScrub`);
/// 2. resolve the NonRAID slot the backing store belongs to, load the
///    array config if an array is present;
/// 3. call `scrub.run(&mut callbacks)` where `callbacks` routes log
///    lines to `eprintln!` and recovery events through the array +
///    recovery glue.
pub trait FilesystemScrub {
    /// Run the scrub, driving `callbacks` per mismatched / no-csum
    /// sector.  Returns the aggregate stats on success or the first
    /// fatal I/O / format error.  Implementations may also emit
    /// progress / informational log lines through `on_log` outside the
    /// per-sector loop (e.g. "walking chunk tree: 24 leaves") — the
    /// contract doesn't police log volume.
    ///
    /// Whether mismatches are emitted raw (deferred re-confirmation) or
    /// re-confirmed inline is decided by
    /// [`ScrubCallbacks::wants_raw_candidates`].  The filesystem
    /// implementation never freezes and never writes; the recovery sink
    /// owns the freeze and the write-back.
    fn run(&mut self, callbacks: &mut dyn ScrubCallbacks) -> std::io::Result<ScrubStats>;

    /// Build an **independent** re-confirmation handle for a concurrent
    /// recovery writer thread.  The handle owns its own file handles and
    /// chunk map, so it never shares (or races) the scrub's reader.
    fn reconfirmer(&self) -> std::io::Result<Box<dyn Reconfirmer>>;

    /// Multi-line header describing this filesystem for the CLI's
    /// pre-scrub dump (device, format version, checksum strategy,
    /// geometry, …).  Format is owned by the implementation.
    fn describe(&self) -> Vec<String>;

    /// Byte offset of this filesystem's primary superblock within its
    /// partition — the fact the startup array-config canary needs to know
    /// where to reconstruct from parity.
    fn superblock_offset(&self) -> u64;

    /// Whether a raw block of bytes carries this filesystem's magic — the
    /// fact the startup array-config canary uses to recognise a correctly
    /// reconstructed superblock.
    fn block_has_magic(&self, block: &[u8]) -> bool;
}
