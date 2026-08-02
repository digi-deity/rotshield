//! Minimal HTTP status server + shared live counters for the plugin.
//!
//! A scrub-rs run is a single-pass process, so the only way the plugin's
//! Settings page can show *live* counts (instead of just "running / idle")
//! is for the running process itself to expose them.  This module does that
//! with zero external dependencies:
//!
//! * [`StatusCounters`] — an `Arc`-shared bank of atomic counters that the
//!   scrub loop and the recovery writer mirror into at their existing
//!   increment sites.  Every field maps 1:1 to a number the run already
//!   accumulates (see `ScrubStats` / `BatchStats`); nothing here computes a
//!   value the run doesn't already track.
//! * [`StatusServer`] — a hand-rolled `TcpListener`-based HTTP server on
//!   `127.0.0.1:<port>` that answers `GET /status` with a shell-parsable
//!   `key=value` text body and 404s everything else.
//!
//! The server runs on its **own thread** so a slow/blocking HTTP client can
//! never stall the scrub.  It is opt-in: scrub-rs only starts it when
//! `--status-port <n>` is passed (the plugin does, via its config); a busy
//! port is logged and skipped, never fatal.
//!
//! ## Counter semantics (mode-aware)
//!
//! Exactly one accounting mode is active at a time, and both write to the
//! *same* `mismatch` / `stale` counters — so the numbers always match the
//! run's end-of-run summary:
//!
//! * **Batched (array present):** the scrub emits raw candidates and the
//!   recovery writer owns mismatch/stale/recovered/failed/skipped
//!   accounting (`flush_batch` mirrors those).
//! * **Inline (plain scrub):** the scrub loop re-confirms and owns
//!   mismatch/stale itself (`process_buf` mirrors those); the writer never
//!   runs, so `recovered`/`failed`/`skipped` stay 0.
//!
//! `sectors_checked`, `sectors_ok`, `sectors_no_csum`, `bytes_checked`,
//! `sectors_read_error` and the `metadata_*` counters are always driven by
//! the scrub loop.
//!
//! Payload example:
//!
//! ```text
//! state=running
//! device=/dev/nmd1p1
//! sectors_checked=123456
//! sectors_ok=123455
//! sectors_mismatch=1
//! sectors_stale=0
//! sectors_no_csum=0
//! sectors_read_error=0
//! bytes_checked=505937920
//! metadata_header_errors=0
//! metadata_mirror_mismatches=0
//! metadata_read_errors=0
//! recovered=1
//! failed=0
//! skipped=0
//! progress_total=1073741824
//! progress_done=268435456
//! progress_pct=25.00
//! ```
//!
//! Consumption from the plugin is a one-liner:
//!
//! ```sh
//! curl -s http://127.0.0.1:9101/status | awk -F= '$1=="recovered"{print $2}'
//! ```

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Shared live counters mirrored from the scrub loop + recovery writer.
///
/// Each field corresponds to a number the run already tracks (the scrub's
/// `ScrubStats` and the writer's `BatchStats`); the status server just
/// reads whatever has been accumulated so far.  `recovered` / `failed` /
/// `skipped` stay 0 when no recovery pipeline is running (no array config,
/// or the device isn't a data disk).
#[derive(Default)]
pub struct StatusCounters {
    /// Scrub-loop totals (always driven by the scrub).
    pub sectors_checked: AtomicU64,
    pub sectors_ok: AtomicU64,
    pub sectors_no_csum: AtomicU64,
    pub sectors_read_error: AtomicU64,
    pub bytes_checked: AtomicU64,

    /// Genuine corruption found.  Mirrored by the scrub loop in inline
    /// (array-less) mode and by the recovery writer in batched mode — the
    /// same number the end-of-run "sectors mismatch" summary prints.
    pub mismatch: AtomicU64,
    /// Benign churn (freed/rewritten/nodatasum).  Same dual-source rule as
    /// [`StatusCounters::mismatch`]; matches the "sectors stale" summary.
    pub stale: AtomicU64,

    /// Metadata errors (folded up by the filesystem driver at the end of
    /// open / the run, from its own + the csum walker's counters).
    pub metadata_header_errors: AtomicU64,
    pub metadata_mirror_mismatches: AtomicU64,
    pub metadata_read_errors: AtomicU64,

    /// Recovery-writer outcomes (from `BatchStats`).
    pub recovered: AtomicU64,
    pub failed: AtomicU64,
    pub skipped: AtomicU64,

    /// Coarse progress (dev-tree position).  `progress_total` is the
    /// denominator: the sum of physical lengths of the DATA dev-extents
    /// the scrub will actually scrub, set once by the scrub driver at
    /// `set_status` time from the eagerly-walked DEV_TREE (no scan).
    /// `progress_done` is the numerator: physical bytes of those extents
    /// fully scrubbed so far, bumped by the scrub loop as each data
    /// extent completes.  `progress_pct` (computed in `snapshot`) is
    /// `done / total`, monotonic non-decreasing by construction.
    pub progress_total: AtomicU64,
    pub progress_done: AtomicU64,

    /// Run state / device, set by `main` ("starting", "running", "done",
    /// "error").  Plain strings, read under a mutex (only set a handful of
    /// times — the mutex is never on the per-sector hot path).
    state: Mutex<String>,
    device: Mutex<String>,
}

impl StatusCounters {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_state(&self, state: impl Into<String>) {
        *self.state.lock().unwrap() = state.into();
    }

    pub fn set_device(&self, device: impl Into<String>) {
        *self.device.lock().unwrap() = device.into();
    }

    /// Serialise the current counters as shell-parsable `key=value` lines,
    /// one per line, in a stable order (one line per already-tracked
    /// counter — nothing extra is computed here).
    pub fn snapshot(&self) -> String {
        let l = |a: &AtomicU64| a.load(Ordering::Relaxed);
        let total = l(&self.progress_total);
        let done = l(&self.progress_done);
        // Coarse progress percentage: done / total, emitted as a float with
        // two decimal places so a partially-completed extent still shows
        // fractional movement.  Computed with integer fixed-point math
        // (milli-percent, 3 decimals of resolution) rather than raw f64
        // division: u128 guards against `done * 100_000` overflowing u64
        // and avoids any f64 rounding surprise on huge totals — exact for
        // any realistically-sized disk.  Monotonic non-decreasing by
        // construction (the numerator only grows; the denominator is fixed
        // after `set_status`), so the bar can never tick backwards.
        // total == 0 (no DATA extents — e.g. an empty fs) reports 100%:
        // all 0 scrubbable bytes are done.
        let pct = if total == 0 {
            100.0
        } else {
            let milli =
                u128::from(done).saturating_mul(100_000) / u128::from(total);
            (milli.min(100_000) as f64) / 1000.0
        };
        format!(
            "state={}\ndevice={}\n\
             sectors_checked={}\nsectors_ok={}\nsectors_mismatch={}\n\
             sectors_stale={}\nsectors_no_csum={}\nsectors_read_error={}\n\
             bytes_checked={}\nmetadata_header_errors={}\n\
             metadata_mirror_mismatches={}\nmetadata_read_errors={}\n\
             recovered={}\nfailed={}\nskipped={}\n\
             progress_total={}\nprogress_done={}\nprogress_pct={:.2}\n",
            self.state.lock().unwrap(),
            self.device.lock().unwrap(),
            l(&self.sectors_checked),
            l(&self.sectors_ok),
            l(&self.mismatch),
            l(&self.stale),
            l(&self.sectors_no_csum),
            l(&self.sectors_read_error),
            l(&self.bytes_checked),
            l(&self.metadata_header_errors),
            l(&self.metadata_mirror_mismatches),
            l(&self.metadata_read_errors),
            l(&self.recovered),
            l(&self.failed),
            l(&self.skipped),
            total,
            done,
            pct,
        )
    }
}

/// A minimal HTTP/1.1 server on `127.0.0.1:<port>` that answers `GET
/// /status` with the [`StatusCounters::snapshot`] payload and 404s every
/// other request.
pub struct StatusServer {
    listener: TcpListener,
    counters: Arc<StatusCounters>,
}

impl StatusServer {
    /// Bind the listener (fails with `AddrInUse` if the port is taken —
    /// the caller treats that as "no server", not an error).
    pub fn bind(port: u16, counters: Arc<StatusCounters>) -> std::io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", port))?;
        Ok(Self { listener, counters })
    }

    /// Bind on the given port and run the serve loop on a dedicated
    /// "scrub-status" thread, returning its join handle.  The caller drops
    /// the handle to detach (the thread dies with the process).
    pub fn spawn(
        port: u16,
        counters: Arc<StatusCounters>,
    ) -> std::io::Result<thread::JoinHandle<()>> {
        let server = Self::bind(port, counters)?;
        thread::Builder::new()
            .name("scrub-status".into())
            .spawn(move || server.serve())
            .map_err(std::io::Error::other)
    }

    /// Serve requests on the current thread until the listener fails.
    /// Handles one connection at a time; each is short-lived.  Errors on
    /// individual connections are skipped, never fatal — a misbehaving
    /// client cannot take down the process or stall the scrub (which runs
    /// on its own thread and never touches this listener).
    pub fn serve(&self) {
        for stream in self.listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            // Don't let a client that connects but never sends hold the
            // server thread forever.
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let mut buf = [0u8; 512];
            let _ = stream.read(&mut buf); // discard the request body

            let (code, reason, body) = if buf.starts_with(b"GET /status") {
                (200, "OK", self.counters.snapshot())
            } else {
                (404, "Not Found", "not found\n".to_string())
            };
            let response = format!(
                "HTTP/1.1 {code} {reason}\r\nContent-Length: {}\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n{body}",
                body.len(),
            );
            let _ = stream.write_all(response.as_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bind on an ephemeral port, run the serve loop on a thread, and
    /// verify `GET /status` returns the live counter payload as `key=value`
    /// text and everything else 404s.  This is the deterministic contract
    /// the plugin's polling depends on.
    #[test]
    fn serves_status_payload_and_404s() {
        let counters = Arc::new(StatusCounters::new());
        counters.set_state("running");
        counters.set_device("/dev/nmd1p1");
        counters.sectors_checked.store(42, Ordering::Relaxed);
        counters.sectors_ok.store(40, Ordering::Relaxed);
        counters.mismatch.store(2, Ordering::Relaxed);
        counters.stale.store(1, Ordering::Relaxed);
        counters.bytes_checked.store(4096, Ordering::Relaxed);
        counters.recovered.store(1, Ordering::Relaxed);
        counters
            .progress_total
            .store(1073741824, Ordering::Relaxed);
        counters
            .progress_done
            .store(268435456, Ordering::Relaxed);

        // Bind with port 0 -> the OS assigns an ephemeral port; query it so
        // the test connects to the right socket.
        let server = StatusServer::bind(0, counters.clone()).unwrap();
        let port = server.listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || server.serve());

        // 1. GET /status -> 200 with the payload.
        let mut stream =
            std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
        stream
            .write_all(b"GET /status HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .expect("write");
        let mut resp = String::new();
        stream.read_to_string(&mut resp).expect("read");
        assert!(resp.starts_with("HTTP/1.1 200 OK"), "got: {resp:?}");
        assert!(resp.contains("Content-Type: text/plain"), "got: {resp:?}");
        assert!(resp.contains("Content-Length:"), "got: {resp:?}");
        for expected in [
            "state=running",
            "device=/dev/nmd1p1",
            "sectors_checked=42",
            "sectors_ok=40",
            "sectors_mismatch=2",
            "sectors_stale=1",
            "sectors_no_csum=0",
            "sectors_read_error=0",
            "bytes_checked=4096",
            "metadata_header_errors=0",
            "metadata_mirror_mismatches=0",
            "metadata_read_errors=0",
            "recovered=1",
            "failed=0",
            "skipped=0",
            "progress_total=1073741824",
            "progress_done=268435456",
            "progress_pct=25.00",
        ] {
            assert!(resp.contains(expected), "missing {expected:?} in: {resp:?}");
        }

        // 2. Any other path -> 404.
        let mut stream =
            std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
        stream
            .write_all(b"GET /nope HTTP/1.1\r\n\r\n")
            .expect("write");
        let mut resp = String::new();
        stream.read_to_string(&mut resp).expect("read");
        assert!(resp.starts_with("HTTP/1.1 404"), "got: {resp:?}");

        drop(handle);
    }
}
