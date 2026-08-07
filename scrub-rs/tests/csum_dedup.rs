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
//! The second test pins the **stale-branch coverage counter** (H8): a
//! header-valid but generation-mismatched node (freed/repurposed by a live
//! transaction) is normal churn, NOT a metadata error — but it IS a
//! coverage gap, so it is counted into `stale_branches` (deduplicated per
//! node per run) instead of being silently dropped.
//!
//! Requires `mkfs.btrfs` on PATH (btrfs-progs); the tests skip gracefully
//! when it is absent.  No mount is needed: a freshly-formatted image file
//! has a valid chunk/root/csum tree, and the CSUM_TREE is patched in place
//! (corrupting the root's bytes for the failure test; rebuilding it as a
//! two-level tree with a stale child for the coverage test).

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

#[test]
fn stale_csum_branch_counted_once_per_run() {
    if !mkfs_available() {
        eprintln!("skipping: mkfs.btrfs not available");
        return;
    }

    let dir = std::env::temp_dir().join(format!("scrub_rs_csum_stale_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("fs.img");
    let img_str = img.to_str().unwrap().to_string();
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

    let ctx = scrub_rs::btrfs::open(&img_str, 0).expect("open freshly-formatted image");
    let csum_root = ctx.roots.csum_root;
    let node_size = ctx.superblock.node_size as usize;
    let fsid = ctx.superblock.fsid;
    assert!(csum_root != 0, "freshly-formatted fs must have a csum root");

    // Turn the (single-node, empty) CSUM_TREE into a two-level tree:
    //   * a crafted child LEAF whose header is checksum-valid but whose
    //     generation deliberately differs from the parent pointer's
    //     expected generation — read_node classifies exactly this as
    //     "stale" (freed/repurposed block), and walk_leaves routes it to
    //     on_stale;
    //   * the original root promoted to an INTERNAL node (level 1,
    //     nritems 1) holding one key pointer to that child.
    // The stale child is the ONLY node with a parent, which is required:
    // tree roots are read with GEN_DONT_CHECK and can never be stale.
    let root_stripes = ctx
        .chunk_map
        .lookup_stripes(csum_root)
        .expect("csum root must resolve to physical stripes");
    let root_phys = root_stripes[0].1;
    let hash_len = ctx.strategy.hash_len as usize;
    let mut root_buf = vec![0u8; node_size];
    {
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&img)
            .unwrap();
        f.seek(SeekFrom::Start(root_phys)).unwrap();
        f.read_exact(&mut root_buf).unwrap();
        let hdr = scrub_rs::btrfs::node::Header::parse(&root_buf);
        let root_gen = hdr.generation;
        let root_owner = hdr.owner;
        let root_flags = hdr.flags;
        let chunk_tree_uuid = hdr.chunk_tree_uuid;

        // Find a free node slot in the SAME chunk as the csum root: probe
        // forward; the slot must map with the same logical->phys delta as
        // the root (same chunk = same stripe base) and currently be
        // all-zero (unused — live btrfs nodes always have a nonzero csum
        // field).  On a fresh 64 MiB image the metadata chunk is almost
        // entirely empty, so the first probe succeeds.
        let mut child_logical = 0u64;
        let mut child_phys = 0u64;
        let mut found = false;
        for k in 1u64..4096 {
            let cand = csum_root + k * node_size as u64;
            let Some(stripes) = ctx.chunk_map.lookup_stripes(cand) else {
                continue;
            };
            if stripes[0].1 != root_phys + k * node_size as u64 {
                continue; // different chunk (different stripe base) — skip
            }
            let mut probe = vec![0u8; node_size];
            f.seek(SeekFrom::Start(stripes[0].1)).unwrap();
            f.read_exact(&mut probe).unwrap();
            if probe.iter().all(|&b| b == 0) {
                child_logical = cand;
                child_phys = stripes[0].1;
                found = true;
                break;
            }
        }
        assert!(found, "no free node slot found next to the csum root");

        // ---- Craft the stale child leaf (level 0, nritems 1) ----
        // One EXTENT_CSUM item covering logical sector [0, 4096).
        let mut child = vec![0u8; node_size];
        child[32..48].copy_from_slice(&fsid);
        child[48..56].copy_from_slice(&child_logical.to_le_bytes());
        child[56..64].copy_from_slice(&root_flags.to_le_bytes());
        child[64..80].copy_from_slice(&chunk_tree_uuid);
        child[80..88].copy_from_slice(&(root_gen + 1).to_le_bytes()); // WRONG generation
        child[88..96].copy_from_slice(&root_owner.to_le_bytes());
        child[96..100].copy_from_slice(&1u32.to_le_bytes()); // nritems = 1
        child[100] = 0; // level: leaf
        let slot = scrub_rs::btrfs::node::HEADER_SIZE; // 101: slot 0 starts here
        child[slot..slot + 8]
            .copy_from_slice(&scrub_rs::btrfs::key::objectid::EXTENT_CSUM_OBJECTID.to_le_bytes());
        child[slot + 8] = scrub_rs::btrfs::key::key_type::EXTENT_CSUM;
        child[slot + 9..slot + 17].copy_from_slice(&0u64.to_le_bytes()); // item key.offset = 0
        child[slot + 17..slot + 21]
            .copy_from_slice(&(scrub_rs::btrfs::node::LEAF_ITEM_SIZE as u32).to_le_bytes());
        child[slot + 21..slot + 25].copy_from_slice(&(hash_len as u32).to_le_bytes());
        let data_abs = scrub_rs::btrfs::node::HEADER_SIZE + scrub_rs::btrfs::node::LEAF_ITEM_SIZE;
        for b in child[data_abs..data_abs + hash_len].iter_mut() {
            *b = 0xAA; // dummy csum payload
        }
        // Recompute the header checksum (first hash_len bytes of the 32-byte
        // csum field; checksummed body starts at byte 32).
        let csum = ctx.strategy.compute(&child[32..]);
        child[..hash_len].copy_from_slice(&csum);

        // ---- Promote the root to an internal node pointing at the child ----
        root_buf[96..100].copy_from_slice(&1u32.to_le_bytes()); // nritems = 1
        root_buf[100] = 1; // level: internal
        let ptr = scrub_rs::btrfs::node::HEADER_SIZE; // 101: KeyPtr 0
        root_buf[ptr..ptr + 8]
            .copy_from_slice(&scrub_rs::btrfs::key::objectid::EXTENT_CSUM_OBJECTID.to_le_bytes());
        root_buf[ptr + 8] = scrub_rs::btrfs::key::key_type::EXTENT_CSUM;
        root_buf[ptr + 9..ptr + 17].copy_from_slice(&0u64.to_le_bytes()); // ptr key.offset = 0
        root_buf[ptr + 17..ptr + 25].copy_from_slice(&child_logical.to_le_bytes()); // blockptr
        root_buf[ptr + 25..ptr + 33].copy_from_slice(&root_gen.to_le_bytes()); // expected gen
        let csum = ctx.strategy.compute(&root_buf[32..]);
        root_buf[..hash_len].copy_from_slice(&csum);

        // Write both nodes back.
        f.seek(SeekFrom::Start(child_phys)).unwrap();
        f.write_all(&child).unwrap();
        f.seek(SeekFrom::Start(root_phys)).unwrap();
        f.write_all(&root_buf).unwrap();
    }

    // Walk the patched tree: the stale child must be counted exactly once
    // (deduplicated across overlapping walks) and must NOT count as a
    // metadata-header error (stale is a coverage gap, not corruption).
    let lazy_file = ctx.reader.reopen().expect("dup reader fd");
    let mut provider = scrub_rs::btrfs::csum::LazyCsumProvider::new(
        lazy_file,
        node_size,
        ctx.reader.base_offset(),
        ctx.strategy,
        ctx.superblock.devid,
        ctx.superblock.fsid,
        ctx.chunk_map.clone(),
        csum_root,
    );
    let span = ctx.superblock.total_bytes.min(16 << 20);
    provider.range(0, span, None, |_| {});
    assert_eq!(
        provider.stale_branches(),
        1,
        "first walk counts the stale branch once"
    );
    assert_eq!(
        provider.metadata_errors(),
        0,
        "a stale branch is NOT a metadata-header error"
    );
    provider.range(0, span, None, |_| {});
    assert_eq!(
        provider.stale_branches(),
        1,
        "overlapping walk must NOT re-count the same stale node (dedup by bytenr)"
    );
    assert_eq!(provider.mirror_mismatches(), 0);
    assert_eq!(provider.metadata_read_errors(), 0);

    std::fs::remove_dir_all(&dir).ok();
}
