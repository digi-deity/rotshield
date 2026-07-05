//! btrfs B-tree node/leaf header and item-slot parsing.
//!
//! On-disk layout of a node/leaf (`struct btrfs_header` + items):
//!   btrfs_header  (101 bytes, *including* the 32-byte csum prefix)
//!   if level == 0: nritems * LeafItem slots   (each 17 + 4 + 4 = 25 bytes)
//!   else:          nritems * KeyPtr           (each 17 + 8 + 8 = 33 bytes)
//!   ... followed by item data (for leaves).
//!
//! Field offsets within the 101-byte header:
//!   +0   csum[32]
//!   +32  fsid[16]
//!   +48  bytenr u64
//!   +56  flags u64
//!   +64  chunk_tree_uuid[16]
//!   +80  generation u64
//!   +88  owner u64
//!   +96  nritems u32
//!   +100 level u8

use super::key::{Key, KeyPtr};

/// Fixed size of the btrfs node/leaf header (`struct btrfs_header`),
/// *including* the 32-byte csum prefix.
pub const HEADER_SIZE: usize = 101;

/// Per-item leaf slot: (key, data_offset, data_size).
///   key         17 bytes
///   data_offset  4 bytes (relative to the *end* of the header+slots block)
///   data_size    4 bytes
pub const LEAF_ITEM_SIZE: usize = 25;

/// Internal-node key pointer size (key + blockptr + generation).
pub const KEYPTR_SIZE: usize = 33;

#[derive(Debug, Clone, Copy)]
pub struct Header {
    pub fsid: [u8; 16],
    pub bytenr: u64,
    pub flags: u64,
    pub chunk_tree_uuid: [u8; 16],
    pub generation: u64,
    pub owner: u64,
    pub nritems: u32,
    pub level: u8,
}

impl Header {
    pub fn parse(buf: &[u8]) -> Self {
        let mut fsid = [0u8; 16];
        fsid.copy_from_slice(&buf[32..48]);
        let bytenr = u64::from_le_bytes(buf[48..56].try_into().unwrap());
        let flags = u64::from_le_bytes(buf[56..64].try_into().unwrap());
        let mut chunk_tree_uuid = [0u8; 16];
        chunk_tree_uuid.copy_from_slice(&buf[64..80]);
        let generation = u64::from_le_bytes(buf[80..88].try_into().unwrap());
        let owner = u64::from_le_bytes(buf[88..96].try_into().unwrap());
        let nritems = u32::from_le_bytes(buf[96..100].try_into().unwrap());
        let level = buf[100];
        Self { fsid, bytenr, flags, chunk_tree_uuid, generation, owner, nritems, level }
    }
}

/// One leaf item slot (key + data offset/size).  The data itself is stored
/// later in the leaf, at `header_end + slots_total_size + data_offset`.
#[derive(Debug, Clone, Copy)]
pub struct LeafItemSlot {
    pub key: Key,
    pub data_offset: u32,
    pub data_size: u32,
}

impl LeafItemSlot {
    pub fn parse(buf: &[u8], pos: usize) -> Self {
        let key = Key::parse(buf, pos);
        let data_offset = u32::from_le_bytes(buf[pos + 17..pos + 21].try_into().unwrap());
        let data_size = u32::from_le_bytes(buf[pos + 21..pos + 25].try_into().unwrap());
        Self { key, data_offset, data_size }
    }
}

/// A parsed leaf node: header + slots, plus the raw node buffer (so callers
/// can slice item data out by absolute offset).
pub struct Leaf {
    pub header: Header,
    pub slots: Vec<LeafItemSlot>,
    /// The full node buffer (length == node_size), used for slicing item data.
    pub buf: Vec<u8>,
}

impl Leaf {
    /// Byte offset within `buf` where item data begins (end of header + slots).
    pub fn data_start(&self) -> usize {
        HEADER_SIZE + self.slots.len() * LEAF_ITEM_SIZE
    }

    /// Slice the payload bytes for slot `i`.
    ///
    /// In btrfs, a leaf item's `data_offset` is measured from the end of
    /// the 101-byte header to the item's data — data grows backward from
    /// the end of the node.  This matches the Python reference parser, which
    /// computes the data position as `header.phys_end + data_offset` where
    /// `phys_end` is the stream position just past the header (= node_start
    /// + 101), so the node-relative position is `101 + data_offset`.
    pub fn item_data(&self, i: usize) -> &[u8] {
        let slot = &self.slots[i];
        let start = HEADER_SIZE + slot.data_offset as usize;
        let end = start + slot.data_size as usize;
        &self.buf[start..end]
    }
}

/// A parsed internal node: header + key pointers.
pub struct InternalNode {
    pub header: Header,
    pub ptrs: Vec<KeyPtr>,
}

/// Either a leaf or an internal node, discriminated by `header.level`.
pub enum Node {
    Leaf(Leaf),
    Internal(InternalNode),
}

impl Node {
    pub fn header(&self) -> &Header {
        match self {
            Node::Leaf(l) => &l.header,
            Node::Internal(i) => &i.header,
        }
    }

    /// Parse a full node buffer of `node_size` bytes.
    pub fn parse(buf: Vec<u8>) -> Self {
        let header = Header::parse(&buf);
        let n = header.nritems as usize;
        let slots_start = HEADER_SIZE;
        if header.level == 0 {
            let mut slots = Vec::with_capacity(n);
            for i in 0..n {
                slots.push(LeafItemSlot::parse(&buf, slots_start + i * LEAF_ITEM_SIZE));
            }
            Node::Leaf(Leaf { header, slots, buf })
        } else {
            let mut ptrs = Vec::with_capacity(n);
            for i in 0..n {
                ptrs.push(KeyPtr::parse(&buf, slots_start + i * KEYPTR_SIZE));
            }
            Node::Internal(InternalNode { header, ptrs })
        }
    }
}