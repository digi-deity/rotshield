//! Checksum strategy — the algorithm + sector size the scrub uses to verify
//! data, derived from the on-disk superblock rather than hard-coded.
//!
//! btrfs stores the checksum algorithm in `Superblock::csum_type` and the
//! data sector size in `Superblock::sector_size`.  The scrub previously
//! hard-wired CRC32C over fixed 4096-byte sectors, so any filesystem
//! created with `mkfs.btrfs -C xxhash|sha256|blake2` (or a non-4K
//! `--sectorsize`) was scrubbed with the **wrong algorithm / granularity**
//! and every sector mismatched — a false-positive flood.  This module
//! makes the checksum a *strategy* selected from the superblock so the
//! scrub honours what the filesystem actually uses.
//!
//! The four btrfs csum profiles (from `btrfs_super_block::csum_type`):
//!   - `0` CRC32C   — 32-bit, little-endian
//!   - `1` XXHASH  — 64-bit, little-endian
//!   - `2` SHA256  — 256-bit
//!   - `3` BLAKE2  — 256-bit
//!
//! The stored csum is carried as raw `Vec<u8>` (length == `hash_len`) so
//! SHA256/BLAKE2 (32 bytes) and XXHASH (8 bytes) all fit; comparison is a
//! byte-for-byte equality against `compute(data)`.

use std::io;

use super::superblock::Superblock;

/// btrfs on-disk csum type identifiers (`superblock.csum_type`).
pub mod csum_type {
    pub const CRC32C: u16 = 0;
    pub const XXHASH: u16 = 1;
    pub const SHA256: u16 = 2;
    pub const BLAKE2: u16 = 3;
}

/// A selected checksum strategy: how to hash a data sector and how big a
/// sector is.
#[derive(Debug, Clone, Copy)]
pub struct CsumStrategy {
    /// On-disk csum type id (mirrors `superblock.csum_type`).
    pub csum_type: u16,
    /// Human-readable algorithm name (for logs / diagnostics).
    pub name: &'static str,
    /// Length in bytes of one stored checksum.
    pub hash_len: usize,
    /// Data sector size in bytes — the checksum granularity.
    pub sector_size: u64,
}

impl CsumStrategy {
    /// Build the strategy from a parsed superblock.
    ///
    /// Returns an error for csum types this tool does not implement, so we
    /// fail loudly instead of silently producing false mismatches.
    pub fn from_superblock(sb: &Superblock) -> io::Result<Self> {
        let (name, hash_len) = match sb.csum_type {
            csum_type::CRC32C => ("crc32c", 4usize),
            csum_type::XXHASH => ("xxhash", 8usize),
            csum_type::SHA256 => ("sha256", 32usize),
            csum_type::BLAKE2 => ("blake2", 32usize),
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unsupported btrfs csum_type {other} (only crc32c/xxhash/sha256/blake2 are implemented)"),
                ));
            }
        };
        // btrfs data sector sizes are 4K..64K; the superblock exposes the
        // real value (the old code ignored it and used a fixed 4096).
        let sector_size = sb.sector_size as u64;
        if sector_size == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "superblock reports sector_size == 0",
            ));
        }
        Ok(Self {
            csum_type: sb.csum_type,
            name,
            hash_len,
            sector_size,
        })
    }

    /// Compute the checksum of `data` under this strategy, returning the raw
    /// bytes (length == `hash_len`).
    pub fn compute(&self, data: &[u8]) -> Vec<u8> {
        match self.csum_type {
            csum_type::CRC32C => {
                let v = crc32c::crc32c(data);
                v.to_le_bytes().to_vec()
            }
            csum_type::XXHASH => {
                let v = xxhash_rust::xxh64::xxh64(data, 0);
                v.to_le_bytes().to_vec()
            }
            csum_type::SHA256 => {
                use sha2::{Digest, Sha256};
                let mut h = Sha256::new();
                h.update(data);
                h.finalize().to_vec()
            }
            csum_type::BLAKE2 => {
                use blake2::{Blake2bVar, digest::Update, digest::VariableOutput};
                // btrfs uses BLAKE2b with a 32-byte (256-bit) digest.
                let mut h = Blake2bVar::new(32)
                    .expect("32 is a valid BLAKE2b digest size");
                h.update(data);
                let mut out = [0u8; 32];
                h.finalize_variable(&mut out)
                    .expect("32-byte output is always valid for BLAKE2b");
                out.to_vec()
            }
            // Unreachable: from_superblock rejects unknown types.
            _ => unreachable!("csum_type validated at construction"),
        }
    }

    /// Compare a stored checksum against the freshly computed one.
    pub fn matches(&self, stored: &[u8], data: &[u8]) -> bool {
        stored == self.compute(data).as_slice()
    }
}
