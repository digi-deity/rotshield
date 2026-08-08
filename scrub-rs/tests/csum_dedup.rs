//! Integration tests for the lazy CSUM_TREE walker: metadata-failure and
//! stale-branch accounting must deduplicate overlapping range walks.

use std::io::{Read, Seek, SeekFrom, Write};
use std::process::Command;

use scrub_rs::btrfs::csum::LazyCsumProvider;

/// True when mkfs.btrfs is on PATH; the tests skip otherwise.
fn mkfs_available() -> bool {
    Command::new("mkfs.btrfs")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// A CSUM_TREE node whose all mirror copies fail header verification must
/// be counted exactly once, even when overlapping `range()` walks re-read
/// it. Corrupts the csum root's physical stripes, then walks the same
/// range twice.
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
    let total_bytes = ctx.superblock.total_bytes;
    assert!(csum_root != 0, "freshly-formatted fs must have a csum root");

    // Corrupt one byte of every physical stripe backing the csum root.
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

    // Construction must not touch the tree; counting happens per range().
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

    // Same span twice: the second walk must not re-count the bad node.
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

    assert_eq!(provider.mirror_mismatches(), 0);
    assert_eq!(provider.metadata_read_errors(), 0);

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

/// A stale (freed-and-repurposed) CSUM_TREE branch is a coverage gap, not
/// a metadata error, and must be counted once per run. Crafts a valid child
/// leaf next to the csum root with a bumped generation, links it from the
/// root, then walks twice.
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

    // Find a free node slot adjacent to the csum root (all-zero block on
    // a fresh image) to host the hand-built stale leaf.
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

        let mut child_logical = 0u64;
        let mut child_phys = 0u64;
        let mut found = false;
        for k in 1u64..4096 {
            let cand = csum_root + k * node_size as u64;
            let Some(stripes) = ctx.chunk_map.lookup_stripes(cand) else {
                continue;
            };
            if stripes[0].1 != root_phys + k * node_size as u64 {
                continue;
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

        // Hand-build the child leaf header + one EXTENT_CSUM item.
        let mut child = vec![0u8; node_size];
        child[32..48].copy_from_slice(&fsid);
        child[48..56].copy_from_slice(&child_logical.to_le_bytes());
        child[56..64].copy_from_slice(&root_flags.to_le_bytes());
        child[64..80].copy_from_slice(&chunk_tree_uuid);
        child[80..88].copy_from_slice(&(root_gen + 1).to_le_bytes());
        child[88..96].copy_from_slice(&root_owner.to_le_bytes());
        child[96..100].copy_from_slice(&1u32.to_le_bytes());
        child[100] = 0;
        let slot = scrub_rs::btrfs::node::HEADER_SIZE;
        child[slot..slot + 8]
            .copy_from_slice(&scrub_rs::btrfs::key::objectid::EXTENT_CSUM_OBJECTID.to_le_bytes());
        child[slot + 8] = scrub_rs::btrfs::key::key_type::EXTENT_CSUM;
        child[slot + 9..slot + 17].copy_from_slice(&0u64.to_le_bytes());
        child[slot + 17..slot + 21]
            .copy_from_slice(&(scrub_rs::btrfs::node::LEAF_ITEM_SIZE as u32).to_le_bytes());
        child[slot + 21..slot + 25].copy_from_slice(&(hash_len as u32).to_le_bytes());
        let data_abs = scrub_rs::btrfs::node::HEADER_SIZE + scrub_rs::btrfs::node::LEAF_ITEM_SIZE;
        for b in child[data_abs..data_abs + hash_len].iter_mut() {
            *b = 0xAA;
        }

        let csum = ctx.strategy.compute(&child[32..]);
        child[..hash_len].copy_from_slice(&csum);

        // Link the child from the root with the root's own generation, so
        // the walker sees an expired (stale) branch.
        root_buf[96..100].copy_from_slice(&1u32.to_le_bytes());
        root_buf[100] = 1;
        let ptr = scrub_rs::btrfs::node::HEADER_SIZE;
        root_buf[ptr..ptr + 8]
            .copy_from_slice(&scrub_rs::btrfs::key::objectid::EXTENT_CSUM_OBJECTID.to_le_bytes());
        root_buf[ptr + 8] = scrub_rs::btrfs::key::key_type::EXTENT_CSUM;
        root_buf[ptr + 9..ptr + 17].copy_from_slice(&0u64.to_le_bytes());
        root_buf[ptr + 17..ptr + 25].copy_from_slice(&child_logical.to_le_bytes());
        root_buf[ptr + 25..ptr + 33].copy_from_slice(&root_gen.to_le_bytes());
        let csum = ctx.strategy.compute(&root_buf[32..]);
        root_buf[..hash_len].copy_from_slice(&csum);

        f.seek(SeekFrom::Start(child_phys)).unwrap();
        f.write_all(&child).unwrap();
        f.seek(SeekFrom::Start(root_phys)).unwrap();
        f.write_all(&root_buf).unwrap();
    }

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
    // Overlapping walks must count the stale branch exactly once, and it
    // must not surface as a header error.
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
