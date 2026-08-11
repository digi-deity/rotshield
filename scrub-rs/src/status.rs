//! Opt-in HTTP-over-Unix-socket status server: serves live scrub counters
//! over a root-only filesystem socket so the unRAID plugin can show progress
//! without polling process logs or consuming a TCP port.
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
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

    /// CSUM-tree branches skipped as stale mid-scrub — expected churn on a
    /// live filesystem; informational only (does not block exit 0).
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

/// Minimal HTTP-over-Unix-socket server: answers GET /status with the counter
/// snapshot and everything else with 404, to whoever can connect to the
/// socket (pinned to mode 0600, so root only — the page's status.php runs as
/// root via emhttp).
pub struct StatusServer {
    listener: UnixListener,
    counters: Arc<StatusCounters>,
}

impl StatusServer {
    /// Bind a Unix socket at `path`, creating parent directories as needed.
    ///
    /// Recovery policy for an already-existing path (EADDRINUSE):
    ///   - a regular file / directory / symlink is NOT ours to delete — the
    ///     bind fails and the caller decides;
    ///   - a socket file with no server behind it (crashed/killed run) is
    ///     unlinked and rebound;
    ///   - a socket a live server is actually answering on (probe connect
    ///     succeeds — a second scrub-rs instance) is left alone and the
    ///     bind fails.  The probe runs before every unlink, so a stale
    ///     file is the only thing ever removed.
    pub fn bind(path: impl AsRef<Path>, counters: Arc<StatusCounters>) -> std::io::Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        let listener = match UnixListener::bind(path) {
            Ok(l) => l,
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                // Never unlink a non-socket: the path may be a file or
                // directory some other tool left there.
                let meta = fs::symlink_metadata(path)?;
                if !meta.file_type().is_socket() {
                    return Err(e);
                }
                // A successful probe connect means another instance is
                // serving on this path right now — do not clobber it.
                if UnixStream::connect(path).is_ok() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::AddrInUse,
                        format!("status socket {path:?} is in use by a live server"),
                    ));
                }
                fs::remove_file(path)?;
                UnixListener::bind(path)?
            }
            Err(e) => return Err(e),
        };
        // Root-only: the socket file is visible on the filesystem to every
        // local user, so pin its mode to 0600.
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
        Ok(Self { listener, counters })
    }

    /// Start the server on a background thread named scrub-status.
    pub fn spawn(
        path: impl AsRef<Path>,
        counters: Arc<StatusCounters>,
    ) -> std::io::Result<thread::JoinHandle<()>> {
        let server = Self::bind(path, counters)?;
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

    /// Unique per-test socket path under the temp dir (parallel-safe).
    fn test_sock_path() -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("scrub-status-{}-{nanos}.sock", std::process::id()))
    }

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

        let path = test_sock_path();
        let server = StatusServer::bind(&path, counters.clone()).unwrap();
        let handle = thread::spawn(move || server.serve());

        let mut stream = UnixStream::connect(&path).expect("connect");
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

        let mut stream = UnixStream::connect(&path).expect("connect");
        stream
            .write_all(b"GET /nope HTTP/1.1\r\n\r\n")
            .expect("write");
        let mut resp = String::new();
        stream.read_to_string(&mut resp).expect("read");
        assert!(resp.starts_with("HTTP/1.1 404"), "got: {resp:?}");

        drop(handle);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn rebinds_over_stale_socket() {
        // A crashed run leaves its socket file behind; bind() must unlink it
        // and rebind instead of failing with EADDRINUSE.
        let counters = Arc::new(StatusCounters::new());
        let path = test_sock_path();
        {
            // Dropping the listener leaves the socket FILE in place with no
            // server behind it — exactly the stale state a crash produces.
            let server = StatusServer::bind(&path, counters.clone()).unwrap();
            drop(server);
        }
        assert!(path.exists(), "socket file should linger after drop");
        let server = StatusServer::bind(&path, counters.clone()).unwrap();
        drop(server);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn refuses_to_unlink_a_non_socket_file() {
        // A regular file squatting on the socket path is not ours to
        // delete: bind must fail and the file must survive untouched.
        let counters = Arc::new(StatusCounters::new());
        let path = test_sock_path();
        fs::write(&path, b"someone else's data").unwrap();

        let err = match StatusServer::bind(&path, counters.clone()) {
            Ok(_) => panic!("bind must fail over a non-socket file"),
            Err(e) => e,
        };
        assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);
        assert_eq!(fs::read(&path).unwrap(), b"someone else's data");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn refuses_to_clobber_a_live_socket() {
        // A second instance binding the same path must fail without
        // unlinking the first instance's live socket.
        let counters = Arc::new(StatusCounters::new());
        let path = test_sock_path();

        let server_a = StatusServer::bind(&path, counters.clone()).unwrap();
        let handle_a = thread::spawn(move || server_a.serve());

        let err = match StatusServer::bind(&path, counters.clone()) {
            Ok(_) => panic!("second bind must fail while the first is live"),
            Err(e) => e,
        };
        assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);

        // The original server still answers and the file is still there.
        let mut stream = UnixStream::connect(&path).expect("connect");
        stream
            .write_all(b"GET /status HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .expect("write");
        let mut resp = String::new();
        stream.read_to_string(&mut resp).expect("read");
        assert!(resp.starts_with("HTTP/1.1 200 OK"), "got: {resp:?}");
        assert!(path.exists(), "live socket must not be unlinked");

        drop(handle_a);
        let _ = fs::remove_file(&path);
    }
}
