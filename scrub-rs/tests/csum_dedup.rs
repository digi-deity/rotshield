//! Integration test: `LazyCsumProvider`'s metadata-failure accounting is
//! **lazy** (no tree I/O at construction) and **deduplicated** (a node that
//! fails verification is counted exactly once per run, even when several
//! `range()` walks re-read it).
//!
//! This pins the semantics that replaced the old open-time header-only
//! CSUM_TREE sweep (which read the entire tree — ~12–100 GB on a multi-TB
//! disk — before the scrub could start, just to count bad leaves once).
//! Under the new design the failure is counted as a side effect of the
//! per-range walks that actually need the node.
//!
//! Requires `mkfs.btrfs` on PATH (btrfs-progs); the test skips gracefully
//! when it is absent.  No mount is needed: a freshly-formatted image file
//! has a valid chunk/root/csum tree, and the CSUM_TREE root block is
//! corrupted in place (both mirrors, if any) to make `read_node`'s header
//! verification fail.

use std::io::{Read, Seek, SeekFrom, Write};
use std::process::Command;

use scrub_rs::btrfs::csum::LazyCsumProvider;

fn mkfs_available() -> bool {
    Command::new("mkfs.btrfs")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn csum_failure_counted_once_across_overlapping_ranges() {
    if !mkfs_available() {
        eprintln!("skipping: mkfs.btrfs not available");
        return;
    }

    let dir = std::env::temp_dir().join(format!("scrub_rs_csum_dedup_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("fs.img");
    let img_str = img.to_str().unwrap().to_string();

    // Sparse 64 MiB image, single metadata profile (no mirror cross-check
    // to complicate the header-failure path).
    {
        let f = std::fs::File::create(&img).unwrap();
        f.set_len(64 << 20).unwrap();
    }
    let status = Command::new("mkfs.btrfs")
        .args(["-f", "-q", "-n", "16k", "-d", "single", "-m", "single"])
        .arg(&img_str)
        .status()
        .expect("run mkfs.btrfs");
    assert!(status.success(), "mkfs.btrfs failed on {img_str}");

    // Open through the crate's own machinery to learn the CSUM_TREE root.
    let ctx = scrub_rs::btrfs::open(&img_str, 0).expect("open freshly-formatted image");
    let csum_root = ctx.roots.csum_root;
    let total_bytes = ctx.superblock.total_bytes;
    assert!(csum_root != 0, "freshly-formatted fs must have a csum root");

    // Corrupt the CSUM_TREE root block in place: flip the first byte of
    // every physical copy (single profile -> exactly one) so read_node's
    // header-checksum verification fails with all_mirrors_failed.
    let stripes = ctx
        .chunk_map
        .lookup_stripes(csum_root)
        .expect("csum root must resolve to physical stripes");
    {
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&img)
            .unwrap();
        for (_devid, phys) in stripes {
            let mut b = [0u8; 1];
            f.seek(SeekFrom::Start(phys)).unwrap();
            f.read_exact(&mut b).unwrap();
            f.seek(SeekFrom::Start(phys)).unwrap();
            f.write_all(&[b[0] ^ 0xFF]).unwrap();
        }
    }

    // Build the provider the same way BtrfsScrub does (dup'd fd + chunk
    // map clone).  No work may happen at construction anymore.
    let lazy_file = ctx.reader.reopen().expect("dup reader fd");
    let mut provider = LazyCsumProvider::new(
        lazy_file,
        ctx.superblock.node_size as usize,
        ctx.reader.base_offset(),
        ctx.strategy,
        ctx.superblock.devid,
        ctx.superblock.fsid,
        ctx.chunk_map.clone(),
        csum_root,
    );
    assert_eq!(
        provider.metadata_errors(),
        0,
        "construction must be lazy: no tree I/O, no counting before any range() call"
    );

    // Two overlapping range walks over the whole fs.  Both descend through
    // (and fail to verify) the corrupt csum root; the node must be counted
    // exactly once.
    let span = total_bytes.min(16 << 20);
    provider.range(0, span, None, |_| {});
    assert_eq!(
        provider.metadata_errors(),
        1,
        "first walk counts the bad node once"
    );
    provider.range(0, span, None, |_| {});
    assert_eq!(
        provider.metadata_errors(),
        1,
        "second overlapping walk must NOT re-count the same node (dedup by bytenr)"
    );

    // The other two failure classes stay untouched on this image.
    assert_eq!(provider.mirror_mismatches(), 0);
    assert_eq!(provider.metadata_read_errors(), 0);

    // A *healthy* control: rebuild the image untouched and verify a clean
    // walk counts nothing.
    let img_ok = dir.join("fs_ok.img");
    {
        let f = std::fs::File::create(&img_ok).unwrap();
        f.set_len(64 << 20).unwrap();
    }
    let ok_str = img_ok.to_str().unwrap().to_string();
    let status = Command::new("mkfs.btrfs")
        .args(["-f", "-q", "-n", "16k", "-d", "single", "-m", "single"])
        .arg(&ok_str)
        .status()
        .unwrap();
    assert!(status.success());
    let ctx_ok = scrub_rs::btrfs::open(&ok_str, 0).unwrap();
    let lazy_ok = ctx_ok.reader.reopen().unwrap();
    let mut provider_ok = LazyCsumProvider::new(
        lazy_ok,
        ctx_ok.superblock.node_size as usize,
        ctx_ok.reader.base_offset(),
        ctx_ok.strategy,
        ctx_ok.superblock.devid,
        ctx_ok.superblock.fsid,
        ctx_ok.chunk_map.clone(),
        ctx_ok.roots.csum_root,
    );
    provider_ok.range(0, span, None, |_| {});
    assert_eq!(
        provider_ok.metadata_errors(),
        0,
        "healthy fs: no failures counted"
    );
    assert_eq!(provider_ok.mirror_mismatches(), 0);
    assert_eq!(provider_ok.metadata_read_errors(), 0);

    std::fs::remove_dir_all(&dir).ok();
}
