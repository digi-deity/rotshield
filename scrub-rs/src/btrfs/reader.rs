//! Node reads: logical/physical addressing, mirror handling, header checks,
//! and kernel read-ahead hints.

use std::fs::File;

use super::chunk::ChunkMap;
use super::csum_strategy::CsumStrategy;
use super::node::Node;
use super::util::pread_at;

/// Hint that the whole file will be read sequentially.
#[cfg(unix)]
fn advise_sequential(file: &File) {
    use std::os::fd::AsRawFd;

    let _ = nix::fcntl::posix_fadvise(
        file.as_raw_fd(),
        0,
        0,
        nix::fcntl::PosixFadviseAdvice::POSIX_FADV_SEQUENTIAL,
    );
}

/// Hint the kernel to read `len` bytes at `offset` into the page cache.
#[cfg(unix)]
pub(crate) fn advise_willneed(file: &File, offset: u64, len: u64) {
    use std::os::fd::AsRawFd;
    if len == 0 {
        return;
    }
    let _ = nix::fcntl::posix_fadvise(
        file.as_raw_fd(),
        offset as i64,
        len as i64,
        nix::fcntl::PosixFadviseAdvice::POSIX_FADV_WILLNEED,
    );
}

#[cfg(not(unix))]
fn advise_sequential(_file: &File) {}
#[cfg(not(unix))]
fn advise_willneed(_file: &File, _offset: u64, _len: u64) {}

/// Sentinel for `expected_generation`: skip the generation check.
pub const GEN_DONT_CHECK: u64 = u64::MAX;

/// Outcome of one node read across all its mirror copies.
pub struct ReadNodeResult {
    /// The node, when at least one copy had a valid header checksum.
    pub node: Option<Node>,
    pub all_mirrors_failed: bool,
    /// No copy passed full header validation, but at least one passed the
    /// header checksum (stale node).
    pub generation_mismatch: bool,

    /// Multiple mirror copies exist and only some passed the header checksum.
    pub mirror_mismatch: bool,
}

/// Reads btrfs metadata from one backing file (device or image), mapping
/// logical addresses through the chunk map.
pub struct FsReader {
    fp: File,
    node_size: usize,

    /// Offset of the filesystem partition within the backing store.
    base_offset: u64,

    /// Expected device id; physical reads of other devids are rejected.
    devid: Option<u64>,

    /// Checksum strategy for header verification; None skips verification.
    strategy: Option<CsumStrategy>,

    /// Expected fsid, checked against node headers when set.
    fsid: Option<[u8; 16]>,
}

impl FsReader {
    /// Open a reader over `fp`; hints sequential access to the kernel.
    pub fn new(
        fp: File,
        node_size: usize,
        base_offset: u64,
        strategy: Option<CsumStrategy>,
    ) -> Self {
        #[cfg(unix)]
        advise_sequential(&fp);
        Self {
            fp,
            node_size,
            base_offset,
            strategy,
            devid: None,
            fsid: None,
        }
    }

    /// Set the expected device id.
    pub fn with_devid(mut self, devid: u64) -> Self {
        self.devid = Some(devid);
        self
    }

    /// Set the expected filesystem id.
    pub fn with_fsid(mut self, fsid: [u8; 16]) -> Self {
        self.fsid = Some(fsid);
        self
    }

    /// Clone the backing fd so a second reader can share the file with its
    /// own seek position.
    pub fn reopen(&self) -> std::io::Result<File> {
        self.fp.try_clone()
    }

    pub fn base_offset(&self) -> u64 {
        self.base_offset
    }

    pub fn node_size(&self) -> usize {
        self.node_size
    }

    /// Read `n` bytes at a logical address, mapped through the chunk map.
    pub fn read_logical(
        &mut self,
        chunk_map: &ChunkMap,
        logical: u64,
        n: usize,
    ) -> std::io::Result<Vec<u8>> {
        let (_devid, phys) = chunk_map.lookup(logical).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("no chunk mapping for logical 0x{logical:x}"),
            )
        })?;

        pread_at(&self.fp, self.base_offset + phys, n)
    }

    /// Read `n` bytes at a physical offset, rejecting reads for other devices
    /// and hinting read-ahead past the current position.
    pub fn read_physical(&mut self, devid: u64, phys: u64, n: usize) -> std::io::Result<Vec<u8>> {
        if let Some(expected) = self.devid
            && devid != expected
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("read_physical devid {devid} does not match opened device {expected}"),
            ));
        }

        #[cfg(unix)]
        {
            const READAHEAD_CAP: u64 = 64 << 20;
            let readahead = (n as u64).min(READAHEAD_CAP);
            let start = self.base_offset + phys + n as u64;
            advise_willneed(&self.fp, start, readahead);
        }
        pread_at(&self.fp, self.base_offset + phys, n)
    }

    /// Hint the kernel to read ahead `len` bytes on every mirror copy of
    /// `logical`.
    pub fn prefetch_logical(&self, chunk_map: &ChunkMap, logical: u64, len: usize) {
        let Some(stripes) = chunk_map.lookup_stripes(logical) else {
            return;
        };
        let len = len as u64;
        for (_devid, phys) in stripes {
            let off = self.base_offset.saturating_add(phys);

            advise_willneed(&self.fp, off, len);
        }
    }

    pub fn strategy(&self) -> Option<&CsumStrategy> {
        self.strategy.as_ref()
    }

    pub fn read_node(
        &mut self,
        chunk_map: &ChunkMap,
        logical: u64,
        expected_generation: u64,
        expected_level: Option<u8>,
        expected_owner: Option<u64>,
    ) -> std::io::Result<ReadNodeResult> {
        let strategy = match &self.strategy {
            Some(s) => s,
            None => {
                let buf = self.read_logical(chunk_map, logical, self.node_size)?;
                return Ok(ReadNodeResult {
                    node: Some(super::node::Node::parse(buf)),
                    all_mirrors_failed: false,
                    generation_mismatch: false,
                    mirror_mismatch: false,
                });
            }
        };

        let stripes = chunk_map
            .lookup_stripes(logical)
            // Unmapped address: fall back to a fake stripe so the read
            // fails verification and the node is reported unverifiable.
            .unwrap_or_else(|| vec![(0u64, 0u64)]);

        let mut good: Option<Vec<u8>> = None;
        let mut stale: Option<Vec<u8>> = None;
        let mut corrupt: Option<Vec<u8>> = None;

        // Try every mirror copy: keep the first good one, remember stale
        // (generation mismatch) and corrupt (header failure) copies for
        // diagnostics.
        let mut valid_count: usize = 0;
        for (_devid, phys) in &stripes {
            let buf = match pread_at(&self.fp, self.base_offset + phys, self.node_size) {
                Ok(b) => b,
                Err(_) => continue,
            };
            if !strategy.verify_node_header(&buf) {
                if corrupt.is_none() {
                    corrupt = Some(buf);
                }
                continue;
            }
            valid_count += 1;

            let hdr = super::node::Header::parse(&buf);
            if !self.validate_header(
                &hdr,
                logical,
                expected_generation,
                expected_level,
                expected_owner,
            ) {
                if stale.is_none() {
                    stale = Some(buf);
                }
                continue;
            }

            if good.is_none() {
                good = Some(buf);
            }
        }
        let generation_mismatch = good.is_none() && stale.is_some();

        let mirror_mismatch = stripes.len() > 1 && valid_count > 0 && valid_count < stripes.len();
        let buf = match good.or(stale).or(corrupt) {
            Some(b) => b,
            None => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("could not read any stripe for node at logical 0x{logical:x}"),
                ));
            }
        };

        // No copy had a valid header checksum: surface the node as
        // unreadable metadata rather than parsing garbage.
        let all_mirrors_failed = !strategy.verify_node_header(&buf);
        if all_mirrors_failed {
            eprintln!(
                "metadata header csum mismatch at logical 0x{logical:x} \
                 (no verifiable copy; {} mirror(s) read)",
                stripes.len()
            );

            return Ok(ReadNodeResult {
                node: None,
                all_mirrors_failed,
                generation_mismatch,
                mirror_mismatch,
            });
        }
        Ok(ReadNodeResult {
            node: Some(super::node::Node::parse(buf)),
            all_mirrors_failed,
            generation_mismatch,
            mirror_mismatch,
        })
    }

    /// Check a node header against the expected logical address, fsid, level,
    /// owner, and generation (checks skipped when no expectation is given).
    pub fn validate_header(
        &self,
        hdr: &super::node::Header,
        logical: u64,
        expected_generation: u64,
        expected_level: Option<u8>,
        expected_owner: Option<u64>,
    ) -> bool {
        // All checks are optional except bytenr: the node must sit where
        // its parent's key pointer claimed it sits.
        if hdr.bytenr != logical {
            return false;
        }

        if let Some(fsid) = self.fsid
            && hdr.fsid != fsid
        {
            return false;
        }

        if let Some(lvl) = expected_level
            && hdr.level != lvl
        {
            return false;
        }

        if let Some(owner) = expected_owner
            && hdr.owner != owner
        {
            return false;
        }

        if expected_generation != GEN_DONT_CHECK && hdr.generation != expected_generation {
            return false;
        }
        true
    }
}
