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
    /// fail loudly instead of silently producing false mismatches.  The
    /// algorithm/length come from the superblock's `csum_type`; the real
    /// data `sector_size` (which the old code ignored, hard-wiring 4096) is
    /// taken from the superblock so the scrub honours what the filesystem
    /// actually uses.
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

    /// Verify a btrfs metadata node/leaf header checksum given only the
    /// on-disk `csum_type` — no [`CsumStrategy`] object required.
    ///
    /// This is the single shared primitive behind both
    /// [`CsumStrategy::verify_node_header`] (used for every tree node) and
    /// [`super::superblock::Superblock::read`] (which verifies the
    /// superblock's *own* header before any tree walk begins).  The
    /// chicken-and-egg there is resolved by *ordering*, not by a separate
    /// code path: the caller reads the raw 4096-byte block, peeks
    /// `csum_type` (a plain `le_u16` at offset 196, itself inside the
    /// checksummed body), and calls this — the algorithm id is covered by
    /// the very checksum we are about to check, so a corrupt `csum_type`
    /// simply fails verification rather than causing a circular dependency.
    ///
    /// Returns `Err` for an unsupported `csum_type` (so the caller fails
    /// loudly), `Ok(true)` iff the stored header csum matches the computed
    /// one.  `node_buf` must be exactly `node_size` bytes (the full on-disk
    /// node); the superblock is a 4096-byte node with the identical
    /// csum-prefix layout, so it verifies with the same code.
    pub(crate) fn verify_header(csum_type: u16, node_buf: &[u8]) -> io::Result<bool> {
        const CSUM_PREFIX: usize = 32; // btrfs_header.csum[32]
        if node_buf.len() <= CSUM_PREFIX {
            return Ok(false);
        }
        let (_name, hash_len) = match csum_type {
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
        // Only the first `hash_len` bytes of the 32-byte csum field are the
        // real checksum; the rest is padding.  Comparing the full 32-byte
        // prefix against `compute` (which returns `hash_len` bytes) would
        // always mismatch for crc32c/xxhash (hash_len < 32) and falsely
        // fail every clean node.
        let stored = &node_buf[..hash_len];
        let body = &node_buf[CSUM_PREFIX..];
        let actual = match csum_type {
            csum_type::CRC32C => crc32c::crc32c(body).to_le_bytes().to_vec(),
            csum_type::XXHASH => xxhash_rust::xxh64::xxh64(body, 0).to_le_bytes().to_vec(),
            csum_type::SHA256 => {
                use sha2::{Digest, Sha256};
                let mut h = Sha256::new();
                h.update(body);
                h.finalize().to_vec()
            }
            csum_type::BLAKE2 => {
                use blake2::{Blake2bVar, digest::Update, digest::VariableOutput};
                let mut h = Blake2bVar::new(32).expect("32 is a valid BLAKE2b digest size");
                h.update(body);
                let mut out = [0u8; 32];
                h.finalize_variable(&mut out)
                    .expect("32-byte output is always valid for BLAKE2b");
                out.to_vec()
            }
            _ => unreachable!("csum_type validated above"),
        };
        Ok(stored == actual.as_slice())
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

    /// Verify a btrfs metadata node/leaf header checksum.
    ///
    /// btrfs stores a 32-byte checksum field (`btrfs_header::csum[32]`) at
    /// the start of every tree node, but only the first `hash_len` bytes are
    /// the actual checksum — the remainder of the 32-byte field is padding.
    /// The checksum covers the *rest* of the node — everything after the
    /// 32-byte csum prefix — up to `node_size` bytes.  This is the same
    /// algorithm as the data csum (selected by `csum_type`), just applied to
    /// the metadata block instead of a data sector.
    ///
    /// Returns `true` iff the stored header csum matches the computed one.
    /// `node_buf` must be exactly `node_size` bytes (the full on-disk node).
    ///
    /// Delegates to the shared [`CsumStrategy::verify_header`] primitive so
    /// the superblock (which has no [`CsumStrategy`] yet) and every tree
    /// node verify with one identical code path.
    pub fn verify_node_header(&self, node_buf: &[u8]) -> bool {
        CsumStrategy::verify_header(self.csum_type, node_buf).unwrap_or(false)
    }
}
