//! NonRAID / Unraid array integration.
//!
//! This is the array-side counterpart to `btrfs/`.  Where a filesystem
//! scrub reports checksum mismatches with their on-disk physical
//! location `(devid, array_phys)`, `array/` knows how to talk to a
//! NonRAID/Unraid parity array: it parses `/proc/nmdstat` (or
//! `/proc/mdstat`), maps a `(devid, array_phys)` pair to a raw rdev
//! path + raw offset, and uses parity XOR to recover corrupt data
//! blocks.
//!
//! The module does not depend on `btrfs/`; any filesystem scrub that can
//! produce `(devid, array_phys, block_size, expected_csum)` can drive
//! recovery through it.  The block size is passed per-call rather than
//! hardcoded so a future ZFS integration (which uses different checksums
//! and record sizes) can reuse the same recovery path.
//!
//! # Address spaces and I/O paths
//!
//! A byte location travels through three address spaces on its way from
//! a filesystem key to a real byte on a spinning disk.  Each space is
//! also a distinct I/O path with different parity semantics:
//!
//! ```text
//!   logical space  ──chunk tree──▶  array-partition space  ──rdevOffset──▶  raw-rdev space
//!   (btrfs-internal)                  (/dev/nmd1p1)                          (/dev/loop2)
//! ```
//!
//! **Logical space** — e.g. `0x13e0d000`.
//! The filesystem's own address space.  B-tree keys, CSUM tree keys, and
//! `disk_bytenr` in file extents all live here.  btrfs pretends it has
//! one contiguous space; the chunk tree maps each logical address to a
//! physical stripe on a device.  No I/O happens in this space — it's
//! pure addressing.
//!
//! **Array-partition space** — e.g. `0xcd1d000`.
//! The byte offset within the array-partition device (`/dev/nmd1p1`).
//! From the filesystem's perspective this *is* physical — it's what the
//! chunk tree's stripe offsets produce.  But `/dev/nmd1p1` is a virtual
//! device stacked on the NonRAID array driver, which stacks on a raw
//! rdev.
//! - **Reads** go through the array driver.  For a present, healthy
//!   disk this is transparent and corruption is visible.  For a
//!   *missing* disk the driver transparently reconstructs from parity,
//!   **masking corruption** — the scrub sees good data and cannot
//!   detect it.  This is acceptable for our use case (we scrub disks
//!   that are present but corrupt) but is a known limitation.
//! - **Writes** go through the array driver, which writes the data
//!   **and recomputes parity P/Q** to match.  Recovery must *not* write
//!   here — see below.
//!
//! **Raw-rdev space** — e.g. `0xcd25000`.
//! The byte offset on the actual underlying block device (`/dev/loop2`).
//! This is `array-partition offset + rdevOffset` (the per-disk header,
//! typically 32 KiB).
//! - **Reads** are direct disk access — they show the actual bytes on
//!   disk, including corruption.
//! - **Writes** are direct disk access — **parity is not touched**.  The
//!   array driver is bypassed entirely, so parity stays consistent with
//!   whatever was there *before* the write.
//!
//! # Why recovery writes to raw-rdev space
//!
//! The recovery write-back (see [`stripe::write_block`]) reads and writes
//! exclusively in raw-rdev space.  Writing the recovered data through the
//! array partition would make the
//! array driver recompute parity from the new data — making parity
//! consistent with the (possibly wrong) recovered data and destroying
//! the original parity relationship.  If the recovery turned out wrong
//! (bad checksum, wrong sector), you could never re-recover.
//!
//! Writing to the raw rdev leaves parity holding the *original*
//! relationship.  A subsequent parity check will flag the inconsistency
//! (data changed, parity didn't), giving a second chance to detect and
//! fix a botched recovery.  This mirrors the Python `recover.py`, which
//! opens the raw rdev directly for the same reason.
//!
//! # What lives here
//!
//! `array/` parses `/proc/nmdstat` into [`config::ArrayConfig`],
//! translates `(slot, array_phys)` to a raw-rdev path + offset via
//! [`resolve::resolve`], and gathers the aligned chunks of one stripe
//! (data disks + parity, with zero-substitution past a smaller disk's
//! end) via [`stripe::gather_stripe`].  It does not depend on `btrfs/`;
//! the only `recovery/` dependency is [`crate::recovery::gf`] (the shared
//! GF(2^8) arithmetic tables), used by [`parity`] to compute live P/Q
//! syndromes for `bin/craft_corrupt` — there is no `array::gf` shim, so
//! any caller that needs the tables imports [`crate::recovery::gf`]
//! directly.  The startup array-soundness canary used to live here; it is
//! now [`crate::canary`], a top-level glue module that composes `array/`
//! + `recovery/` + a filesystem magic probe.
//!
//! Writing to the raw rdev leaves parity holding the *original*
//! relationship.  A subsequent parity check will flag the inconsistency
//! (data changed, parity didn't), giving a second chance to detect and
//! fix a botched recovery.  This mirrors the Python `recover.py`, which
//! opens the raw rdev directly for the same reason.

pub mod config;
pub mod parity;
pub mod resolve;
pub mod stripe;
