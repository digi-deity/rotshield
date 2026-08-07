//! Filesystem freeze control for safe live recovery writes.
//!
//! When scrub-rs recovers a corrupted sector it writes directly to the raw
//! rdev (`/dev/loop2`), bypassing the kernel's view of the mounted
//! filesystem (`/mnt/disk1`).  If the live filesystem writes new data to
//! that same block *between* our reconfirmation (which reads the live
//! EXTENT/CSUM trees) and our `write_block`, we would overwrite the live
//! write with the recovered *old* version — silent data loss.
//!
//! Freezing the mounted filesystem (`FIFREEZE` ioctl) stops the kernel from
//! issuing any new writes for the duration of the reconfirm+write, so scrub-rs
//! is the only writer to those bytes.  The freeze is held for one **batch**
//! at a time (`batch_recover.rs::flush_batch` acquires a single guard that
//! covers every candidate's reconfirm + stripe gather + write + read-back
//! for the whole batch, up to `batch_max` sectors), not per-sector and not
//! for the whole scrub run.  This is a deliberate choice over freezing
//! per-sector: toggling `FIFREEZE`/`FITHAW` on and off dozens of times per
//! batch would flicker the live filesystem frozen/thawed repeatedly, which
//! is its own source of latency spikes for anything doing I/O against the
//! mount during recovery — a single bounded freeze per batch is preferred
//! over frequent on/off flapping. The window is still bounded (never longer
//! than one batch's worth of work) and released as soon as that batch's
//! `flush_batch` call returns.
//!
//! ## Safety: thaw is guaranteed
//!
//! `FIFREEZE` is a filesystem *state*, not a process-scoped lock.  If scrub-rs
//! panics or is killed after freezing but before `FITHAW`, the filesystem
//! stays frozen and every writer (databases, journald, the shell) stalls.
//! That is strictly worse than the corruption we were fixing, so thawing is
//! non-negotiable.  Five layers guarantee it:
//!
//! 1. **RAII `FreezeGuard`** — thaws on `Drop`, covering normal return *and*
//!    unwinding panics.  A failed `FITHAW` is *recorded* (see
//!    [`FreezeController::take_thaw_error`]) so the caller can abort the run
//!    instead of assuming the filesystem thawed.
//! 2. **Panic hook** — a best-effort thaw installed on first freeze, covering
//!    the common panic path even before abort (note: `panic=abort` builds
//!    skip both Drop and the hook, which is why layers 4/5 exist for the
//!    operator's peace of mind).
//! 3. **SIGTERM/SIGINT handler** — installed on first freeze (same once-only
//!    pattern as the panic hook): on a terminate signal it best-effort thaws
//!    any active freeze, then restores default disposition and re-raises the
//!    signal so the process dies with the semantics the caller asked for.
//! 4. **Shell backstop** — `scrub.sh stop()` runs an unconditional
//!    best-effort `fsfreeze -u` on the array's mounts *after* killing the
//!    process tree, covering SIGKILL (uncatchable by userspace) and a freeze
//!    that predates the handler.
//! 5. **Operator awareness** — because a truly stuck process (abort, SIGKILL,
//!    kernel panic) cannot be caught by userspace, the freeze window is kept
//!    as short as reasonably possible (bounded to one batch's worth of
//!    reconfirm+write, not the whole scrub run) and the mountpoint is
//!    explicit, so a human can `fsfreeze -u <mnt>` if ever needed.  We
//!    deliberately do NOT add a watchdog thread: the batch's reconfirm+
//!    write work is trusted to complete promptly, and a watchdog that
//!    force-thaws mid-write would defeat the freeze's purpose.
//!
//! ## No live filesystem?
//!
//! If the target is an offline/unmounted image (no live mount), the
//! controller is constructed with `mountpoint = None` and `guard()` returns
//! `Ok(None)` — no ioctl is issued, the scrub proceeds exactly as before.  The
//! mountpoint is always provided explicitly by the caller (never
//! auto-detected), so a mounted-but-undeclared FS is never frozen by
//! accident, and an unmounted one is never assumed live.

use std::io;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use nix::libc;

/// Controls freezing of a (possibly absent) live mounted filesystem during
/// recovery writes.
///
/// Construct with [`FreezeController::new`]; pass `None` for the mountpoint
/// when scrubbing an offline/unmounted image.  Acquire a scoped freeze with
/// [`FreezeController::guard`] — it freezes on construction and thaws on
/// `Drop`.  `guard()` returns `Ok(None)` when there is no live mount (so
/// callers can hold an `Option<FreezeGuard>` uniformly) and `Err` when a
/// live mount was declared but the freeze ioctl failed — the caller must
/// not write in that case.  A `FITHAW` failure is surfaced to the caller
/// via [`FreezeController::take_thaw_error`] (the guard's `Drop` cannot
/// return an error).
pub struct FreezeController {
    mountpoint: Option<PathBuf>,
    /// Whether a freeze is currently held (for the panic hook / diagnostics).
    frozen: bool,
    /// The most recent `FITHAW` failure, if any.  Set when a thaw ioctl
    /// fails (the filesystem may then STILL be frozen); read-and-cleared by
    /// [`FreezeController::take_thaw_error`].  The RAII guard's `Drop` cannot
    /// return an error, so this is how the batch writer learns that a thaw
    /// failed and must abort the run instead of starting further batches on
    /// a false "thawed" assumption.
    last_thaw_error: Option<io::Error>,
}

impl FreezeController {
    /// Build a controller.  `mountpoint` is the directory the live
    /// filesystem is mounted at (e.g. `/mnt/disk1`); `None` means we are
    /// scrubbing an offline image and no freeze will ever be attempted.
    pub fn new(mountpoint: Option<PathBuf>) -> Self {
        Self {
            mountpoint,
            frozen: false,
            last_thaw_error: None,
        }
    }

    /// The declared live mountpoint, if any — for operator-facing
    /// messages (e.g. the manual `fsfreeze -u <mountpoint>` recovery
    /// command printed when a thaw fails).
    pub fn mountpoint(&self) -> Option<&Path> {
        self.mountpoint.as_deref()
    }

    /// Whether a live mount was declared (i.e. freezing is possible at all).
    pub fn has_live_mount(&self) -> bool {
        self.mountpoint.is_some()
    }

    /// Acquire a scoped freeze.  Three outcomes, distinguished so the
    /// caller can never confuse "no freeze needed" with "freeze failed":
    ///
    /// * `Ok(Some(guard))` — a live mount was declared and `FIFREEZE`
    ///   succeeded; the guard thaws on drop.  Writes are safe.
    /// * `Ok(None)` — **no freeze is required**: no live mount was declared
    ///   (offline/unmounted image, dry-run, or the operator passed
    ///   `--no-freeze`).  Writes proceed exactly as before; there is
    ///   nothing to freeze.
    /// * `Err(e)` — a live mount **was** declared but `FIFREEZE` failed
    ///   (EBUSY, EPERM, wrong mountpoint, filesystem quirks).  The caller
    ///   MUST NOT proceed with a write that assumes a frozen filesystem:
    ///   it should run that batch assess-only (classify, never write).
    ///   Freezing is idempotent at the kernel level but we only issue
    ///   `FIFREEZE` once per guard to keep the thaw accounting exact.
    pub fn guard(&mut self) -> Result<Option<FreezeGuard<'_>>, io::Error> {
        let Some(mountpoint) = self.mountpoint.as_ref() else {
            // No live mount declared — nothing to freeze, no freeze needed.
            return Ok(None);
        };
        match freeze_path(mountpoint) {
            Ok(()) => {
                self.frozen = true;
                note_active_mount(mountpoint);
                install_panic_hook();
                install_signal_handlers();
                Ok(Some(FreezeGuard { controller: self }))
            }
            Err(e) => {
                // Freeze REQUIRED but failed.  Return the error: the caller
                // decides whether a non-frozen write is acceptable (it is
                // NOT — the batch must be run assess-only).
                Err(e)
            }
        }
    }

    /// Thaw the currently-held freeze, if any.  Safe to call when not frozen.
    ///
    /// On a failed `FITHAW` the failure is *recorded* (see
    /// [`FreezeController::take_thaw_error`]) and the controller stays
    /// `frozen`, so `Drop`/a later explicit thaw retries and the panic
    /// hook / signal handler stay armed (the active-freeze marker is NOT
    /// cleared on failure — the filesystem may still be frozen).  A retry
    /// is harmless: `FITHAW` on an already-thawed filesystem just fails.
    fn thaw(&mut self) {
        if !self.frozen {
            return;
        }
        if let Some(mountpoint) = self.mountpoint.as_ref() {
            match thaw_path(mountpoint) {
                Ok(()) => {
                    clear_active_mount();
                    self.frozen = false;
                }
                Err(e) => {
                    // A failed thaw is serious: the FS stays frozen.  Log
                    // loudly; the operator can `fsfreeze -u <mnt>` manually.
                    // We do not panic here (we may already be unwinding).
                    eprintln!(
                        "ERROR: failed to thaw {} after recovery write: {} \
                             (filesystem may still be frozen — run `fsfreeze -u {}`)",
                        mountpoint.display(),
                        e,
                        mountpoint.display(),
                    );
                    self.last_thaw_error = Some(e);
                }
            }
        } else {
            // Defensive: `frozen` is only ever set together with a declared
            // mountpoint, but if that invariant is ever violated, never
            // leave the controller stuck in a frozen-looking state.
            clear_active_mount();
            self.frozen = false;
        }
    }

    /// Return (and clear) the recorded `FITHAW` failure from the most recent
    /// thaw, if any.
    ///
    /// Contract: the RAII guard's `Drop` cannot surface errors, so the batch
    /// writer calls this AFTER the guard drops at the end of a batch.  A
    /// `Some(_)` answer means the thaw ioctl failed and the filesystem may
    /// still be frozen — the caller MUST abort the repair run (do not start
    /// further batches) and surface the manual recovery command
    /// (`fsfreeze -u <mountpoint>`) in the exit path and notification.
    pub fn take_thaw_error(&mut self) -> Option<io::Error> {
        self.last_thaw_error.take()
    }
}

impl Drop for FreezeController {
    fn drop(&mut self) {
        self.thaw();
    }
}

/// RAII guard: thaws the filesystem when dropped.  Held for the
/// reconfirm+write window of one batch (`batch_recover.rs::flush_batch`
/// acquires exactly one guard per batch, not per-sector — see the module
/// doc for why).
pub struct FreezeGuard<'a> {
    controller: &'a mut FreezeController,
}

impl Drop for FreezeGuard<'_> {
    fn drop(&mut self) {
        self.controller.thaw();
    }
}

/// Best-effort panic hook that thaws any active freeze.  Installed once (on
/// first successful freeze) so a panic during reconfirm/write does not leave
/// the filesystem frozen.  Covers the common unwind path; `panic=abort`
/// builds skip this, which is why the freeze window is kept microscopic.
fn install_panic_hook() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static INSTALLED: AtomicBool = AtomicBool::new(false);
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Thaw every mountpoint we might have frozen.  We can't reach the
        // specific controller from here, but the panic hook is a last resort
        // and a best-effort global thaw of the declared mount is better than
        // nothing.  The controller's own Drop still runs for unwinding panics.
        if let Some(mnt) = ACTIVE_FREEZE_MOUNT.lock().ok().and_then(|g| g.clone()) {
            let _ = thaw_path(Path::new(&mnt));
        }
        prev(info);
    }));
}

/// Best-effort SIGTERM/SIGINT handler: thaws any active freeze, then
/// restores the default disposition and re-raises the signal so the process
/// dies with the signal semantics the caller asked for (e.g. `scrub.sh`
/// `stop()`'s TERM-then-KILL flow observes the death it requested).
///
/// Async-signal-safety: `thaw_path` (open + `ioctl`) and the `Mutex` guard
/// are NOT strictly async-signal-safe — exactly the same tradeoff the panic
/// hook above already makes.  The alternative (dying with the filesystem
/// frozen) is strictly worse, and a torn thaw here is backed up by layer 4
/// (scrub.sh's post-kill `fsfreeze -u`), so the risk is acceptable and
/// documented.  `sigaction`/`raise` in the tail of the handler ARE
/// async-signal-safe.
///
/// Installed once per process, on the first successful freeze (see
/// [`FreezeController::guard`]), so plain scrubs without a live mount are
/// unaffected.
extern "C" fn signal_thaw_handler(sig: libc::c_int) {
    // Best-effort thaw of any active freeze before dying.  Only the most
    // recent freeze is tracked (same as the panic hook); the RAII guard
    // remains the primary mechanism.
    if let Some(mnt) = ACTIVE_FREEZE_MOUNT.lock().ok().and_then(|g| g.clone())
        && thaw_path(Path::new(&mnt)).is_ok()
    {
        clear_active_mount();
    }
    // Restore default disposition, then re-raise so the process dies for
    // real (without this, the handler would swallow the signal and the
    // process would keep running — and a second signal could recurse into
    // this handler).
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = libc::SIG_DFL;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(sig, &action, std::ptr::null_mut());
        libc::raise(sig);
    }
}

/// Install [`signal_thaw_handler`] for SIGTERM/SIGINT.  Idempotent (once
/// per process, like [`install_panic_hook`]); called from
/// [`FreezeController::guard`] on the first successful freeze so a plain
/// scrub without a live mount never touches signal dispositions.
///
/// Installation is best-effort: if `sigaction` fails (essentially never),
/// we log once and continue — layers 1/2/4/5 still cover the thaw.
fn install_signal_handlers() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static INSTALLED: AtomicBool = AtomicBool::new(false);
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};
    let action = SigAction::new(
        SigHandler::Handler(signal_thaw_handler),
        SaFlags::empty(),
        SigSet::empty(),
    );
    for sig in [Signal::SIGTERM, Signal::SIGINT] {
        if let Err(e) = unsafe { sigaction(sig, &action) } {
            eprintln!(
                "WARNING: could not install {sig:?} thaw handler: {e} — a kill mid-freeze \
                 may leave the filesystem frozen (scrub.sh stop() still attempts fsfreeze -u)"
            );
        }
    }
}

/// Tracks the mountpoint of the currently-held freeze so the panic hook can
/// thaw it as a last resort.  Only the most recent freeze is recorded; the
/// RAII guard remains the primary mechanism.
static ACTIVE_FREEZE_MOUNT: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

/// Record the active freeze mountpoint (called by [`FreezeController::guard`]
/// on success) so the panic hook has something to thaw.
pub(crate) fn note_active_mount(mnt: &Path) {
    if let Ok(mut g) = ACTIVE_FREEZE_MOUNT.lock() {
        *g = Some(mnt.to_path_buf());
    }
}

/// Clear the active freeze mountpoint (called by [`FreezeController::thaw`]).
pub(crate) fn clear_active_mount() {
    if let Ok(mut g) = ACTIVE_FREEZE_MOUNT.lock() {
        *g = None;
    }
}

/// Convenience: build a controller from an optional mount-point string.
pub fn controller_for(mountpoint: Option<&str>) -> FreezeController {
    FreezeController::new(mountpoint.map(PathBuf::from))
}

/// Expose the freeze result type for callers that want to surface errors.
pub type FreezeResult = io::Result<()>;

/// `FIFREEZE` / `FITHAW` ioctl request codes (Linux).  These are not exposed
/// by the `libc` crate we depend on, so we define them from the standard
/// `_IOWR('X', n, int)` macro expansion (note: `_IOWR`, read|write direction):
/// `_IOC_READ|WRITE (0xC000_0000) | (size_of::<c_int>() << 16) | ('X' << 8) | nr`.
/// 'X' = 88 = 0x58, int size = 4 = 0x04.  So FIFREEZE = 0xC004_5877,
/// FITHAW = 0xC004_5878.  (Using `_IOR`/bare `_IO` forms yields ENOTTY — the
/// kernel rejects the wrong direction/size bits.)
const FIFREEZE: std::os::raw::c_ulong = 0xC004_5877;
const FITHAW: std::os::raw::c_ulong = 0xC004_5878;

/// Freeze the filesystem mounted at `mountpoint` via the `FIFREEZE` ioctl.
/// Opens the mountpoint directory and issues the ioctl on its fd; the
/// kernel blocks all new writes until a matching `FITHAW`.
fn freeze_path(mountpoint: &Path) -> io::Result<()> {
    let file = std::fs::OpenOptions::new().read(true).open(mountpoint)?;
    // `ioctl` takes a `c_int` argument; for FIFREEZE the value is ignored
    // (traditionally 0 or 1).  We pass 0.
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), FIFREEZE, 0) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Thaw a previously-frozen filesystem at `mountpoint` via `FITHAW`.
fn thaw_path(mountpoint: &Path) -> io::Result<()> {
    let file = std::fs::OpenOptions::new().read(true).open(mountpoint)?;
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), FITHAW, 0) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Thin wrapper over `libc::ioctl` so other modules (e.g. the batched
/// recovery writer, for `BLKFLSBUF` cache invalidation) can issue raw
/// ioctls without re-importing `nix::libc`.  `request` is the ioctl
/// number; `arg` is passed through unchanged.
pub(crate) unsafe fn libc_ioctl(
    fd: std::os::raw::c_int,
    request: std::os::raw::c_ulong,
    arg: std::os::raw::c_ulong,
) -> std::os::raw::c_int {
    unsafe {
        libc::ioctl(
            fd,
            request as std::os::raw::c_ulong,
            arg as std::os::raw::c_ulong,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes tests that touch the shared `ACTIVE_FREEZE_MOUNT` global:
    /// `failed_thaw_is_recorded_and_keeps_frozen_state` asserts the marker
    /// survives a failed thaw while `no_thaw_error_by_default...` clears it
    /// at its start — without the lock the two race (intermittent CI
    /// failure).  A plain `Mutex` is fine: correctness needs only
    /// serialization, not fairness.
    static TEST_MOUNT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// A thaw whose ioctl fails (nonexistent mountpoint) must be recorded
    /// as an error AND must not clear the frozen state: the filesystem may
    /// still be frozen, so `Drop`/a retry must be able to try again, the
    /// panic/signal handlers must stay armed, and the batch writer must be
    /// able to observe the failure via `take_thaw_error`.
    #[test]
    fn failed_thaw_is_recorded_and_keeps_frozen_state() {
        let _guard = TEST_MOUNT_LOCK.lock().unwrap();
        clear_active_mount(); // avoid cross-test contamination of the global
        // Simulate a real freeze: guard() sets both `frozen` and the
        // active-freeze marker before returning the guard.
        note_active_mount(Path::new("/nonexistent/freeze-test-mnt"));
        let mut c = FreezeController {
            mountpoint: Some(PathBuf::from("/nonexistent/freeze-test-mnt")),
            frozen: true,
            last_thaw_error: None,
        };
        c.thaw();
        assert!(
            c.frozen,
            "failed thaw must keep frozen so Drop/retry stays possible"
        );
        assert!(
            c.take_thaw_error().is_some(),
            "failed thaw must be observable via take_thaw_error"
        );
        assert!(
            c.take_thaw_error().is_none(),
            "take_thaw_error must clear the recorded error"
        );
        // The active-freeze marker must also survive a failed thaw: the
        // signal handler / panic hook rely on it as the last-resort thaw.
        assert!(
            ACTIVE_FREEZE_MOUNT.lock().unwrap().is_some(),
            "active-freeze marker must stay set after a failed thaw"
        );
    }

    /// A fresh controller has no recorded thaw error, and thawing when not
    /// frozen is a no-op that records nothing.
    #[test]
    fn no_thaw_error_by_default_and_thaw_noop_when_not_frozen() {
        let _guard = TEST_MOUNT_LOCK.lock().unwrap();
        clear_active_mount();
        let mut c = FreezeController::new(Some(PathBuf::from("/nonexistent/freeze-test-mnt")));
        assert!(c.take_thaw_error().is_none());
        c.thaw(); // not frozen -> no-op, must not attempt the ioctl
        assert!(!c.frozen);
        assert!(c.take_thaw_error().is_none());
    }

    /// Installing the terminate handlers is idempotent and must not disturb
    /// normal operation (no signal is sent by installation; the handler is
    /// inert until a SIGTERM/SIGINT arrives, at which point it thaws any
    /// active freeze and re-raises with default disposition — i.e. the
    /// process still dies, which is the behaviour an uninstalled default
    /// would have given).
    #[test]
    fn signal_handlers_install_once_without_breaking_normal_operation() {
        install_signal_handlers();
        install_signal_handlers(); // second call must be a no-op (no panic)
    }

    /// A controller without a live mount never records thaw errors and its
    /// guard is `Ok(None)` — the offline-image path is untouched.
    #[test]
    fn no_mount_controller_never_frozen() {
        let mut c = FreezeController::new(None);
        assert!(!c.has_live_mount());
        assert!(c.guard().unwrap().is_none());
        assert!(c.take_thaw_error().is_none());
    }
}
