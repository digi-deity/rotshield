//! Minimal btrfs superblock parsing.
//!
//! Only reads the handful of fields the scrub needs: magic verification,
//! fsid, root/chunk_root bytenr, node/sector sizes, and the system chunk
//! array (the bootstrap needed to walk the chunk tree).

use std::io::{self, Read, Seek};

use super::csum_strategy::CsumStrategy;
use super::util::{le_u16, le_u32, le_u64, read_at};

/// Offset of the primary superblock on the device (64 KiB).
pub const SUPERBLOCK_OFFSET: u64 = 0x10_000;
/// btrfs keeps mirror copies of the superblock at well-known offsets so the
/// filesystem can still be opened if the primary is corrupted.  The primary
/// lives at 64 KiB; further backups exist at 64 MiB, 256 GiB, and 1 TiB.
/// btrfs only writes a given backup if the device is large enough to contain
/// it (a backup at offset X is meaningless on a device smaller than X+4KiB),
/// so [`Superblock::read`] only probes a copy when the device can hold it.
/// We try each reachable copy in order and use the first that passes magic +
/// header-checksum verification — exactly as the kernel and `btrfs check` do.
pub const SUPERBLOCK_BACKUP_OFFSETS: &[u64] = &[
    0x10_000,        // 64 KiB  — primary
    0x400_0000,      // 64 MiB  — backup 1
    0x40_0000_0000,  // 256 GiB — backup 2
    0x100_0000_0000, // 1 TiB   — backup 3
];
/// btrfs magic bytes.
pub const BTRFS_MAGIC: [u8; 8] = *b"_BHRfS_M";
/// Bytes of CRC32C stored at the start of a metadata/superblock header.
pub const BTRFS_CSUM_SIZE: usize = 32;
/// Size of a btrfs data sector (checksum granularity).
pub const BTRFS_SECTOR_SIZE: usize = 4096;

/// A handful of superblock fields sufficient for navigating the on-disk
/// trees of a single-device btrfs filesystem.
#[derive(Debug)]
pub struct Superblock {
    pub fsid: [u8; 16],
    pub bytenr: u64,
    pub magic: [u8; 8],
    pub generation: u64,
    /// Logical address of the root-tree root.
    pub root: u64,
    /// Logical address of the chunk-tree root.
    pub chunk_root: u64,
    pub total_bytes: u64,
    pub bytes_used: u64,
    pub num_devices: u64,
    pub sector_size: u32,
    pub node_size: u32,
    pub stripesize: u32,
    pub sys_chunk_array_size: u32,
    pub chunk_root_generation: u64,
    pub csum_type: u16,
    /// This device's id, taken from `dev_item.devid` in the superblock.
    /// Each NonRAID slot is its own single-device filesystem, so there is
    /// exactly one device; the physical-order scrub uses this to drive its
    /// DEV_TREE walk (keyed by devid) and to guard `FsReader::read_physical`
    /// against reading the wrong disk.
    pub devid: u64,
    /// Raw bytes of the system-chunk bootstrap array, exactly as stored
    /// on disk — caller is responsible for parsing it.
    pub sys_chunks: Vec<u8>,
}

/// Field offsets inside the (4 KiB) superblock block.
//
// Layout reference: btrfs_recon/structure/superblock.py
//   +0   csum[32]
//   +32  fsid[16]
//   +48  bytenr u64
//   +56  flags u64
//   +64  magic[8]
//   +72  generation u64
//   +80  root u64
//   +88  chunk_root u64
//   +96  log_root u64
//   +104 log_root_transid u64
//   +112 total_bytes u64
//   +120 bytes_used u64
//   +128 root_dir_objectid u64
//   +136 num_devices u64
//   +144 sector_size u32
//   +148 node_size u32
//   +152 leafsize u32 (== node_size)
//   +156 stripesize u32
//   +160 sys_chunk_array_size u32
//   +164 chunk_root_generation u64
//   +172 compat_flags u64
//   +180 compat_ro_flags u64
//   +188 incompat_flags u64
//   +196 csum_type u16
//   +198 root_level u8
//   +199 chunk_root_level u8
const OFF_FSID: usize = 32;
const OFF_BYTENR: usize = 48;
const OFF_MAGIC: usize = 64;
const OFF_GENERATION: usize = 72;
const OFF_ROOT: usize = 80;
const OFF_CHUNK_ROOT: usize = 88;
const OFF_TOTAL_BYTES: usize = 112;
const OFF_BYTES_USED: usize = 120;
const OFF_NUM_DEVICES: usize = 136;
const OFF_SECTOR_SIZE: usize = 144;
const OFF_NODE_SIZE: usize = 148;
const OFF_STRIPESIZE: usize = 156;
const OFF_SYS_CHUNK_ARRAY_SIZE: usize = 160;
const OFF_CHUNK_ROOT_GENERATION: usize = 164;
const OFF_CSUM_TYPE: usize = 196;
// dev_item.devid sits at the start of the 98-byte dev_item structure, which
// begins right after the 3 *_level bytes that follow csum_type.  Verified
// against a real mkfs.btrfs image (matches btrfs2/superblock.rs).
const OFF_DEVID: usize = 198 + 3;

// The system chunk array follows the fixed-size portion of the superblock.
// Everything between csum_type (end @198) and sys_chunks is fixed: 3 bytes of
// *_level, 98-byte dev_item, 256-byte label, cache_generation(8),
// uuid_tree_generation(8), metadata_uuid(16), 224 bytes of reserved.
const OFF_SYS_CHUNKS: usize = 811;

impl Superblock {
    /// Read and parse the superblock from `fp`, falling back through the
    /// btrfs superblock mirror copies if the primary is unusable.
    ///
    /// `offset` is the byte offset of the start of the btrfs partition
    /// within the underlying file/device (0 for a bare btrfs image or an
    /// array partition like /dev/nmd1p1; the partition's start sector for a
    /// whole-disk image or a raw rdev that needs rdevOffset added).
    ///
    /// btrfs stores mirror copies of the superblock at well-known offsets
    /// (see [`SUPERBLOCK_BACKUP_OFFSETS`]); we try each *reachable* copy in
    /// order and use the first that passes both magic and header-checksum
    /// verification.  A copy whose 4096-byte block would extend past
    /// end-of-device yields a short read, which we treat as "this backup
    /// isn't present on this device" and skip — so larger backups (256 GiB,
    /// 1 TiB) are only consulted when the device is actually big enough to
    /// hold them.  This means a corrupted primary superblock (e.g. a wiped
    /// 64 KiB copy) no longer makes the tool refuse outright — it
    /// transparently uses an intact backup, exactly as the kernel and `btrfs
    /// check` do.  A superblock is accepted only after its own 32-byte header
    /// checksum is verified with the same [`CsumStrategy::verify_header`]
    /// primitive every tree node uses, so the trust chain still starts from a
    /// verified block.
    pub fn read<R: Read + Seek>(fp: &mut R, offset: u64) -> std::io::Result<Self> {
        // We probe each superblock mirror copy in order and use the first
        // that passes magic + header-checksum verification.  A copy whose
        // 4096-byte block would extend past end-of-device yields a short read
        // (UnexpectedEof); we treat that as "this copy isn't present on this
        // device" and skip to the next, rather than failing the whole open.
        // This makes the fallback size-aware without needing to stat `fp`
        // (which the generic `R: Read + Seek` bound doesn't allow), and works
        // for any seekable reader — a regular file, a block device, or a pipe.
        let mut last_err: Option<io::Error> = None;
        for &sb_off in SUPERBLOCK_BACKUP_OFFSETS {
            let abs_off = offset + sb_off;
            match Self::read_one(fp, abs_off) {
                Ok(sb) => return Ok(sb),
                // A short read means this backup copy doesn't physically exist
                // on this device (device too small) — skip it, try the next.
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => continue,
                // Keep the most informative error (the primary's) but try the
                // next reachable copy; only surface it if every copy fails.
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "no readable btrfs superblock found",
            )
        }))
    }

    /// Read and parse a single superblock copy at absolute byte `abs_off`.
    ///
    /// Verifies magic first (cheap reject for a wiped/zeroed block), then the
    /// superblock's OWN 32-byte header checksum using the shared
    /// [`CsumStrategy::verify_header`] primitive — the superblock is just a
    /// 4096-byte node with the identical csum-prefix layout (csum[32] at
    /// offset 0, body from offset 32).  The chicken-and-egg is resolved by
    /// *ordering*: we read the raw block, peek `csum_type` (offset 196, itself
    /// Read and parse *only* the primary superblock copy (the 64 KiB mirror
    /// at [`SUPERBLOCK_BACKUP_OFFSETS`]\[0\]), without falling back to the
    /// backup copies.
    ///
    /// This is used by the open path to detect a wiped/primary-with-intact-
    /// backup situation: the main [`Superblock::read`] transparently falls
    /// back to a backup when the primary is unusable, so a caller that wants
    /// to *know* whether the primary itself is bad must probe it in
    /// isolation. A bad primary is a recoverable metadata divergence (the
    /// filesystem is still fully readable via a backup), so this is
    /// deliberately non-fatal for the overall scrub.
    ///
    /// `offset` is the byte offset of the start of the btrfs partition within
    /// the underlying file/device (same meaning as in [`Superblock::read`]).
    pub fn read_primary<R: Read + Seek>(fp: &mut R, offset: u64) -> std::io::Result<Self> {
        let primary_off = SUPERBLOCK_BACKUP_OFFSETS
            .first()
            .copied()
            .unwrap_or(0x10_000);
        Self::read_one(fp, offset + primary_off)
    }

    /// inside the checksummed body), and verify — the algorithm id is covered
    /// by the very checksum we are about to check, so a corrupt `csum_type`
    /// simply fails verification.
    fn read_one<R: Read + Seek>(fp: &mut R, abs_off: u64) -> std::io::Result<Self> {
        let buf = read_at(fp, abs_off, 4096)?;

        let mut magic = [0u8; 8];
        magic.copy_from_slice(&buf[OFF_MAGIC..OFF_MAGIC + 8]);
        if magic != BTRFS_MAGIC {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "not a btrfs superblock: bad magic {:?} (expected {:?})",
                    magic, BTRFS_MAGIC
                ),
            ));
        }
        // Verify this copy's OWN 32-byte header checksum (see `read_one` doc).
        let csum_type = le_u16(&buf, OFF_CSUM_TYPE);
        if !CsumStrategy::verify_header(csum_type, &buf)? {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "superblock header checksum mismatch (block corrupt or not a btrfs superblock)",
            ));
        }
        let mut fsid = [0u8; 16];
        fsid.copy_from_slice(&buf[OFF_FSID..OFF_FSID + 16]);

        let sys_chunk_array_size = le_u32(&buf, OFF_SYS_CHUNK_ARRAY_SIZE) as usize;
        let sys_chunks = if OFF_SYS_CHUNKS + sys_chunk_array_size <= buf.len() {
            buf[OFF_SYS_CHUNKS..OFF_SYS_CHUNKS + sys_chunk_array_size].to_vec()
        } else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "system chunk array overruns superblock",
            ));
        };

        Ok(Self {
            fsid,
            bytenr: le_u64(&buf, OFF_BYTENR),
            magic,
            generation: le_u64(&buf, OFF_GENERATION),
            root: le_u64(&buf, OFF_ROOT),
            chunk_root: le_u64(&buf, OFF_CHUNK_ROOT),
            total_bytes: le_u64(&buf, OFF_TOTAL_BYTES),
            bytes_used: le_u64(&buf, OFF_BYTES_USED),
            num_devices: le_u64(&buf, OFF_NUM_DEVICES),
            sector_size: le_u32(&buf, OFF_SECTOR_SIZE),
            node_size: le_u32(&buf, OFF_NODE_SIZE),
            stripesize: le_u32(&buf, OFF_STRIPESIZE),
            sys_chunk_array_size: le_u32(&buf, OFF_SYS_CHUNK_ARRAY_SIZE),
            chunk_root_generation: le_u64(&buf, OFF_CHUNK_ROOT_GENERATION),
            csum_type: le_u16(&buf, OFF_CSUM_TYPE),
            devid: le_u64(&buf, OFF_DEVID),
            sys_chunks,
        })
    }
}
