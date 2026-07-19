//! scrub-rs library crate — shared modules for the scrub binary and
//! utility binaries (corruption crafting, etc.).
//!
//! The two top-level modules are kept filesystem-agnostic at the boundary:
//! `btrfs/` knows nothing about parity arrays, and `array/` knows nothing
//! about btrfs.  Utility binaries can pull in either side independently.

pub mod array;
pub mod batch_recover;
pub mod btrfs;
pub mod freeze;
pub mod fs;
pub mod recovery;

pub use btrfs::csum_strategy::CsumStrategy;
