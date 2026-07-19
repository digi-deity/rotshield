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
//! is the only writer to those bytes.  The freeze is held for the shortest
//! possible window: from just before reconfirmation through the recovery
//! write emitted by the `on_event` callback (which runs synchronously inside
//! the scrub loop), then released.  It is NOT held for the whole scrub run.
//!
//! ## Safety: thaw is guaranteed
//!
//! `FIFREEZE` is a filesystem *state*, not a process-scoped lock.  If scrub-rs
//! panics or is killed after freezing but before `FITHAW`, the filesystem
//! stays frozen and every writer (databases, journald, the shell) stalls.
//! That is strictly worse than the corruption we were fixing, so thawing is
//! non-negotiable.  Three layers guarantee it:
//!
//! 1. **RAII `FreezeGuard`** — thaws on `Drop`, covering normal return *and*
//!    unwinding panics.
//! 2. **Panic hook** — a best-effort thaw installed on first freeze, covering
//!    the common panic path even before abort (note: `panic=abort` builds
//!    skip both Drop and the hook, which is why layer 3 exists for the
//!    operator's peace of mind — see below).
//! 3. **Operator awareness** — because a truly stuck process (abort, SIGKILL,
//!    kernel panic) cannot be caught by userspace, the freeze window is kept
//!    as short as possible (one sector's reconfirm+write, milliseconds) and
//!    the mountpoint is explicit, so a human can `fsfreeze -u <mnt>` if ever
//!    needed.  We deliberately do NOT add a watchdog thread: the reconfirm+
//!    write is trusted to be fast, and a watchdog that force-thaws mid-write
//!    would defeat the freeze's purpose.
//!
//! ## No live filesystem?
//!
//! If the target is an offline/unmounted image (no live mount), the
//! controller is constructed with `mountpoint = None` and `guard()` returns
//! `None` — no ioctl is issued, the scrub proceeds exactly as before.  The
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
/// `Drop`.  `guard()` returns `None` when there is no live mount, so callers
/// can hold an `Option<FreezeGuard>` uniformly.
pub struct FreezeController {
    mountpoint: Option<PathBuf>,
    /// Whether a freeze is currently held (for the panic hook / diagnostics).
    frozen: bool,
}

impl FreezeController {
    /// Build a controller.  `mountpoint` is the directory the live
    /// filesystem is mounted at (e.g. `/mnt/disk1`); `None` means we are
    /// scrubbing an offline image and no freeze will ever be attempted.
    pub fn new(mountpoint: Option<PathBuf>) -> Self {
        Self {
            mountpoint,
            frozen: false,
        }
    }

    /// Whether a live mount was declared (i.e. freezing is possible at all).
    pub fn has_live_mount(&self) -> bool {
        self.mountpoint.is_some()
    }

    /// Acquire a scoped freeze.  Returns `Some(guard)` that thaws on drop
    /// when a live mount was declared; `None` (no-op) otherwise.  Freezing
    /// is idempotent at the kernel level but we only issue `FIFREEZE` once
    /// per guard to keep the thaw accounting exact.
    pub fn guard(&mut self) -> Option<FreezeGuard<'_>> {
        let mountpoint = self.mountpoint.as_ref()?;
        match freeze_path(mountpoint) {
            Ok(()) => {
                self.frozen = true;
                note_active_mount(mountpoint);
                install_panic_hook();
                Some(FreezeGuard { controller: self })
            }
            Err(e) => {
                // Freeze failed (e.g. not a mountpoint, EPERM, EBUSY).  We
                // must NOT proceed with a write that assumes a frozen FS, but
                // the scrub's mismatch detection is still valid — so we log
                // and return None, letting the caller skip the freeze rather
                // than abort the whole scrub.  The caller decides whether a
                // non-frozen write is acceptable (it prints a warning).
                eprintln!(
                    "warning: could not freeze {} for safe recovery write: {} \
                     (proceeding WITHOUT freeze — live writes may race the recovery write)",
                    mountpoint.display(),
                    e
                );
                None
            }
        }
    }

    /// Thaw the currently-held freeze, if any.  Safe to call when not frozen.
    fn thaw(&mut self) {
        if !self.frozen {
            return;
        }
        if let Some(mountpoint) = self.mountpoint.as_ref()
            && let Err(e) = thaw_path(mountpoint)
        {
            // A failed thaw is serious: the FS stays frozen.  Log loudly;
            // the operator can `fsfreeze -u <mnt>` manually.  We do not
            // panic here (we may already be unwinding).
            eprintln!(
                "ERROR: failed to thaw {} after recovery write: {} \
                     (filesystem may still be frozen — run `fsfreeze -u {}`)",
                mountpoint.display(),
                e,
                mountpoint.display(),
            );
        }
        clear_active_mount();
        self.frozen = false;
    }
}

impl Drop for FreezeController {
    fn drop(&mut self) {
        self.thaw();
    }
}

/// RAII guard: thaws the filesystem when dropped.  Held only for the
/// reconfirm+write window of a single mismatched sector.
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
