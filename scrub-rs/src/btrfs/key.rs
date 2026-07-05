//! btrfs on-disk key types and constants.
//!
//! See btrfs_recon/structure/key.py for the source-of-truth enum values.

/// Special objectids that name the well-known btrfs trees.
pub mod objectid {
    pub const ROOT_TREE: u64 = 1;
    pub const EXTENT_TREE: u64 = 2;
    pub const CHUNK_TREE: u64 = 3;
    pub const DEV_TREE: u64 = 4;
    pub const FS_TREE: u64 = 5;
    pub const CSUM_TREE: u64 = 7;
    pub const UUID_TREE: u64 = 9;
    pub const FREE_SPACE_TREE: u64 = 10;
}

/// btrfs key type byte — discriminates the payload of a leaf item / the
/// meaning of a key in an internal node.
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
    pub const DEV_ITEM: u8 = 216;
    pub const CHUNK_ITEM: u8 = 228;
}

/// Block-group / chunk type flags (selected).
pub mod bg_flag {
    pub const DATA: u64 = 1 << 0;
    pub const SYSTEM: u64 = 1 << 1;
    pub const METADATA: u64 = 1 << 2;
    pub const RAID1: u64 = 1 << 4;
    pub const DUP: u64 = 1 << 5;
    pub const RAID1C3: u64 = 1 << 9;
    pub const RAID1C4: u64 = 1 << 10;

    /// Any flag that means each stripe is a full mirror copy.
    pub const MIRROR_MASK: u64 =
        RAID1 | DUP | RAID1C3 | RAID1C4;
}

/// A btrfs disk key: (objectid, type, offset).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Key {
    pub objectid: u64,
    pub ty: u8,
    pub offset: u64,
}

impl Key {
    /// Parse a 17-byte key from `buf` at `pos`.
    pub fn parse(buf: &[u8], pos: usize) -> Self {
        let objectid = u64::from_le_bytes([
            buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3],
            buf[pos + 4], buf[pos + 5], buf[pos + 6], buf[pos + 7],
        ]);
        let ty = buf[pos + 8];
        let offset = u64::from_le_bytes([
            buf[pos + 9], buf[pos + 10], buf[pos + 11], buf[pos + 12],
            buf[pos + 13], buf[pos + 14], buf[pos + 15], buf[pos + 16],
        ]);
        Self { objectid, ty, offset }
    }
}

/// An internal-node key pointer: (key, blockptr, generation).
#[derive(Debug, Clone, Copy)]
pub struct KeyPtr {
    pub key: Key,
    /// Logical bytenr of the child node.
    pub blockptr: u64,
    pub generation: u64,
}

impl KeyPtr {
    /// Parse a 25-byte keyptr (17-byte key + 8-byte blockptr + 8-byte gen).
    pub fn parse(buf: &[u8], pos: usize) -> Self {
        let key = Key::parse(buf, pos);
        let blockptr = u64::from_le_bytes([
            buf[pos + 17], buf[pos + 18], buf[pos + 19], buf[pos + 20],
            buf[pos + 21], buf[pos + 22], buf[pos + 23], buf[pos + 24],
        ]);
        let generation = u64::from_le_bytes([
            buf[pos + 25], buf[pos + 26], buf[pos + 27], buf[pos + 28],
            buf[pos + 29], buf[pos + 30], buf[pos + 31], buf[pos + 32],
        ]);
        Self { key, blockptr, generation }
    }
}