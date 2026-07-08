//! Filesystem-scrub contract — the boundary a filesystem implementation
//! (`btrfs/`, a future `zfs/`) fulfils so `main` can drive parity recovery
//! without knowing which on-disk format it's reading.
//!
//! This is the **filesystem duty** boundary from scrub-rs's separation of
//! responsibilities:
//!
//! ```text
//!   fs::FilesystemScrub  ──▶  array::stripe  ──▶  recovery::engine
//!   (filesystem-specific)     (chunk-gathering)    (pure parity math)
//! ```
//!
//! ## Two streams, one seam
//!
//! A scrub produces two distinct kinds of output, and the contract keeps
//! them on separate callbacks because they have *different lifetimes and
//! different audiences*:
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
//!   byte offset, what block size, and a `verify` closure that decides
//!   "is this candidate the original data?".  No checksum bytes, no
//!   algorithm name, no log context.  The CLI's `on_event` hands the
//!   closure straight to [`crate::recovery::RecoveryInput::verifier`] and
//!   never inspects it.  This is the seam that has to be stable across
//!   filesystems; the log seam is allowed to be format-flavoured.
//!
//! Splitting them means the recovery seam stays tiny and algorithm-
//! agnostic forever, while logging — which is genuinely format-specific
//! (every filesystem has its own diagnostic vocabulary) — is owned by the
//! filesystem implementation that has context to format it well.  A
//! future ZFS implementation never has to ask "what hex width does the
//! recovery layer expect?" because the answer is "none, format your own
//! log string".
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
//! ## The verifier is owned by the event, not the caller
//!
//! Different filesystems use different checksum algorithms (btrfs crc32c,
//! 4 bytes little-endian; ZFS fletcher2/fletcher4/sha256/blake3/edonr,
//! 4–48 bytes).  Rather than pass the stored csum through a fixed-width
//! field and ask the caller to re-derive the verifier, the scrub builds
//! **the verifier itself** per event.  The btrfs implementation captures
//! `|b| crc32c::crc32c(b) == stored`; a ZFS implementation captures
//! `|b| sha256(b).as_slice() == stored`.  The recovery layer takes a
//! `&dyn Fn(&[u8]) -> bool` and never asks what algorithm produced it —
//! broad enough to cover any checksum, with zero width/type coupling at
//! the boundary.

/// Recovery-only payload emitted by a filesystem scrub per mismatched
/// sector.
///
/// Deliberately tiny: just `array_phys`, `block_size`, and a `verify`
/// closure.  No checksum bytes, no algorithm name, no log context —
/// those live on the [`ScrubCallbacks::on_log`] stream, which the
/// filesystem implementation owns end-to-end.  Recovery only needs to
/// know *where on disk* and *is this candidate right*; everything else
/// is either implementation detail or diagnostic noise that shouldn't
/// cross this seam.
///
/// `verify` carries `Send + Sync` so the event (and the closure it holds)
/// can be moved across threads if the caller ever wants a parallel
/// scrub — btrfs's `crc32c` and ZFS's sha256 closures are both trivially
/// thread-safe since they only read their captured bytes.
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
    pub verify: Option<Box<dyn Fn(&[u8]) -> bool + Send + Sync>>,
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
    pub bytes_checked: u64,
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
///    array config if `--recover` is set;
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
    fn run(&mut self, callbacks: &mut dyn ScrubCallbacks) -> std::io::Result<ScrubStats>;
}