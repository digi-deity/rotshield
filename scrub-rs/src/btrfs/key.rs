// Well-known btrfs tree objectids; also the objectid of each tree's
// ROOT_ITEM key in the root tree.
pub mod objectid {
    pub const ROOT_TREE: u64 = 1;
    pub const EXTENT_TREE: u64 = 2;
    pub const CHUNK_TREE: u64 = 3;
    pub const DEV_TREE: u64 = 4;
    pub const FS_TREE: u64 = 5;
    pub const CSUM_TREE: u64 = 7;
    pub const UUID_TREE: u64 = 9;
    pub const FREE_SPACE_TREE: u64 = 10;
    // -10 as u64: the objectid under which EXTENT_CSUM items live in the
    // csum tree.
    pub const EXTENT_CSUM_OBJECTID: u64 = 0xFFFF_FFFF_FFFF_FFF6;
}

// btrfs on-disk key type numbers.
pub mod key_type {
    pub const INODE_ITEM: u8 = 1;
    pub const INODE_REF: u8 = 12;
    pub const DIR_ITEM: u8 = 84;
    pub const DIR_INDEX: u8 = 96;
    pub const EXTENT_DATA: u8 = 108;
    pub const EXTENT_CSUM: u8 = 128;
    pub const ROOT_ITEM: u8 = 132;
    pub const EXTENT_ITEM: u8 = 168;
    pub const METADATA_ITEM: u8 = 169;
    pub const DEV_EXTENT_KEY: u8 = 204;
    pub const DEV_ITEM: u8 = 216;
    pub const CHUNK_ITEM: u8 = 228;
}

// Block-group type / chunk profile flag bits (the chunk item's type field).
pub mod bg_flag {
    pub const DATA: u64 = 1 << 0;
    pub const SYSTEM: u64 = 1 << 1;
    pub const METADATA: u64 = 1 << 2;
    pub const RAID0: u64 = 1 << 3;
    pub const RAID1: u64 = 1 << 4;
    pub const DUP: u64 = 1 << 5;
    pub const RAID10: u64 = 1 << 6;
    pub const RAID5: u64 = 1 << 7;
    pub const RAID6: u64 = 1 << 8;
    pub const RAID1C3: u64 = 1 << 9;
    pub const RAID1C4: u64 = 1 << 10;

    // Profiles that keep more than one copy of each block (mirrors / DUP);
    // used to detect mirrored chunks.
    pub const MIRROR_MASK: u64 = RAID1 | DUP | RAID1C3 | RAID1C4;

    // Profiles that stripe blocks across multiple devices.
    pub const STRIPED_MASK: u64 = RAID0 | RAID10 | RAID5 | RAID6;
}

/// A btrfs key: (objectid, type, offset), 17 bytes on disk.
/// Items in a node are ordered lexicographically by key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Key {
    pub objectid: u64,
    pub ty: u8,
    pub offset: u64,
}

impl Key {
    pub fn new(objectid: u64, ty: u8, offset: u64) -> Self {
        Self {
            objectid,
            ty,
            offset,
        }
    }

    // 17-byte on-disk layout: objectid (8) | type (1) | offset (8),
    // all little-endian.
    pub fn parse(buf: &[u8], pos: usize) -> Self {
        let objectid = u64::from_le_bytes([
            buf[pos],
            buf[pos + 1],
            buf[pos + 2],
            buf[pos + 3],
            buf[pos + 4],
            buf[pos + 5],
            buf[pos + 6],
            buf[pos + 7],
        ]);
        let ty = buf[pos + 8];
        let offset = u64::from_le_bytes([
            buf[pos + 9],
            buf[pos + 10],
            buf[pos + 11],
            buf[pos + 12],
            buf[pos + 13],
            buf[pos + 14],
            buf[pos + 15],
            buf[pos + 16],
        ]);
        Self {
            objectid,
            ty,
            offset,
        }
    }
}

/// A child pointer in an internal node: 17-byte key, the child node's
/// logical address, and the child's expected generation (33 bytes on disk).
#[derive(Debug, Clone, Copy)]
pub struct KeyPtr {
    pub key: Key,
    /// Logical address of the child node; mapped through the chunk tree.
    pub blockptr: u64,
    pub generation: u64,
}

impl KeyPtr {
    // 33-byte on-disk layout: key (17) | blockptr (8) | generation (8).
    pub fn parse(buf: &[u8], pos: usize) -> Self {
        let key = Key::parse(buf, pos);
        let blockptr = u64::from_le_bytes([
            buf[pos + 17],
            buf[pos + 18],
            buf[pos + 19],
            buf[pos + 20],
            buf[pos + 21],
            buf[pos + 22],
            buf[pos + 23],
            buf[pos + 24],
        ]);
        let generation = u64::from_le_bytes([
            buf[pos + 25],
            buf[pos + 26],
            buf[pos + 27],
            buf[pos + 28],
            buf[pos + 29],
            buf[pos + 30],
            buf[pos + 31],
            buf[pos + 32],
        ]);
        Self {
            key,
            blockptr,
            generation,
        }
    }
}
