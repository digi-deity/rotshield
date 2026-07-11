//! btrfs CHUNK_ITEM parsing and the logical→physical chunk cache.
//!
//! Mirrors `btrfs_recon/util/chunk_cache.py` at the level needed for scrub:
//! given a logical byte address, return the (devid, physical) of the first
//! stripe that holds it.  Striping (RAID0/RAID10/RAID5/RAID6) is not yet
//! implemented — only single-stripe and mirrored (DUP/RAID1/RAID1C3/RAID1C4)
//! profiles, which is all the simple btrfs layouts this tool targets.

use super::key::bg_flag;
use super::util::le_u64;

/// One stripe of a chunk: (devid, physical offset on that dev).
#[derive(Debug, Clone, Copy)]
pub struct Stripe {
    pub devid: u64,
    pub offset: u64,
}

/// Parsed CHUNK_ITEM payload (subset of fields).
#[derive(Debug, Clone)]
pub struct ChunkItem {
    pub length: u64,
    pub stripe_len: u64,
    pub ty: u64,
    pub num_stripes: u16,
    pub stripes: Vec<Stripe>,
}

impl ChunkItem {
    /// Parse a CHUNK_ITEM payload (the bytes after a leaf slot's key).
    pub fn parse(buf: &[u8]) -> Self {
        let length = le_u64(buf, 0);
        let _owner = le_u64(buf, 8);
        let stripe_len = le_u64(buf, 16);
        let ty = le_u64(buf, 24);
        // io_align u32 @32, io_width u32 @36, sector_size u32 @40
        let num_stripes = u16::from_le_bytes([buf[44], buf[45]]);
        let _sub_stripes = u16::from_le_bytes([buf[46], buf[47]]);
        // stripes follow at offset 48, each 32 bytes (devid u64, offset u64, uuid[16]).
        let mut stripes = Vec::with_capacity(num_stripes as usize);
        for i in 0..num_stripes as usize {
            let base = 48 + i * 32;
            let devid = le_u64(buf, base);
            let offset = le_u64(buf, base + 8);
            // skip the 16-byte dev_uuid
            stripes.push(Stripe { devid, offset });
        }
        Self { length, stripe_len, ty, num_stripes, stripes }
    }
}

/// A parsed (Key, ChunkItem) pair, as it appears in the chunk tree or in the
/// superblock's system-chunk bootstrap array.
#[derive(Debug, Clone)]
pub struct ChunkRecord {
    pub logical: u64,
    pub chunk: ChunkItem,
}

/// Parse a sequence of (Key, CHUNK_ITEM) records out of a raw byte buffer —
/// used for the superblock's system-chunk array.
pub fn parse_sys_chunks(buf: &[u8]) -> Vec<ChunkRecord> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos + 17 + 48 <= buf.len() {
        let key = super::key::Key::parse(buf, pos);
        pos += 17;
        if key.ty != super::key::key_type::CHUNK_ITEM {
            // The sys chunk array should only contain CHUNK_ITEMs; bail if not.
            break;
        }
        // We need to compute the chunk item size to advance past it.
        let num_stripes = u16::from_le_bytes([buf[pos + 44], buf[pos + 45]]) as usize;
        let chunk_len = 48 + num_stripes * 32;
        if pos + chunk_len > buf.len() {
            break;
        }
        let chunk = ChunkItem::parse(&buf[pos..pos + chunk_len]);
        out.push(ChunkRecord { logical: key.offset, chunk });
        pos += chunk_len;
    }
    out
}

/// A logical→physical mapping cache.  Stores chunks as a sorted vec for
/// binary-search lookup by logical address.
///
/// Immutable after the chunk-tree walk.  Held by reference (`&ChunkMap`) and
/// shared between the reader, the scrub loop, and the recovery callback —
/// no cloning needed.
#[derive(Debug, Default)]
pub struct ChunkMap {
    entries: Vec<MapEntry>,
}

#[derive(Debug, Clone)]
struct MapEntry {
    begin: u64,
    end: u64,
    stripe_len: u64,
    mirrored: bool,
    stripes: Vec<Stripe>,
}

impl ChunkMap {
    pub fn insert(&mut self, rec: &ChunkRecord) {
        let end = rec.logical + rec.chunk.length;
        let mirrored = (rec.chunk.ty & bg_flag::MIRROR_MASK) != 0;
        self.entries.push(MapEntry {
            begin: rec.logical,
            end,
            stripe_len: rec.chunk.stripe_len,
            mirrored,
            stripes: rec.chunk.stripes.clone(),
        });
        // Keep sorted by begin for binary search.
        self.entries.sort_by_key(|e| e.begin);
    }

    /// Resolve `logical` to a (devid, physical) on the first available stripe.
    ///
    /// For mirrored profiles every stripe is a full copy; we return the first.
    /// For single-stripe chunks the math is the same.  Striped profiles
    /// (RAID0/RAID10/RAID5/RAID6) are not supported and will return the first
    /// stripe as a fallback (which is wrong for them — but we don't target
    /// those here).
    pub fn lookup(&self, logical: u64) -> Option<(u64, u64)> {
        let idx = self.entries.binary_search_by(|e| {
            if logical < e.begin {
                std::cmp::Ordering::Greater
            } else if logical >= e.end {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        }).ok()?;
        let e = &self.entries[idx];
        let log_offset = logical - e.begin;
        let s = &e.stripes[0];
        Some((s.devid, s.offset + log_offset))
    }

    /// Resolve `logical` to *all* stripes of the chunk it falls in, as a
    /// vector of `(devid, physical)` pairs.  For mirrored profiles (DUP /
    /// RAID1 / RAID1C3 / RAID1C4) every entry is a full copy of the same
    /// logical range — this is what lets the scrub cross-check the copies
    /// (e.g. prefer the good DUP metadata copy when one header is corrupt).
    /// For single-stripe chunks the vector has exactly one entry (identical
    /// to [`ChunkMap::lookup`]'s result).  Returns `None` if `logical` is
    /// not covered by any chunk.
    pub fn lookup_stripes(&self, logical: u64) -> Option<Vec<(u64, u64)>> {
        let idx = self.entries.binary_search_by(|e| {
            if logical < e.begin {
                std::cmp::Ordering::Greater
            } else if logical >= e.end {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        }).ok()?;
        let e = &self.entries[idx];
        let log_offset = logical - e.begin;
        Some(
            e.stripes
                .iter()
                .map(|s| (s.devid, s.offset + log_offset))
                .collect(),
        )
    }

    pub fn len(&self) -> usize { self.entries.len() }
    pub fn is_empty(&self) -> bool { self.entries.is_empty() }

    /// Debug helper: list every chunk, mirroring the Python output.
    pub fn dump(&self) {
        for e in &self.entries {
            let stripes: Vec<String> = e.stripes.iter()
                .map(|s| format!("({}, 0x{:x})", s.devid, s.offset))
                .collect();
            println!(
                "  logical 0x{:x}..0x{:x} stripe_len={} mirrored={} stripes=[{}]",
                e.begin, e.end, e.stripe_len, e.mirrored, stripes.join(", ")
            );
        }
    }
}