//! Checksum handling: maps the superblock's csum_type to a concrete hash
//! (crc32c / xxhash / sha256 / blake2) and verifies on-disk checksums.

use std::io;

use super::superblock::Superblock;

// btrfs on-disk csum_type values (the superblock field at offset 196).
pub mod csum_type {
    pub const CRC32C: u16 = 0;
    pub const XXHASH: u16 = 1;
    pub const SHA256: u16 = 2;
    pub const BLAKE2: u16 = 3;
}

/// Checksum parameters for this filesystem, derived from the superblock's
/// csum_type and sector size.
#[derive(Debug, Clone, Copy)]
pub struct CsumStrategy {
    pub csum_type: u16,
    pub name: &'static str,
    /// Bytes per stored checksum (4 crc32c, 8 xxhash, 32 sha256/blake2).
    pub hash_len: usize,
    /// Logical sector size; EXTENT_CSUM items hold one checksum per sector.
    pub sector_size: u64,
}

impl CsumStrategy {
    /// Selects the checksum strategy from the superblock's csum_type.
    /// Returns InvalidData for an unsupported csum_type or a zero sector size.
    pub fn from_superblock(sb: &Superblock) -> io::Result<Self> {
        let (name, hash_len) = match sb.csum_type {
            csum_type::CRC32C => ("crc32c", 4usize),
            csum_type::XXHASH => ("xxhash", 8usize),
            csum_type::SHA256 => ("sha256", 32usize),
            csum_type::BLAKE2 => ("blake2", 32usize),
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "unsupported btrfs csum_type {other} (only crc32c/xxhash/sha256/blake2 are implemented)"
                    ),
                ));
            }
        };

        // Checksums are stored one per sector; a zero sector size would break
        // the item-span math downstream.
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

    // Verifies a node's or the superblock's 32-byte header checksum: the hash
    // stored in the first hash_len bytes of the checksum field must equal the
    // hash of everything after the 32-byte prefix.
    pub(crate) fn verify_header(csum_type: u16, node_buf: &[u8]) -> io::Result<bool> {
        const CSUM_PREFIX: usize = 32; // 32-byte header checksum field
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
                    format!(
                        "unsupported btrfs csum_type {other} (only crc32c/xxhash/sha256/blake2 are implemented)"
                    ),
                ));
            }
        };

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

    /// Computes the checksum of data, returned as the byte sequence stored on
    /// disk (numeric hashes are little-endian).
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
                let mut h = Blake2bVar::new(32).expect("32 is a valid BLAKE2b digest size");
                h.update(data);
                let mut out = [0u8; 32];
                h.finalize_variable(&mut out)
                    .expect("32-byte output is always valid for BLAKE2b");
                out.to_vec()
            }

            _ => unreachable!("csum_type validated at construction"),
        }
    }

    /// True when data's checksum equals the stored on-disk checksum bytes;
    /// any length mismatch counts as no match.
    pub fn matches(&self, stored: &[u8], data: &[u8]) -> bool {
        stored == self.compute(data).as_slice()
    }

    /// Verifies the 32-byte header checksum of a node buffer; a short buffer
    /// or an unsupported checksum type is treated as a mismatch.
    pub fn verify_node_header(&self, node_buf: &[u8]) -> bool {
        CsumStrategy::verify_header(self.csum_type, node_buf).unwrap_or(false)
    }
}
