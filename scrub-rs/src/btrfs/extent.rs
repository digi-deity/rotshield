//! FileExtentItem parsing — just enough to find REGULAR data extents.
//!
//! Layout of `struct btrfs_file_extent_item` (non-inline / REGULAR type):
//!   +0   generation u64
//!   +8   ram_bytes u64
//!   +16  compression u8
//!   +17  encryption u8
//!   +18  other_encoding u16
//!   +20  type u8  (0=INLINE, 1=REGULAR, 2=PREALLOC)
//!   +21  -- if not INLINE: ExtentDataRef --
//!        disk_bytenr  u64  (logical addr of the on-disk extent; 0 = sparse)
//!        disk_num_bytes u64 (size of the on-disk extent)
//!        offset       u64  (offset within the extent)
//!        num_bytes    u64  (logical bytes in the file)

use super::util::le_u64;

pub const TYPE_INLINE: u8 = 0;
pub const TYPE_REGULAR: u8 = 1;
pub const TYPE_PREALLOC: u8 = 2;

/// A parsed REGULAR/PREALLOC file extent (the fields the scrub needs).
#[derive(Debug, Clone, Copy)]
pub struct FileExtent {
    /// btrfs key.objectid — the inode this extent belongs to.
    pub inode: u64,
    /// btrfs key.offset — the file offset where this extent starts.
    pub file_offset: u64,
    pub extent_type: u8,
    /// Logical disk address of the extent (0 = sparse/hole).
    pub disk_bytenr: u64,
    pub disk_num_bytes: u64,
    /// Offset within the on-disk extent.
    pub offset: u64,
    /// Number of bytes covered in the file.
    pub num_bytes: u64,
}

impl FileExtent {
    /// Parse a FileExtentItem payload given the key (objectid, offset).
    ///
    /// Returns `None` for INLINE extents (no on-disk data to scrub).
    pub fn parse(buf: &[u8], inode: u64, file_offset: u64) -> Option<Self> {
        if buf.len() < 21 {
            return None;
        }
        let extent_type = buf[20];
        if extent_type == TYPE_INLINE {
            return None;
        }
        if buf.len() < 21 + 32 {
            return None;
        }
        let disk_bytenr = le_u64(buf, 21);
        let disk_num_bytes = le_u64(buf, 29);
        let offset = le_u64(buf, 37);
        let num_bytes = le_u64(buf, 45);
        Some(Self {
            inode,
            file_offset,
            extent_type,
            disk_bytenr,
            disk_num_bytes,
            offset,
            num_bytes,
        })
    }

    /// The logical address of the first byte of this extent's data on disk.
    pub fn disk_start(&self) -> u64 {
        self.disk_bytenr + self.offset
    }
}