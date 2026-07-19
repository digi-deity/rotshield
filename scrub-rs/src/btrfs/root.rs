//! Minimal ROOT_ITEM parsing — just the two fields the scrub needs.
//!
//! ROOT_ITEM layout (subset, after the embedded InodeItem):
//!   +0    inode: InodeItem (160 bytes)
//!   +160  generation u64
//!   +168  root_dirid u64
//!   +176  bytenr u64        <-- tree root logical address
//!   +184  byte_limit u64
//!   +192  bytes_used u64
//!   +200  last_snapshot u64
//!   +208  flags u64
//!   +216  refs u32
//!   +220  drop_progress: Key (17 bytes)
//!   +237  drop_level u8
//!   +238  level u8          <-- tree height (0 = single leaf)

use super::util::le_u64;

#[derive(Debug, Clone, Copy)]
pub struct RootItem {
    pub bytenr: u64,
    pub level: u8,
}

impl RootItem {
    pub fn parse(buf: &[u8]) -> Self {
        let bytenr = le_u64(buf, 176);
        let level = buf[238];
        Self { bytenr, level }
    }
}
