//! Opt-in HTTP status server: serves live scrub counters over localhost so
//! the unRAID plugin can show progress without polling process logs.
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Live scrub/recovery counters shared with the status server. Mirrors
/// ScrubStats but updated concurrently from multiple threads.
#[derive(Default)]
pub struct StatusCounters {
    pub sectors_checked: AtomicU64,
    pub sectors_ok: AtomicU64,
    pub sectors_no_csum: AtomicU64,
    pub sectors_read_error: AtomicU64,

    /// CSUM-tree branches skipped as stale mid-scrub — a coverage gap that
    /// blocks exit 0.
    pub stale_csum_branches: AtomicU64,

    /// Read runs truncated when the EIO isolation budget ran out.
    pub isolation_truncated: AtomicU64,
    pub bytes_checked: AtomicU64,

    /// Confirmed mismatch tally (snapshot: sectors_mismatch).
    pub mismatch: AtomicU64,

    /// Benign stale-churn tally (snapshot: sectors_stale).
    pub stale: AtomicU64,

    /// Metadata failures: all-mirror header errors, mirror divergences,
    /// read EIOs.
    pub metadata_header_errors: AtomicU64,
    pub metadata_mirror_mismatches: AtomicU64,
    pub metadata_read_errors: AtomicU64,

    /// Recovery verdicts from the batch writer.
    pub recovered: AtomicU64,
    pub failed: AtomicU64,
    pub skipped: AtomicU64,

    /// Candidates recovered but not written because the batch could not freeze.
    pub not_frozen: AtomicU64,

    /// Recovered blocks whose read-back verification failed.
    pub readback_failed: AtomicU64,

    /// Candidates already matching their checksum by write time.
    pub not_corrupt: AtomicU64,

    /// Duplicate candidates dropped within a batch.
    pub deduped: AtomicU64,

    /// Set once a recovered block was written while the filesystem was live.
    pub repaired_while_mounted: AtomicU64,

    /// 1 once recovery mode is active (set at pipeline spawn; never cleared).
    pub recovery: AtomicU64,

    /// Scrub progress in bytes.
    pub progress_total: AtomicU64,
    pub progress_done: AtomicU64,

    // State and device strings for the status page.
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

    pub fn set_recovery(&self, on: bool) {
        self.recovery.store(u64::from(on), Ordering::Relaxed);
    }

    /// Render all counters as key=value text for the status page.
    pub fn snapshot(&self) -> String {
        let l = |a: &AtomicU64| a.load(Ordering::Relaxed);
        let total = l(&self.progress_total);
        let done = l(&self.progress_done);

        // done/total as a percentage, clamped to 100; 100 while total is 0
        // (nothing queued yet).
        let pct = if total == 0 {
            100.0
        } else {
            let milli = u128::from(done).saturating_mul(100_000) / u128::from(total);
            (milli.min(100_000) as f64) / 1000.0
        };
        format!(
            "state={}\ndevice={}\n\
             sectors_checked={}\nsectors_ok={}\nsectors_mismatch={}\n\
             sectors_stale={}\nsectors_no_csum={}\nsectors_read_error={}\n\
             stale_csum_branches={}\nisolation_truncated={}\n\
             bytes_checked={}\nmetadata_header_errors={}\n\
             metadata_mirror_mismatches={}\nmetadata_read_errors={}\n\
             recovered={}\nfailed={}\nskipped={}\n\
             not_frozen={}\nreadback_failed={}\nnot_corrupt={}\ndeduped={}\n\
             repaired_while_mounted={}\n\
             recovery={}\n\
             progress_total={}\nprogress_done={}\nprogress_pct={:.2}\n",
            self.state.lock().unwrap(),
            self.device.lock().unwrap(),
            l(&self.sectors_checked),
            l(&self.sectors_ok),
            l(&self.mismatch),
            l(&self.stale),
            l(&self.sectors_no_csum),
            l(&self.sectors_read_error),
            l(&self.stale_csum_branches),
            l(&self.isolation_truncated),
            l(&self.bytes_checked),
            l(&self.metadata_header_errors),
            l(&self.metadata_mirror_mismatches),
            l(&self.metadata_read_errors),
            l(&self.recovered),
            l(&self.failed),
            l(&self.skipped),
            l(&self.not_frozen),
            l(&self.readback_failed),
            l(&self.not_corrupt),
            l(&self.deduped),
            l(&self.repaired_while_mounted),
            l(&self.recovery),
            total,
            done,
            pct,
        )
    }
}

/// Minimal HTTP server on 127.0.0.1 serving GET /status as text/plain.
pub struct StatusServer {
    listener: TcpListener,
    counters: Arc<StatusCounters>,
}

impl StatusServer {
    /// Bind to an ephemeral localhost port (used by tests) or a fixed one.
    pub fn bind(port: u16, counters: Arc<StatusCounters>) -> std::io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", port))?;
        Ok(Self { listener, counters })
    }

    /// Start the server on a background thread named scrub-status.
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

    /// Serve until the listener fails: one connection per request, answering
    /// GET /status with the counter snapshot and everything else with 404.
    pub fn serve(&self) {
        for stream in self.listener.incoming() {
            let Ok(mut stream) = stream else { continue };

            // Bounded wait so a stalled client cannot pin the connection.
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let mut buf = [0u8; 512];
            let _ = stream.read(&mut buf);

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
        counters.recovery.store(1, Ordering::Relaxed);
        counters.progress_total.store(1073741824, Ordering::Relaxed);
        counters.progress_done.store(268435456, Ordering::Relaxed);

        let server = StatusServer::bind(0, counters.clone()).unwrap();
        let port = server.listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || server.serve());

        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
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
            "recovery=1",
            "progress_total=1073741824",
            "progress_done=268435456",
            "progress_pct=25.00",
        ] {
            assert!(resp.contains(expected), "missing {expected:?} in: {resp:?}");
        }

        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
        stream
            .write_all(b"GET /nope HTTP/1.1\r\n\r\n")
            .expect("write");
        let mut resp = String::new();
        stream.read_to_string(&mut resp).expect("read");
        assert!(resp.starts_with("HTTP/1.1 404"), "got: {resp:?}");

        drop(handle);
    }
}
