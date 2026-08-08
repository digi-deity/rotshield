//! Node/leaf parsing: the 101-byte on-disk header, leaf item slots, and
//! internal-node child pointers.

use super::key::{Key, KeyPtr};
// On-disk node/leaf header: 32-byte checksum, then fsid, bytenr, flags,
// chunk_tree_uuid, generation, owner, nritems, level.
pub const HEADER_SIZE: usize = 101;

// Leaf item slot: 17-byte key + 4-byte data_offset + 4-byte data_size.
pub const LEAF_ITEM_SIZE: usize = 25;

// Internal-node child pointer: 17-byte key + 8-byte blockptr + 8-byte
// generation.
pub const KEYPTR_SIZE: usize = 33;

/// On-disk node/leaf header (101 bytes). The 32-byte checksum at offset 0 is
/// verified separately by the csum strategy, so parsing starts at byte 32.
#[derive(Debug, Clone, Copy)]
pub struct Header {
    pub fsid: [u8; 16],
    /// The node's own logical address; checked against the read position.
    pub bytenr: u64,
    pub flags: u64,
    pub chunk_tree_uuid: [u8; 16],
    pub generation: u64,
    pub owner: u64,
    pub nritems: u32,
    /// 0 = leaf; higher values are internal-node levels.
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
        Self {
            fsid,
            bytenr,
            flags,
            chunk_tree_uuid,
            generation,
            owner,
            nritems,
            level,
        }
    }
}

/// One slot in a leaf's item array: the item's key and where its data lives
/// in the leaf.
#[derive(Debug, Clone, Copy)]
pub struct LeafItemSlot {
    pub key: Key,
    /// Offset of the item data relative to the end of the header (the data
    /// area starts at HEADER_SIZE).
    pub data_offset: u32,
    pub data_size: u32,
}

impl LeafItemSlot {
    pub fn parse(buf: &[u8], pos: usize) -> Self {
        let key = Key::parse(buf, pos);
        let data_offset = u32::from_le_bytes(buf[pos + 17..pos + 21].try_into().unwrap());
        let data_size = u32::from_le_bytes(buf[pos + 21..pos + 25].try_into().unwrap());
        Self {
            key,
            data_offset,
            data_size,
        }
    }
}

/// A parsed leaf: its header, item slots, and the raw node bytes so items
/// can be sliced out of the buffer.
pub struct Leaf {
    pub header: Header,
    pub slots: Vec<LeafItemSlot>,
    /// Raw leaf bytes (header + slots + item data).
    pub buf: Vec<u8>,
}

impl Leaf {
    // First byte after the item-slot array; the leaf's item data lives at or
    // after this point.
    pub fn data_start(&self) -> usize {
        HEADER_SIZE + self.slots.len() * LEAF_ITEM_SIZE
    }

    // slot.data_offset is relative to the end of the header, so the item
    // starts at HEADER_SIZE + data_offset.
    pub fn item_data(&self, i: usize) -> &[u8] {
        let slot = &self.slots[i];
        let start = HEADER_SIZE + slot.data_offset as usize;
        let end = start + slot.data_size as usize;
        &self.buf[start..end]
    }
}

pub struct InternalNode {
    pub header: Header,
    pub ptrs: Vec<KeyPtr>,
}

/// A parsed node: a leaf at level 0, an internal node at higher levels.
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

    // Level 0 nodes are leaves with item slots; higher levels are internal
    // nodes whose slots are child key pointers.
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
