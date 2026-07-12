#!/usr/bin/env bash
#
# btrfs_test_matrix.sh
#
# Generates a matrix of SINGLE-DISK btrfs filesystem images (one loop-backed
# file each) for testing a read-only scrub walker / filesystem verifier.
# This is the btrfs counterpart to zfs_test_matrix.sh -- same goals, adapted
# to btrfs's very different on-disk model (multiple independent B-trees:
# extent tree, chunk tree, root tree, csum/free-space tree, per-subvolume
# trees, uuid tree -- rather than ZFS's one-dnode-per-object model).
#
# Scope, deliberately narrowed to match:
#   - Every filesystem here is single-device. raid0/1/1c3/1c4/5/6/10 profiles
#     are NOT generated -- they're multi-disk features and out of scope.
#     "single" and "dup" profiles ARE generated where relevant, since both
#     are valid on one device (dup just duplicates blocks on the same disk,
#     directly analogous to ZFS's copies/redundant_metadata).
#   - No encryption (btrfs has none natively -- it's layered via LUKS
#     underneath, which is a separate concern from the filesystem itself).
#   - No block-level dedup (btrfs has none natively either -- closest analog
#     is an explicit reflink copy, which IS exercised here since it creates
#     genuinely shared extents, similar in spirit to a ZFS clone).
#
# Two important differences from the ZFS script's structure:
#   1. In ZFS, compression/checksum/etc. are per-DATASET properties, so one
#      pool could hold many differently-configured child datasets. In btrfs,
#      checksum algorithm and nodesize are fixed at mkfs time for the WHOLE
#      filesystem and cannot vary within one image. So checksum/nodesize
#      variation here means separate images, one per value, the same way
#      the ZFS ztest recipes needed their own pools.
#   2. Compression in btrfs CAN vary within one filesystem, via
#      `btrfs property set <path> compression <algo>` per file/directory,
#      independent of mount options -- so compression variety is still
#      exercised within single images via that mechanism.
#
# Corruption tooling: btrfs ships a purpose-built `btrfs-corrupt-block`
# tool for exactly this ("Corrupt data structures on a btrfs filesystem.
# For testing only!"), far more precise than the byte-flipping hacks the
# ZFS script had to resort to. NOTE: some distros only install it when
# btrfs-progs is built with --enable-experimental (this is genuinely
# version/distro-dependent, not something this script can guarantee), so
# it's soft-checked and those specific corruption sub-steps are skipped
# with a warning if it's absent -- everything else still runs.
#
# Ground truth, same philosophy as the ZFS script: every filesystem gets
# BOTH a live `btrfs scrub` (while mounted) AND an offline
# `btrfs check --check-data-csum --readonly` (while unmounted) captured
# before being handed to you. If your walker disagrees with both of those,
# the bug is almost certainly in the walker.
#
# Target: Debian/Ubuntu with btrfs-progs installed and the btrfs kernel
# module available (built-in on most distro kernels). Must be run as root.
#
# Usage:
#   sudo ./btrfs_test_matrix.sh [outdir]
#
set -uo pipefail

# ---------------------------------------------------------------------------
# Config
# ---------------------------------------------------------------------------
OUTDIR="${1:-./btrfs_test_images}"
WORKDIR="$(mktemp -d /tmp/btrfs_test_matrix.XXXXXX)"
IMG_SIZE="${IMG_SIZE:-256M}"
LOGFILE="${OUTDIR}/build.log"
MANIFEST="${OUTDIR}/manifest.tsv"

# Standard btrfs superblock locations (stable across versions). The third
# copy at 256GiB doesn't exist on images this small, so it's not used here.
SB_PRIMARY_OFFSET=65536       # 64 KiB
SB_BACKUP1_OFFSET=67108864    # 64 MiB
SB_SIZE=4096

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------
log() { echo "[$(date +%H:%M:%S)] $*" | tee -a "$LOGFILE"; }
die() { log "FATAL: $*"; exit 1; }

require_root() {
  [[ $EUID -eq 0 ]] || die "must run as root (mount/losetup need root)."
}

require_btrfs() {
  command -v mkfs.btrfs >/dev/null 2>&1 || die "mkfs.btrfs not found. apt install btrfs-progs"
  command -v btrfs >/dev/null 2>&1 || die "btrfs not found. apt install btrfs-progs"
  modprobe btrfs 2>/dev/null || true
  grep -q btrfs /proc/filesystems || log "WARN: btrfs not listed in /proc/filesystems -- module may not be loaded/built-in"
}

have_corrupt_block() {
  command -v btrfs-corrupt-block >/dev/null 2>&1
}

record_manifest() {
  printf '%s\t%s\t%s\n' "$1" "$2" "$3" >> "$MANIFEST"
}

make_image() {
  local path="$1" size="$2"
  truncate -s "$size" "$path"
}

# ---------------------------------------------------------------------------
# mkfs / mount / umount
# ---------------------------------------------------------------------------

# btrfs_mkfs <img> <size> <checksum> <nodesize> <data_profile> <meta_profile> [extra mkfs args...]
btrfs_mkfs() {
  local img="$1" size="$2" csum="$3" nodesize="$4" dprofile="$5" mprofile="$6"
  shift 6
  make_image "$img" "$size"
  mkfs.btrfs -f -q --csum "$csum" -n "$nodesize" -d "$dprofile" -m "$mprofile" "$@" "$img" >>"$LOGFILE" 2>&1 \
    || die "mkfs.btrfs failed for $img (csum=$csum nodesize=$nodesize d=$dprofile m=$mprofile) -- see $LOGFILE"
}

# btrfs_mount <img> <mountpoint> [extra mount -o opts, comma-separated]
btrfs_mount() {
  local img="$1" mnt="$2" opts="${3:-}"
  mkdir -p "$mnt"
  local loopdev
  loopdev="$(losetup --show -f "$img")" || die "losetup failed for $img"
  if [[ -n "$opts" ]]; then
    mount -o "$opts" "$loopdev" "$mnt" || die "mount failed for $img via $loopdev"
  else
    mount "$loopdev" "$mnt" || die "mount failed for $img via $loopdev"
  fi
  echo "$loopdev" > "$WORKDIR/.loopdev_$(basename "$mnt")"
}

# btrfs_umount <mountpoint>
btrfs_umount() {
  local mnt="$1"
  sync
  umount "$mnt" 2>/dev/null || umount -l "$mnt" 2>/dev/null || true
  local loopfile="$WORKDIR/.loopdev_$(basename "$mnt")"
  if [[ -f "$loopfile" ]]; then
    local loopdev
    loopdev="$(cat "$loopfile")"
    losetup -d "$loopdev" 2>/dev/null || true
    rm -f "$loopfile"
  fi
}

# ---------------------------------------------------------------------------
# Ground truth: live scrub (mounted) + offline check (unmounted)
# ---------------------------------------------------------------------------

wait_for_scrub_btrfs() {
  local mnt="$1"
  timeout 300 btrfs scrub start -B "$mnt" >>"$LOGFILE" 2>&1 \
    || log "WARN: btrfs scrub on $mnt timed out, failed to start, or found errors (nonzero exit can be normal for the corrupted variants)"
}

# verify_scrub_btrfs <mountpoint> <outfile>
verify_scrub_btrfs() {
  local mnt="$1" outfile="$2"
  log "scrubbing $mnt for ground-truth verification..."
  wait_for_scrub_btrfs "$mnt"
  btrfs scrub status -v "$mnt" > "$outfile" 2>&1 || true
  log "$mnt: scrub status saved to $(basename "$outfile") -- inspect it for the real result"
}

# verify_check_offline <img> <outfile>
# Returns btrfs check's exit code (0 = clean).
verify_check_offline() {
  local img="$1" outfile="$2"
  btrfs check --readonly --check-data-csum "$img" > "$outfile" 2>&1
  local rc=$?
  log "$(basename "$img"): offline btrfs check exit=$rc, saved to $(basename "$outfile")"
  return $rc
}

# finalize_fs <img> <mountpoint> <destdir> <label>
finalize_fs() {
  local img="$1" mnt="$2" destdir="$3" label="$4"
  mkdir -p "$destdir"
  verify_scrub_btrfs "$mnt" "$WORKDIR/${label}_scrub_status.txt"
  btrfs_umount "$mnt"
  verify_check_offline "$img" "$WORKDIR/${label}_check_status.txt" || true
  cp -a "$WORKDIR/${label}_scrub_status.txt" "$destdir/" 2>/dev/null || true
  cp -a "$WORKDIR/${label}_check_status.txt" "$destdir/" 2>/dev/null || true
  cp -a "$img" "$destdir/"
  log "finalized $label -> $destdir"
}

# ---------------------------------------------------------------------------
# Data population helpers
# ---------------------------------------------------------------------------

populate_tiny() {
  local mnt="$1" n="${2:-300}"
  mkdir -p "$mnt/tiny"
  for i in $(seq 1 "$n"); do
    # well under btrfs's default ~2048-byte max_inline threshold -> should
    # be stored inline in a b-tree leaf rather than as a separate extent
    echo "tiny-$i-$(head -c 16 /dev/urandom | base64)" > "$mnt/tiny/f_$i.txt"
  done
}

populate_compressible() {
  local mnt="$1"
  mkdir -p "$mnt/compressible"
  btrfs property set "$mnt/compressible" compression zstd 2>/dev/null || true
  for i in 1 2 3 4 5; do
    yes "the quick brown fox jumps over the lazy dog. " | head -c 2097152 > "$mnt/compressible/repeat_$i.txt"
  done
}

populate_incompressible() {
  local mnt="$1"
  mkdir -p "$mnt/random"
  btrfs property set "$mnt/random" compression no 2>/dev/null || true
  for i in 1 2 3; do
    dd if=/dev/urandom of="$mnt/random/blob_$i.bin" bs=1M count=2 status=none
  done
}

populate_sparse() {
  local mnt="$1"
  mkdir -p "$mnt/sparse"
  dd if=/dev/zero of="$mnt/sparse/holey.img" bs=1M count=0 seek=64 status=none
  dd if=/dev/urandom of="$mnt/sparse/holey.img" bs=4k count=4 seek=1000 conv=notrunc status=none
}

# writes in several separate passes to encourage multiple extents rather
# than one contiguous run
populate_large_multiextent() {
  local mnt="$1" chunks="${2:-4}" chunk_mb="${3:-10}"
  mkdir -p "$mnt/large"
  for i in $(seq 1 "$chunks"); do
    dd if=/dev/urandom of="$mnt/large/big.bin" bs=1M count="$chunk_mb" \
       seek=$(( (i - 1) * chunk_mb )) conv=notrunc status=none
    sync
  done
}

populate_structural() {
  local mnt="$1"
  local d="$mnt/deep"
  mkdir -p "$d"
  for i in $(seq 1 20); do d="$d/level_$i"; mkdir -p "$d"; done
  echo "leaf" > "$d/leaf.txt"
  ln -s "$mnt/deep/level_1" "$mnt/deep/symlink_to_level1"
  local longtarget
  longtarget=$(printf '/very/long/path/segment%.0s' $(seq 1 20))
  ln -s "$longtarget" "$mnt/deep/long_symlink" 2>/dev/null || true
  echo "hardlink target" > "$mnt/deep/hardtarget.txt"
  ln "$mnt/deep/hardtarget.txt" "$mnt/deep/hardlink_copy.txt"
}

populate_xattrs() {
  local mnt="$1"
  mkdir -p "$mnt/xattr"
  echo "has xattrs" > "$mnt/xattr/file.txt"
  if command -v setfattr >/dev/null 2>&1; then
    setfattr -n user.comment -v "test-attribute-value" "$mnt/xattr/file.txt" 2>/dev/null || true
    setfattr -n user.another -v "second-value-here" "$mnt/xattr/file.txt" 2>/dev/null || true
  fi
}

populate_long_names() {
  local mnt="$1"
  mkdir -p "$mnt/longnames"
  local longname
  longname=$(printf 'a%.0s' $(seq 1 200))
  echo "long name test" > "$mnt/longnames/$longname"
}

# many entries in one directory -> forces the directory's b-tree items
# across multiple leaf nodes
populate_many_entries_dir() {
  local mnt="$1" n="${2:-5000}"
  mkdir -p "$mnt/manyfiles"
  for i in $(seq 1 "$n"); do
    : > "$mnt/manyfiles/entry_$i"
  done
}

# subvolume + snapshot chain + an explicit reflink -> shared/COW extents,
# the closest btrfs analog to the ZFS snapshot-chain recipe
populate_subvolume_snapshot_chain() {
  local mnt="$1"
  btrfs subvolume create "$mnt/subvol1" >>"$LOGFILE" 2>&1
  echo "base content" > "$mnt/subvol1/shared.txt"
  populate_compressible "$mnt/subvol1"
  btrfs subvolume snapshot "$mnt/subvol1" "$mnt/subvol1_snap1" >>"$LOGFILE" 2>&1
  echo "modified after snap1" > "$mnt/subvol1/shared.txt"          # COW: old block still held by snap1
  rm -f "$mnt/subvol1/compressible/repeat_1.txt"                    # deferred free: only snap1 references it now
  cp --reflink=always "$mnt/subvol1/shared.txt" "$mnt/subvol1/shared_reflink.txt" 2>/dev/null \
    || cp "$mnt/subvol1/shared.txt" "$mnt/subvol1/shared_reflink.txt"
  btrfs subvolume snapshot "$mnt/subvol1" "$mnt/subvol1_snap2" >>"$LOGFILE" 2>&1
}

# ---------------------------------------------------------------------------
# Corruption helpers
#
# Two tiers of confidence here, same philosophy as the ZFS script:
#
#   - Superblock zeroing (zero_range at SB_PRIMARY_OFFSET / SB_BACKUP1_OFFSET)
#     is high-confidence: these offsets are stable across btrfs versions.
#
#   - Data-block corruption locates the target byte via `filefrag -v` on the
#     MOUNTED filesystem (which reports real physical block numbers on the
#     underlying loop device -> file, with no extra header offset since our
#     loop devices aren't given an --offset), then flips a byte directly in
#     the unmounted image file. This avoids needing to parse btrfs's
#     internal logical-vs-physical address mapping, which is genuinely
#     harder to get right than ZFS's DVA scheme. It self-verifies via a
#     re-scrub afterwards and WARNs if the injected error didn't show up.
#
#   - Whole-tree corruption uses btrfs-corrupt-block's own documented
#     --extent-tree flag, which needs no offset/field-name knowledge at all.
# ---------------------------------------------------------------------------

flip_byte_at_offset() {
  local file="$1" offset="$2"
  command -v python3 >/dev/null 2>&1 || { log "WARN: python3 not found, cannot flip byte"; return 1; }
  python3 - "$file" "$offset" <<'PYEOF' 2>/dev/null
import sys
path, off = sys.argv[1], int(sys.argv[2])
with open(path, 'r+b') as f:
    f.seek(off)
    b = f.read(1)
    if not b:
        sys.exit(1)
    f.seek(off)
    f.write(bytes([b[0] ^ 0xFF]))
PYEOF
}

zero_range() {
  local file="$1" offset="$2" length="$3"
  dd if=/dev/zero of="$file" bs=1 seek="$offset" count="$length" conv=notrunc status=none
}

# find_physical_byte_offset <mounted_file_path>
# Prints the byte offset (within the backing image file) of the first
# physical block reported by filefrag for this file, or nothing on failure.
find_physical_byte_offset() {
  local f="$1"
  local line
  line="$(filefrag -v "$f" 2>/dev/null | awk '/^[[:space:]]*0:/{print; exit}')"
  [[ -z "$line" ]] && return 1
  # typical filefrag -v line:
  #   0:        0..       7:     123456..    123463:      8:             last,eof
  # field 5 (colon/dot separated) is the first physical block number
  local physblock
  physblock="$(awk -F'[.:]+' '{gsub(/ /,"",$5); print $5}' <<<"$line")"
  [[ -z "$physblock" || ! "$physblock" =~ ^[0-9]+$ ]] && return 1
  echo $(( physblock * 4096 ))
}

# ===========================================================================
# Recipes
# ===========================================================================

recipe_01_baseline_crc32c() {
  local label="btrfs_01"
  local img="$WORKDIR/${label}.img"
  local mnt="$WORKDIR/mnt_${label}"
  btrfs_mkfs "$img" "$IMG_SIZE" crc32c 16k single dup
  btrfs_mount "$img" "$mnt"
  populate_compressible "$mnt"
  populate_incompressible "$mnt"
  populate_sparse "$mnt"
  populate_tiny "$mnt"
  populate_large_multiextent "$mnt"
  populate_structural "$mnt"
  populate_xattrs "$mnt"
  populate_long_names "$mnt"
  populate_subvolume_snapshot_chain "$mnt"
  record_manifest "$label" checksum crc32c
  record_manifest "$label" nodesize 16k
  record_manifest "$label" profiles "data=single, metadata=dup (defaults for a single device)"
  record_manifest "$label" note "full data-shape population: compression via btrfs property set, inline-candidate tiny files, multi-extent large file, subvolume+snapshot chain with a reflink"
  finalize_fs "$img" "$mnt" "$OUTDIR/01_baseline_crc32c" "$label"
}

recipe_02_checksum_xxhash() {
  local label="btrfs_02"
  local img="$WORKDIR/${label}.img"
  local mnt="$WORKDIR/mnt_${label}"
  btrfs_mkfs "$img" "$IMG_SIZE" xxhash 16k single dup
  btrfs_mount "$img" "$mnt"
  populate_compressible "$mnt"
  populate_incompressible "$mnt"
  populate_tiny "$mnt"
  populate_large_multiextent "$mnt"
  record_manifest "$label" checksum xxhash
  finalize_fs "$img" "$mnt" "$OUTDIR/02_checksum_xxhash" "$label"
}

recipe_03_checksum_sha256() {
  local label="btrfs_03"
  local img="$WORKDIR/${label}.img"
  local mnt="$WORKDIR/mnt_${label}"
  btrfs_mkfs "$img" "$IMG_SIZE" sha256 16k single dup
  btrfs_mount "$img" "$mnt"
  populate_compressible "$mnt"
  populate_incompressible "$mnt"
  populate_tiny "$mnt"
  populate_large_multiextent "$mnt"
  record_manifest "$label" checksum sha256
  finalize_fs "$img" "$mnt" "$OUTDIR/03_checksum_sha256" "$label"
}

recipe_04_checksum_blake2() {
  local label="btrfs_04"
  local img="$WORKDIR/${label}.img"
  local mnt="$WORKDIR/mnt_${label}"
  btrfs_mkfs "$img" "$IMG_SIZE" blake2 16k single dup
  btrfs_mount "$img" "$mnt"
  populate_compressible "$mnt"
  populate_incompressible "$mnt"
  populate_tiny "$mnt"
  populate_large_multiextent "$mnt"
  record_manifest "$label" checksum blake2
  finalize_fs "$img" "$mnt" "$OUTDIR/04_checksum_blake2" "$label"
}

recipe_05_nodesize_4k() {
  local label="btrfs_05"
  local img="$WORKDIR/${label}.img"
  local mnt="$WORKDIR/mnt_${label}"
  btrfs_mkfs "$img" "$IMG_SIZE" crc32c 4k single dup
  btrfs_mount "$img" "$mnt"
  populate_many_entries_dir "$mnt" 3000
  populate_large_multiextent "$mnt" 6 8
  record_manifest "$label" nodesize "4k (minimum) -- smaller b-tree nodes, more/shallower-fanout leaves for the same data than 16k default"
  finalize_fs "$img" "$mnt" "$OUTDIR/05_nodesize_4k" "$label"
}

recipe_06_nodesize_64k() {
  local label="btrfs_06"
  local img="$WORKDIR/${label}.img"
  local mnt="$WORKDIR/mnt_${label}"
  btrfs_mkfs "$img" "$IMG_SIZE" crc32c 64k single dup
  btrfs_mount "$img" "$mnt"
  populate_many_entries_dir "$mnt" 3000
  populate_large_multiextent "$mnt" 6 8
  record_manifest "$label" nodesize "64k (maximum) -- larger b-tree nodes, higher fanout than 16k default, for the same data as recipe 05"
  finalize_fs "$img" "$mnt" "$OUTDIR/06_nodesize_64k" "$label"
}

recipe_07_profile_dup_data_dup_meta() {
  local label="btrfs_07"
  local img="$WORKDIR/${label}.img"
  local mnt="$WORKDIR/mnt_${label}"
  btrfs_mkfs "$img" "$IMG_SIZE" crc32c 16k dup dup
  btrfs_mount "$img" "$mnt"
  populate_compressible "$mnt"
  populate_tiny "$mnt"
  record_manifest "$label" profiles "data=dup, metadata=dup -- both data AND metadata duplicated on the single device (heavier analog of ZFS copies=2)"
  finalize_fs "$img" "$mnt" "$OUTDIR/07_profile_dup_data_dup_meta" "$label"
}

recipe_08_profile_single_meta() {
  local label="btrfs_08"
  local img="$WORKDIR/${label}.img"
  local mnt="$WORKDIR/mnt_${label}"
  btrfs_mkfs "$img" "$IMG_SIZE" crc32c 16k single single
  btrfs_mount "$img" "$mnt"
  populate_compressible "$mnt"
  populate_tiny "$mnt"
  record_manifest "$label" profiles "data=single, metadata=single -- deliberately NO redundancy anywhere (btrfs normally defaults metadata to dup even on one device); good 'expect zero ditto copies' edge case"
  finalize_fs "$img" "$mnt" "$OUTDIR/08_profile_single_meta" "$label"
}

recipe_09_inline_extent_threshold() {
  local label_default="btrfs_09a" label_noinline="btrfs_09b"
  local img_default="$WORKDIR/${label_default}.img"
  local img_noinline="$WORKDIR/${label_noinline}.img"
  local mnt_default="$WORKDIR/mnt_${label_default}"
  local mnt_noinline="$WORKDIR/mnt_${label_noinline}"

  btrfs_mkfs "$img_default" "$IMG_SIZE" crc32c 16k single dup
  btrfs_mount "$img_default" "$mnt_default"   # default max_inline (~2048 bytes)
  mkdir -p "$mnt_default/boundary"
  for size in 100 500 1000 1900 2000 2100 4096; do
    head -c "$size" /dev/urandom > "$mnt_default/boundary/f_${size}.bin"
  done
  record_manifest "$label_default" note "default max_inline (~2048B) -- files at 100/500/1000/1900/2000/2100/4096 bytes straddle the inline-vs-extent boundary"
  finalize_fs "$img_default" "$mnt_default" "$OUTDIR/09_inline_extent_threshold/a_default_max_inline" "$label_default"

  btrfs_mkfs "$img_noinline" "$IMG_SIZE" crc32c 16k single dup
  btrfs_mount "$img_noinline" "$mnt_noinline" "max_inline=0"   # disable inlining entirely
  mkdir -p "$mnt_noinline/boundary"
  for size in 100 500 1000 1900 2000 2100 4096; do
    head -c "$size" /dev/urandom > "$mnt_noinline/boundary/f_${size}.bin"
  done
  record_manifest "$label_noinline" note "max_inline=0 -- same file sizes as the default-mount variant, but inlining disabled, so even the smallest files should get a real extent. Compare the two to see how your walker handles inline file-extent items vs regular ones."
  finalize_fs "$img_noinline" "$mnt_noinline" "$OUTDIR/09_inline_extent_threshold/b_max_inline_0" "$label_noinline"
}

recipe_10_directory_and_symlink_shapes() {
  local label="btrfs_10"
  local img="$WORKDIR/${label}.img"
  local mnt="$WORKDIR/mnt_${label}"
  btrfs_mkfs "$img" "$IMG_SIZE" crc32c 16k single dup
  btrfs_mount "$img" "$mnt"
  populate_many_entries_dir "$mnt" 8000
  populate_long_names "$mnt"
  populate_structural "$mnt"   # includes short + long symlinks, hardlinks, deep nesting
  populate_xattrs "$mnt"
  record_manifest "$label" note "8000-entry directory forces the dir b-tree items across many leaves; 200-char filename; long symlink target; hardlinks and deep nesting -- stress cases for object-type handling beyond the simple/small case of each"
  finalize_fs "$img" "$mnt" "$OUTDIR/10_directory_and_symlink_shapes" "$label"
}

recipe_11_corrupted_known_bad() {
  local base_destdir="$OUTDIR/11_corrupted_known_bad"
  mkdir -p "$base_destdir"
  local label="btrfs_11"
  local pristine="$WORKDIR/${label}_pristine.img"
  local mnt="$WORKDIR/mnt_${label}"

  btrfs_mkfs "$pristine" "$IMG_SIZE" crc32c 16k single dup
  btrfs_mount "$pristine" "$mnt"
  dd if=/dev/urandom of="$mnt/target_data.bin" bs=4k count=8 status=none
  sync

  local phys_off
  phys_off="$(find_physical_byte_offset "$mnt/target_data.bin")"
  if [[ -z "$phys_off" ]]; then
    log "WARN: [corrupted_known_bad] could not locate target_data.bin's physical offset via filefrag -- data-bitflip variant will be skipped"
  fi

  verify_scrub_btrfs "$mnt" "$WORKDIR/${label}_pristine_scrub.txt"
  btrfs_umount "$mnt"
  record_manifest "$label" note "pristine baseline for 5 corrupted variants under 11_corrupted_known_bad/"

  # ---- variant a: data block bitflip (expect: scrub detects a checksum error) ----
  local va="$base_destdir/a_data_block_bitflip"; mkdir -p "$va"
  cp -a "$pristine" "$va/${label}.img"
  if [[ -n "$phys_off" ]] && flip_byte_at_offset "$va/${label}.img" "$phys_off"; then
    local mnt_a="$WORKDIR/mnt_va"
    if btrfs_mount "$va/${label}.img" "$mnt_a" "ro" 2>>"$LOGFILE"; then
      verify_scrub_btrfs "$mnt_a" "$va/EXPECTED_scrub_status.txt"
      btrfs_umount "$mnt_a"
    fi
    verify_check_offline "$va/${label}.img" "$va/EXPECTED_check_status.txt" || true
    if grep -qi "error" "$va/EXPECTED_check_status.txt" 2>/dev/null || grep -qi "csum" "$va/EXPECTED_scrub_status.txt" 2>/dev/null; then
      log "[data_block_bitflip] looks like the injected error was detected -- inspect the saved files to confirm"
    else
      log "WARN: [data_block_bitflip] neither scrub nor check obviously flagged an error -- treat as unverified, inspect files manually"
      echo "UNVERIFIED: neither scrub nor check obviously reported an error. filefrag-based offset math may not have landed inside the real block on this btrfs/kernel version." >> "$va/EXPECTED_check_status.txt"
    fi
  else
    log "SKIP [data_block_bitflip]: could not locate or flip target byte"
  fi

  # ---- variant b: whole extent tree corrupted (expect: check/mount fails hard) ----
  local vb="$base_destdir/b_extent_tree_corrupted"; mkdir -p "$vb"
  cp -a "$pristine" "$vb/${label}.img"
  if have_corrupt_block; then
    btrfs-corrupt-block -E "$vb/${label}.img" >>"$LOGFILE" 2>&1
    verify_check_offline "$vb/${label}.img" "$vb/EXPECTED_check_status.txt"
    local rc=$?
    if [[ $rc -ne 0 ]]; then
      log "[extent_tree_corrupted] confirmed: btrfs check reported failure, as expected"
    else
      log "WARN: [extent_tree_corrupted] btrfs check exited 0 despite corrupting the extent tree -- unexpected, treat as unverified"
    fi
    echo "EXPECTED: btrfs check should report serious errors -- the entire extent tree was corrupted via 'btrfs-corrupt-block -E'." >> "$vb/EXPECTED_check_status.txt"
  else
    log "SKIP [extent_tree_corrupted]: btrfs-corrupt-block not installed (may need btrfs-progs built with --enable-experimental)"
    echo "SKIPPED: btrfs-corrupt-block not found on this system." > "$vb/EXPECTED_check_status.txt"
  fi

  # ---- variant c: single superblock wiped (expect: still mounts/checks fine) ----
  local vc="$base_destdir/c_single_superblock_wiped"; mkdir -p "$vc"
  cp -a "$pristine" "$vc/${label}.img"
  zero_range "$vc/${label}.img" "$SB_PRIMARY_OFFSET" "$SB_SIZE"
  verify_check_offline "$vc/${label}.img" "$vc/EXPECTED_check_status.txt"
  local rc_c=$?
  if [[ $rc_c -eq 0 ]]; then
    log "[single_superblock_wiped] confirmed: pool still checks out via the backup superblock at 64MiB"
  else
    log "WARN: [single_superblock_wiped] btrfs check FAILED even with only the primary superblock wiped -- unexpected"
  fi
  echo "EXPECTED: should still succeed -- only the primary superblock (offset 64KiB) was wiped; btrfs keeps a backup copy at 64MiB on any device this size." >> "$vc/EXPECTED_check_status.txt"

  # ---- variant d: all superblocks wiped (expect: check/mount fails outright) ----
  local vd="$base_destdir/d_all_superblocks_wiped"; mkdir -p "$vd"
  cp -a "$pristine" "$vd/${label}.img"
  zero_range "$vd/${label}.img" "$SB_PRIMARY_OFFSET" "$SB_SIZE"
  zero_range "$vd/${label}.img" "$SB_BACKUP1_OFFSET" "$SB_SIZE"
  verify_check_offline "$vd/${label}.img" "$vd/EXPECTED_check_status.txt"
  local rc_d=$?
  if [[ $rc_d -ne 0 ]]; then
    log "[all_superblocks_wiped] confirmed: btrfs check refused, as expected"
  else
    log "WARN: [all_superblocks_wiped] btrfs check exited 0 despite both reachable superblocks being wiped -- unexpected"
  fi
  echo "EXPECTED: should FAIL -- both superblock copies that exist on a device this size (64KiB primary, 64MiB backup) were zeroed." >> "$vd/EXPECTED_check_status.txt"

  # ---- variant e: truncated image (expect: check/mount fails / reports missing device) ----
  local ve="$base_destdir/e_truncated_image"; mkdir -p "$ve"
  cp -a "$pristine" "$ve/${label}.img"
  local filesize
  filesize=$(stat -c %s "$ve/${label}.img")
  truncate -s "$(( filesize * 60 / 100 ))" "$ve/${label}.img"
  verify_check_offline "$ve/${label}.img" "$ve/EXPECTED_check_status.txt"
  local rc_e=$?
  if [[ $rc_e -ne 0 ]]; then
    log "[truncated_image] confirmed: btrfs check refused a truncated device, as expected"
  else
    log "WARN: [truncated_image] btrfs check exited 0 despite the image being truncated to 60% -- unexpected"
  fi
  echo "EXPECTED: should FAIL or report a missing/short device -- file truncated to 60% of its original size." >> "$ve/EXPECTED_check_status.txt"

  cp -a "$pristine" "$base_destdir/pristine_baseline_${label}.img"
  record_manifest "$label" note "5 corrupted variants under 11_corrupted_known_bad/: a) data-block bitflip, b) whole extent-tree corrupted (needs btrfs-corrupt-block), c) single superblock wiped (should still work), d) all superblocks wiped (should fail), e) truncated to 60% (should fail). Each has EXPECTED_*.txt with the real btrfs-observed ground truth."
}

# ===========================================================================
# Main
# ===========================================================================
main() {
  require_root
  require_btrfs
  mkdir -p "$OUTDIR"
  : > "$LOGFILE"
  printf 'label\tfield\tvalue\n' > "$MANIFEST"

  log "workdir: $WORKDIR"
  log "outdir:  $OUTDIR"
  if have_corrupt_block; then
    log "btrfs-corrupt-block found: $(command -v btrfs-corrupt-block)"
  else
    log "btrfs-corrupt-block NOT found -- the extent-tree-corruption sub-step of recipe 11 will be skipped. This tool is sometimes only built with btrfs-progs' --enable-experimental configure flag."
  fi

  recipe_01_baseline_crc32c
  recipe_02_checksum_xxhash
  recipe_03_checksum_sha256
  recipe_04_checksum_blake2
  recipe_05_nodesize_4k
  recipe_06_nodesize_64k
  recipe_07_profile_dup_data_dup_meta
  recipe_08_profile_single_meta
  recipe_09_inline_extent_threshold
  recipe_10_directory_and_symlink_shapes
  recipe_11_corrupted_known_bad

  rm -rf "$WORKDIR"

  log "DONE. Filesystem images and manifest are under: $OUTDIR"
  log "Manifest: $MANIFEST"
  log "Each filesystem has <label>_scrub_status.txt (live btrfs scrub) and"
  log "  <label>_check_status.txt (offline btrfs check --check-data-csum) saved alongside it."
  log "Recipe 11's corrupted variants each have their own EXPECTED_*.txt ground truth."
  log ""
  log "To mount a generated image read-only, e.g.:"
  log "  sudo losetup -f --show $OUTDIR/01_baseline_crc32c/btrfs_01.img"
  log "  sudo mount -o ro,loop $OUTDIR/01_baseline_crc32c/btrfs_01.img /mnt/b"
}

main "$@"