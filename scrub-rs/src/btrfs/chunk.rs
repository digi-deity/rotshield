//! Chunk mapping: parse chunk items and translate logical addresses to physical stripes.

use super::key::bg_flag;
use super::util::le_u64;

/// One stripe of a chunk: device id and physical offset within it.
#[derive(Debug, Clone, Copy)]
pub struct Stripe {
    pub devid: u64,
    pub offset: u64,
}

/// Parsed btrfs chunk item: the logical span it covers and the stripes backing it.
#[derive(Debug, Clone)]
pub struct ChunkItem {
    pub length: u64,
    pub stripe_len: u64,
    pub ty: u64,
    pub num_stripes: u16,
    pub stripes: Vec<Stripe>,
}

impl ChunkItem {
    /// Parse an on-disk chunk item: a 48-byte header followed by one 32-byte
    /// stripe entry per `num_stripes`.
    pub fn parse(buf: &[u8]) -> Self {
        let length = le_u64(buf, 0);
        let _owner = le_u64(buf, 8);
        let stripe_len = le_u64(buf, 16);
        let ty = le_u64(buf, 24);

        let num_stripes = u16::from_le_bytes([buf[44], buf[45]]);
        let _sub_stripes = u16::from_le_bytes([buf[46], buf[47]]);

        // Stripe entries start at byte 48; only devid and offset are used.
        let mut stripes = Vec::with_capacity(num_stripes as usize);
        for i in 0..num_stripes as usize {
            let base = 48 + i * 32;
            let devid = le_u64(buf, base);
            let offset = le_u64(buf, base + 8);

            stripes.push(Stripe { devid, offset });
        }
        Self {
            length,
            stripe_len,
            ty,
            num_stripes,
            stripes,
        }
    }
}

/// A chunk item together with the logical address it starts at.
#[derive(Debug, Clone)]
pub struct ChunkRecord {
    pub logical: u64,
    pub chunk: ChunkItem,
}

const PROFILE_MASK: u64 = bg_flag::RAID0
    | bg_flag::RAID1
    | bg_flag::DUP
    | bg_flag::RAID10
    | bg_flag::RAID5
    | bg_flag::RAID6
    | bg_flag::RAID1C3
    | bg_flag::RAID1C4;

/// Human-readable name of a chunk's RAID profile (SINGLE when unset).
pub fn profile_name(ty: u64) -> &'static str {
    match ty & PROFILE_MASK {
        0 => "SINGLE",
        p if p == bg_flag::RAID0 => "RAID0",
        p if p == bg_flag::RAID1 => "RAID1",
        p if p == bg_flag::DUP => "DUP",
        p if p == bg_flag::RAID10 => "RAID10",
        p if p == bg_flag::RAID5 => "RAID5",
        p if p == bg_flag::RAID6 => "RAID6",
        p if p == bg_flag::RAID1C3 => "RAID1C3",
        p if p == bg_flag::RAID1C4 => "RAID1C4",
        _ => "UNKNOWN",
    }
}

/// Parse the superblock's system chunk array: alternating 17-byte keys and
/// chunk items, ending at the first non-CHUNK_ITEM key or the buffer end.
pub fn parse_sys_chunks(buf: &[u8]) -> Vec<ChunkRecord> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos + 17 + 48 <= buf.len() {
        let key = super::key::Key::parse(buf, pos);
        pos += 17;
        if key.ty != super::key::key_type::CHUNK_ITEM {
            break;
        }

        let num_stripes = u16::from_le_bytes([buf[pos + 44], buf[pos + 45]]) as usize;
        let chunk_len = 48 + num_stripes * 32;
        if pos + chunk_len > buf.len() {
            break;
        }
        let chunk = ChunkItem::parse(&buf[pos..pos + chunk_len]);
        out.push(ChunkRecord {
            logical: key.offset,
            chunk,
        });
        pos += chunk_len;
    }
    out
}

/// Sorted chunk ranges for logical-to-physical address translation.
#[derive(Debug, Default, Clone)]
pub struct ChunkMap {
    entries: Vec<MapEntry>,
}

#[derive(Debug, Clone)]
struct MapEntry {
    begin: u64,
    end: u64,

    ty: u64,
    stripes: Vec<Stripe>,
}

/// Length and profile flags of the chunk containing a given offset.
#[derive(Debug, Clone, Copy)]
pub struct ChunkInfo {
    pub length: u64,
    pub flags: u64,
}

impl ChunkMap {
    /// Add a chunk, keeping entries sorted by logical start for binary search.
    pub fn insert(&mut self, rec: &ChunkRecord) {
        let end = rec.logical + rec.chunk.length;
        self.entries.push(MapEntry {
            begin: rec.logical,
            end,
            ty: rec.chunk.ty,
            stripes: rec.chunk.stripes.clone(),
        });

        self.entries.sort_by_key(|e| e.begin);
    }

    /// Flags and length of the chunk containing `chunk_offset`.
    pub fn info(&self, chunk_offset: u64) -> Option<ChunkInfo> {
        let idx = self
            .entries
            .binary_search_by(|e| {
                if chunk_offset < e.begin {
                    std::cmp::Ordering::Greater
                } else if chunk_offset >= e.end {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .ok()?;
        let e = &self.entries[idx];
        Some(ChunkInfo {
            length: e.end - e.begin,
            flags: e.ty,
        })
    }

    /// Map a logical address to (devid, physical offset) on its first stripe.
    /// Valid for SINGLE/DUP chunks, where every copy sits at the same offset.
    pub fn lookup(&self, logical: u64) -> Option<(u64, u64)> {
        let idx = self
            .entries
            .binary_search_by(|e| {
                if logical < e.begin {
                    std::cmp::Ordering::Greater
                } else if logical >= e.end {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .ok()?;
        let e = &self.entries[idx];
        let log_offset = logical - e.begin;
        let s = &e.stripes[0];
        Some((s.devid, s.offset + log_offset))
    }

    /// (devid, physical offset) for every stripe copy of `logical`.
    pub fn lookup_stripes(&self, logical: u64) -> Option<Vec<(u64, u64)>> {
        let idx = self
            .entries
            .binary_search_by(|e| {
                if logical < e.begin {
                    std::cmp::Ordering::Greater
                } else if logical >= e.end {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .ok()?;
        let e = &self.entries[idx];
        let log_offset = logical - e.begin;
        Some(
            e.stripes
                .iter()
                .map(|s| (s.devid, s.offset + log_offset))
                .collect(),
        )
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Reject data chunks with striped RAID profiles — the scrub maps
    /// logical-to-physical linearly and cannot handle interleaved stripes.
    pub fn validate_data_profiles(&self) -> std::io::Result<()> {
        for e in &self.entries {
            if e.ty & bg_flag::DATA == 0 {
                continue;
            }
            let profile = e.ty & PROFILE_MASK;

            if profile != 0 && profile != bg_flag::DUP {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "data chunk at logical 0x{:x} uses profile {} — only SINGLE and DUP \
                         data chunks are supported (striped RAID profiles cannot be mapped \
                         linearly by this tool)",
                        e.begin,
                        profile_name(profile)
                    ),
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(logical: u64, ty: u64) -> ChunkRecord {
        ChunkRecord {
            logical,
            chunk: ChunkItem {
                length: 1 << 20,
                stripe_len: 64 * 1024,
                ty,
                num_stripes: 1,
                stripes: vec![Stripe {
                    devid: 1,
                    offset: 0,
                }],
            },
        }
    }

    #[test]
    fn data_single_and_dup_are_accepted() {
        let mut m = ChunkMap::default();
        m.insert(&rec(0x1000_0000, bg_flag::DATA));
        m.insert(&rec(0x2000_0000, bg_flag::DATA | bg_flag::DUP));
        assert!(m.validate_data_profiles().is_ok());
    }

    #[test]
    fn striped_data_chunk_is_rejected() {
        for profile in [
            bg_flag::RAID0,
            bg_flag::RAID1,
            bg_flag::RAID10,
            bg_flag::RAID5,
            bg_flag::RAID6,
            bg_flag::RAID1C3,
            bg_flag::RAID1C4,
        ] {
            let mut m = ChunkMap::default();
            m.insert(&rec(0x1000_0000, bg_flag::DATA | profile));
            let err = m.validate_data_profiles().unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains("logical 0x1000000"), "{msg}");
            assert!(msg.contains("RAID"), "{msg}");
        }
    }

    #[test]
    fn metadata_raid1_is_not_rejected() {
        let mut m = ChunkMap::default();
        m.insert(&rec(0x1000_0000, bg_flag::METADATA | bg_flag::RAID1));
        m.insert(&rec(0x2000_0000, bg_flag::DATA));
        assert!(m.validate_data_profiles().is_ok());
    }
}
