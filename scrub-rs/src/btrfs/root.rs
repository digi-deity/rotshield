//! ROOT_ITEM parsing: resolves each tree's root node from the root tree.

use super::util::le_u64;

/// A btrfs ROOT_ITEM pared down to the field the scrub needs: the tree
/// root's logical address.
#[derive(Debug, Clone, Copy)]
pub struct RootItem {
    pub bytenr: u64,
    pub level: u8,
}

impl RootItem {
    // Offsets into the on-disk ROOT_ITEM: bytenr at 176, level at 238; the
    // rest of the item is ignored.
    pub fn parse(buf: &[u8]) -> Self {
        let bytenr = le_u64(buf, 176);
        let level = buf[238];
        Self { bytenr, level }
    }
}
