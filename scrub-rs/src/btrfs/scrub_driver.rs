//! The btrfs FilesystemScrub implementation: wires the reader, lazy csum
//! walker, and dev-extent scrub into the fs contract.

use std::io;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use crate::btrfs::chunk::ChunkMap;
use crate::btrfs::csum::LazyCsumProvider;
use crate::btrfs::csum_strategy::CsumStrategy;
use crate::btrfs::dev_extent::build_dev_extents;
use crate::btrfs::extent::reconfirm_mismatch;
use crate::btrfs::key::bg_flag;
use crate::btrfs::reader::FsReader;
use crate::btrfs::scrub::scrub_dev_tree;
use crate::btrfs::superblock::Superblock;
use crate::fs;
use crate::fs::{Reconfirm, ReconfirmRequest, Reconfirmer, ScrubEvent, ScrubStats, SectorVerifier};
use crate::status::StatusCounters;

/// A single-device btrfs scrub over one backing file.
pub struct BtrfsScrub {
    reader: FsReader,
    chunk_map: ChunkMap,

    /// Lazy CSUM_TREE walker feeding per-extent checksum streams.
    csum_provider: LazyCsumProvider,

    /// This device's physical extents, from the DEV_TREE.
    dev_extents: Vec<crate::btrfs::dev_extent::DevExtent>,
    strategy: CsumStrategy,
    superblock: Superblock,

    /// Open-time metadata failures; the csum walker adds its own at run time.
    metadata_header_errors: u64,

    /// Open-time mirror divergences; the csum walker adds its own.
    metadata_mirror_mismatches: u64,

    /// Open-time metadata read errors; the csum walker adds its own.
    metadata_read_errors: u64,

    dev: String,

    /// Live status counters, when a status server is attached.
    status: Option<Arc<StatusCounters>>,
}

impl BtrfsScrub {
    /// Open the filesystem and prepare the scrub: chunk map, csum provider,
    /// and this device's dev-extent list.
    pub fn open(dev: &str, base_offset: u64) -> io::Result<Self> {
        let ctx = crate::btrfs::open(dev, base_offset)?;
        let crate::btrfs::BtrfsContext {
            mut reader,
            chunk_map,
            superblock,
            roots,
            strategy,
            mut metadata_header_errors,
            mut metadata_mirror_mismatches,
            mut metadata_read_errors,
        } = ctx;

        // The lazy csum walker gets its own fd so its seeks never race
        // the scrub reader's.
        let lazy_file = reader.reopen()?;
        let csum_provider = LazyCsumProvider::new(
            lazy_file,
            superblock.node_size as usize,
            reader.base_offset(),
            strategy,
            superblock.devid,
            superblock.fsid,
            chunk_map.clone(),
            roots.csum_root,
        );

        // The dev-extent list is what the physical-order scrub walks.
        let dev_extents = match roots.dev_tree_root {
            Some(dev_tree_root) => build_dev_extents(
                &mut reader,
                &chunk_map,
                dev_tree_root,
                superblock.devid,
                &mut metadata_header_errors,
                &mut metadata_mirror_mismatches,
                &mut metadata_read_errors,
            )?,

            // DEV_TREE unresolvable: nothing to scrub; the run reports
            // METADATA FATAL instead of silently skipping.
            None => {
                metadata_header_errors += 1;
                eprintln!(
                    "note: DEV_TREE root could not be resolved (the root-tree branch \
                     carrying its ROOT_ITEM failed metadata verification). The dev-extent \
                     set is EMPTY — no data extents can be scrubbed; the run will report \
                     METADATA FATAL (exit 6)."
                );
                Vec::new()
            }
        };

        Ok(Self {
            reader,
            chunk_map,
            csum_provider,
            dev_extents,
            strategy,
            superblock,
            metadata_header_errors,
            metadata_mirror_mismatches,
            metadata_read_errors,
            dev: dev.to_string(),
            status: None,
        })
    }

    /// Attach the live status counters and seed the progress and metadata totals.
    pub fn set_status(&mut self, counters: Arc<StatusCounters>) {
        let progress_total: u64 = self
            .dev_extents
            .iter()
            .filter(|d| {
                self.chunk_map
                    .info(d.chunk_offset)
                    .is_some_and(|c| c.flags & bg_flag::DATA != 0)
            })
            .map(|d| d.length)
            .sum();
        counters
            .progress_total
            .store(progress_total, Ordering::Relaxed);

        counters.metadata_header_errors.store(
            self.metadata_header_errors + self.csum_provider.metadata_errors(),
            Ordering::Relaxed,
        );
        counters
            .metadata_mirror_mismatches
            .store(self.metadata_mirror_mismatches, Ordering::Relaxed);
        counters.metadata_read_errors.store(
            self.metadata_read_errors + self.csum_provider.metadata_read_errors(),
            Ordering::Relaxed,
        );

        self.status = Some(counters);
    }
}

impl fs::FilesystemScrub for BtrfsScrub {
    /// Run the scrub, emitting one log line and one recovery event per
    /// mismatched sector.
    fn run(&mut self, callbacks: &mut dyn crate::fs::ScrubCallbacks) -> io::Result<ScrubStats> {
        let batch = callbacks.wants_raw_candidates();

        let block_size = self.strategy.sector_size as usize;
        let mut emit = |r: &crate::btrfs::scrub::SectorResult| {
            let stored_tag = match &r.stored_csum {
                None => format!(
                    "actual=0x{} (no stored csum)",
                    crate::btrfs::util::hex(&r.actual_csum)
                ),
                Some(stored) => format!(
                    "stored=0x{} actual=0x{}",
                    crate::btrfs::util::hex(stored),
                    crate::btrfs::util::hex(&r.actual_csum),
                ),
            };
            let line = format!(
                "  MISMATCH logical=0x{:x} devid={} array_phys=0x{:x} ino={} off=0x{:x} {stored_tag}",
                r.logical, r.devid, r.array_phys, r.inode, r.file_offset,
            );
            callbacks.on_log(&line);

            // Without a stored checksum there is nothing to verify or
            // re-confirm; the event carries None for both.
            let (verify, reconfirm) = match r.stored_csum.as_ref() {
                None => (None, None),
                Some(stored) => {
                    let stored = stored.clone();
                    let strategy = self.strategy;
                    let verify = {
                        let s = stored.clone();
                        Arc::new(move |b: &[u8]| strategy.compute(b) == s) as SectorVerifier
                    };
                    let reconfirm = ReconfirmRequest {
                        token: r.logical,
                        stored_csum: stored,
                    };
                    (Some(verify), Some(reconfirm))
                }
            };
            callbacks.on_event(&ScrubEvent {
                array_phys: r.array_phys,
                block_size,
                verify,
                reconfirm,

                unreadable: r.unreadable,
            });
        };

        let local = scrub_dev_tree(
            &mut self.reader,
            &self.chunk_map,
            &mut self.csum_provider,
            &self.dev_extents,
            &self.strategy,
            batch,
            self.status.as_deref(),
            &mut emit,
        )?;

        Ok(ScrubStats {
            sectors_checked: local.sectors_checked,
            sectors_ok: local.sectors_ok,
            sectors_mismatch: local.sectors_mismatch,
            sectors_no_csum: local.sectors_no_csum,
            sectors_read_error: local.sectors_read_error,
            sectors_stale: local.sectors_stale,
            stale_csum_branches: local.stale_csum_branches,
            isolation_truncated: local.isolation_truncated,
            bytes_checked: local.bytes_checked,
            metadata_header_errors: self.metadata_header_errors
                + self.csum_provider.metadata_errors(),
            metadata_mirror_mismatches: self.metadata_mirror_mismatches
                + self.csum_provider.mirror_mismatches(),
            metadata_read_errors: self.metadata_read_errors
                + self.csum_provider.metadata_read_errors(),
        })
    }

    /// A re-confirmation handle with its own reader (never shares the scrub's
    /// seek position).
    fn reconfirmer(&self) -> io::Result<Box<dyn Reconfirmer>> {
        BtrfsReconfirmer::new(self).map(|r| Box::new(r) as Box<dyn Reconfirmer>)
    }

    /// Multi-line human-readable filesystem summary for the pre-scrub dump.
    fn describe(&self) -> Vec<String> {
        let sb = &self.superblock;
        let strategy = &self.strategy;

        let num_sectors: u64 = self
            .dev_extents
            .iter()
            .map(|e| e.length / strategy.sector_size)
            .sum();
        vec![
            format!("device        : {}", self.dev),
            format!(
                "base offset   : 0x{:x} ({})",
                self.reader.base_offset(),
                self.reader.base_offset()
            ),
            format!("magic         : {:?}", sb.magic),
            format!("fsid          : {}", crate::btrfs::util::hex(&sb.fsid)),
            format!("bytenr        : 0x{:x}", sb.bytenr),
            format!("generation   : {}", sb.generation),
            format!("root          : 0x{:x}", sb.root),
            format!("chunk_root    : 0x{:x}", sb.chunk_root),
            format!("total_bytes   : {}", sb.total_bytes),
            format!("bytes_used    : {}", sb.bytes_used),
            format!("num_devices   : {}", sb.num_devices),
            format!("sector_size   : {}", sb.sector_size),
            format!("node_size     : {}", sb.node_size),
            format!("stripesize    : {}", sb.stripesize),
            format!("csum_type     : {} ({})", sb.csum_type, strategy.name),
            format!(
                "csum sectors  : {} ({} bytes)",
                num_sectors,
                num_sectors * strategy.sector_size
            ),
            format!("dev extents   : {}", self.dev_extents.len()),
        ]
    }

    fn superblock_offset(&self) -> u64 {
        crate::btrfs::superblock::SUPERBLOCK_OFFSET
    }

    fn block_has_magic(&self, block: &[u8]) -> bool {
        crate::btrfs::superblock::has_magic_at(block, crate::btrfs::superblock::OFF_MAGIC)
    }
}

/// Re-confirms mismatched sectors against live metadata at write time.
struct BtrfsReconfirmer {
    reader: FsReader,
    chunk_map: ChunkMap,
    strategy: CsumStrategy,

    counters: Option<Arc<StatusCounters>>,

    /// (superblock generation, extent_root, csum_root); reused while the
    /// generation still matches.
    cached_roots: Option<(u64, u64, u64)>,
}

impl BtrfsReconfirmer {
    fn new(scrub: &BtrfsScrub) -> io::Result<Self> {
        let f = scrub.reader.reopen()?;
        let reader = FsReader::new(
            f,
            scrub.reader.node_size(),
            scrub.reader.base_offset(),
            Some(scrub.strategy),
        )
        .with_devid(scrub.superblock.devid)
        .with_fsid(scrub.superblock.fsid);
        Ok(Self {
            reader,
            chunk_map: scrub.chunk_map.clone(),
            strategy: scrub.strategy,

            counters: scrub.status.clone(),
            cached_roots: None,
        })
    }
}

impl Reconfirmer for BtrfsReconfirmer {
    /// Re-check one sector: read the live superblock, resolve the live roots,
    /// and compare the live checksum against the stored one.
    fn reconfirm(&mut self, req: &ReconfirmRequest) -> Reconfirm {
        let base_offset = self.reader.base_offset();

        // A live superblock that cannot be read makes the sector
        // unverifiable — skip the write.
        let sb = match crate::btrfs::open::read_live_superblock(
            &self.reader,
            base_offset,
            self.counters.as_deref(),
        ) {
            Some(sb) => sb,
            None => return Reconfirm::Unverifiable,
        };

        // Re-resolve the roots only when the superblock generation
        // has moved on since the last re-confirmation.
        let roots = match cached_roots_at(self.cached_roots, sb.generation) {
            Some(roots) => Some(roots),
            None => {
                let r = crate::btrfs::open::resolve_live_tree_roots(
                    &mut self.reader,
                    &self.chunk_map,
                    self.counters.as_deref(),
                    &sb,
                );
                if let Some((ext_root, csum_root)) = r {
                    self.cached_roots = Some((sb.generation, ext_root, csum_root));
                }
                r
            }
        };

        match roots {
            None => Reconfirm::Unverifiable,
            Some((ext_root, csum_root)) => reconfirm_mismatch(
                &mut self.reader,
                &self.chunk_map,
                ext_root,
                csum_root,
                req.token,
                &req.stored_csum,
                self.strategy.hash_len,
                self.strategy.sector_size,
            ),
        }
    }
}

/// Return the cached roots only when the generation still matches.
fn cached_roots_at(cached: Option<(u64, u64, u64)>, generation: u64) -> Option<(u64, u64)> {
    match cached {
        Some((cached_gen, ext, csum)) if cached_gen == generation => Some((ext, csum)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::cached_roots_at;

    #[test]
    fn cache_hit_only_at_exact_generation() {
        let cached = Some((42u64, 0x1000u64, 0x2000u64));

        assert_eq!(cached_roots_at(cached, 42), Some((0x1000, 0x2000)));

        assert_eq!(cached_roots_at(cached, 43), None);

        assert_eq!(cached_roots_at(cached, 41), None);

        assert_eq!(cached_roots_at(None, 42), None);
    }
}
