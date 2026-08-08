//! Minimal btrfs superblock parsing: magic, header checksum, and tree roots.

use std::io::{self, Read, Seek};

use super::csum_strategy::CsumStrategy;
use super::util::{le_u16, le_u32, le_u64, read_at};

/// Byte offset of the primary superblock (64 KiB).
pub const SUPERBLOCK_OFFSET: u64 = 0x10_000;

/// Mirror copies of the superblock, probed in order when a lower copy fails.
pub const SUPERBLOCK_BACKUP_OFFSETS: &[u64] =
    &[0x10_000, 0x400_0000, 0x40_0000_0000, 0x100_0000_0000];

pub const BTRFS_MAGIC: [u8; 8] = *b"_BHRfS_M";

/// True if `buf` holds the btrfs magic at `offset`. Used by the
/// filesystem's `block_has_magic` probe.
pub fn has_magic_at(buf: &[u8], offset: usize) -> bool {
    buf.len() >= offset + BTRFS_MAGIC.len()
        && buf[offset..offset + BTRFS_MAGIC.len()] == BTRFS_MAGIC
}

pub const BTRFS_SECTOR_SIZE: usize = 4096;

/// The superblock fields needed to navigate the on-disk trees.
#[derive(Debug)]
pub struct Superblock {
    pub fsid: [u8; 16],
    pub bytenr: u64,
    pub magic: [u8; 8],
    pub generation: u64,

    /// Logical address of the root tree's root node.
    pub root: u64,

    /// Logical address of the chunk tree's root node.
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

    /// This device's id. Each NonRAID slot hosts its own single-device
    /// filesystem, so there is exactly one device.
    pub devid: u64,

    /// Raw bytes of the system chunk array, as stored (parsed in chunk.rs).
    pub sys_chunks: Vec<u8>,
}

// Field offsets within the 4 KiB superblock block.
const OFF_FSID: usize = 32;
const OFF_BYTENR: usize = 48;

pub const OFF_MAGIC: usize = 64;
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

// dev_item.devid: csum_type ends at 198, then 3 *_level bytes, then the
// 98-byte dev_item starts (its first field is devid).
const OFF_DEVID: usize = 198 + 3;

// The system chunk array follows the fixed portion: level bytes, dev_item,
// label, uuid fields, and reserved bytes.
const OFF_SYS_CHUNKS: usize = 811;

impl Superblock {
    /// Read the superblock, trying each reachable mirror copy in order and
    /// using the first that passes magic + header-checksum verification.
    pub fn read<R: Read + Seek>(fp: &mut R, offset: u64) -> std::io::Result<Self> {
        let mut last_err: Option<io::Error> = None;
        for &sb_off in SUPERBLOCK_BACKUP_OFFSETS {
            let abs_off = offset + sb_off;
            match Self::read_one(fp, abs_off) {
                Ok(sb) => return Ok(sb),

                // Short read: the copy lies past the device end — skip it.
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => continue,

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

    /// Read only the primary copy, without fallback — lets the open path
    /// detect a wiped primary that still has an intact backup.
    pub fn read_primary<R: Read + Seek>(fp: &mut R, offset: u64) -> std::io::Result<Self> {
        let primary_off = SUPERBLOCK_BACKUP_OFFSETS
            .first()
            .copied()
            .unwrap_or(0x10_000);
        Self::read_one(fp, offset + primary_off)
    }

    /// Read and verify one superblock copy at an absolute byte offset.
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

        // The header checksum covers csum_type itself, so a corrupt
        // strategy id fails verification here.
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
