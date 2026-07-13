#!/usr/bin/env bash
#
# btrfs_test_matrix.sh
#
# Generates a matrix of SINGLE-DISK btrfs filesystem images (one loop-backed
# file each) for testing a read-only scrub walker / filesystem verifier.
# Single-device only, by design -- no raid0/1/1c3/1c4/5/6/10 profiles
# (multi-disk), no LUKS, no block-level dedup. "single" and "dup" data/meta
# profiles are both in scope since both are valid on one device.
#
# This is the harmonized counterpart to run_matrix.sh and
# btrfs_live_scrub_test.sh: it sources btrfs_test_lib.sh (expected next to
# this script) for every mkfs/mount/corruption primitive, and in addition
# to the human-readable *_check_status.txt / *_scrub_status.txt ground
# truth, it emits a machine-readable expectations.tsv that run_matrix.sh
# consumes directly instead of re-deriving pass/fail by grepping btrfs
# check's text output.
#
# Usage:
#   sudo ./btrfs_test_matrix.sh [outdir]
#
# Env overrides:
#   IMG_SIZE=512M        default image size for the standard recipes
#   IMG_SIZE_SMALL=128M  used by recipe 14a
#   IMG_SIZE_LARGE=2G    used by recipe 14b
#   IMG_SIZE_NEARFULL=256M  used by recipe 15
#
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OUTDIR="${1:-./btrfs_test_images}"
WORKDIR="$(mktemp -d /tmp/btrfs_test_matrix.XXXXXX)"
IMG_SIZE="${IMG_SIZE:-512M}"
IMG_SIZE_SMALL="${IMG_SIZE_SMALL:-128M}"
IMG_SIZE_LARGE="${IMG_SIZE_LARGE:-2G}"
IMG_SIZE_NEARFULL="${IMG_SIZE_NEARFULL:-256M}"
LOGFILE="${OUTDIR}/build.log"
MANIFEST="${OUTDIR}/manifest.tsv"
EXPECTATIONS="${OUTDIR}/expectations.tsv"

mkdir -p "$OUTDIR"
: > "$LOGFILE"

# shellcheck source=btrfs_test_lib.sh
source "$SCRIPT_DIR/btrfs_test_lib.sh" || { echo "FATAL: could not source $SCRIPT_DIR/btrfs_test_lib.sh"; exit 1; }

# ===========================================================================
# Recipes 01-11: unchanged in substance from the original matrix, refactored
# onto the shared lib.
# ===========================================================================

recipe_01_baseline_crc32c() {
  local label="btrfs_01"
  local img="$WORKDIR/${label}.img" mnt="$WORKDIR/mnt_${label}"
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
  record_manifest "$label" profiles "data=single, metadata=dup (defaults for a single device)"
  record_manifest "$label" note "full data-shape population: compression, inline-candidate tiny files, multi-extent large file, subvolume+snapshot chain with a reflink"
  finalize_fs "$img" "$mnt" "$OUTDIR/01_baseline_crc32c" "$label"
}

recipe_02_checksum_xxhash() {
  local label="btrfs_02"
  local img="$WORKDIR/${label}.img" mnt="$WORKDIR/mnt_${label}"
  btrfs_mkfs "$img" "$IMG_SIZE" xxhash 16k single dup
  btrfs_mount "$img" "$mnt"
  populate_compressible "$mnt"; populate_incompressible "$mnt"; populate_tiny "$mnt"; populate_large_multiextent "$mnt"
  record_manifest "$label" checksum xxhash
  finalize_fs "$img" "$mnt" "$OUTDIR/02_checksum_xxhash" "$label"
}

recipe_03_checksum_sha256() {
  local label="btrfs_03"
  local img="$WORKDIR/${label}.img" mnt="$WORKDIR/mnt_${label}"
  btrfs_mkfs "$img" "$IMG_SIZE" sha256 16k single dup
  btrfs_mount "$img" "$mnt"
  populate_compressible "$mnt"; populate_incompressible "$mnt"; populate_tiny "$mnt"; populate_large_multiextent "$mnt"
  record_manifest "$label" checksum sha256
  finalize_fs "$img" "$mnt" "$OUTDIR/03_checksum_sha256" "$label"
}

recipe_04_checksum_blake2() {
  local label="btrfs_04"
  local img="$WORKDIR/${label}.img" mnt="$WORKDIR/mnt_${label}"
  btrfs_mkfs "$img" "$IMG_SIZE" blake2 16k single dup
  btrfs_mount "$img" "$mnt"
  populate_compressible "$mnt"; populate_incompressible "$mnt"; populate_tiny "$mnt"; populate_large_multiextent "$mnt"
  record_manifest "$label" checksum blake2
  finalize_fs "$img" "$mnt" "$OUTDIR/04_checksum_blake2" "$label"
}

recipe_05_nodesize_4k() {
  local label="btrfs_05"
  local img="$WORKDIR/${label}.img" mnt="$WORKDIR/mnt_${label}"
  btrfs_mkfs "$img" "$IMG_SIZE" crc32c 4k single dup
  btrfs_mount "$img" "$mnt"
  populate_many_entries_dir "$mnt" 3000; populate_large_multiextent "$mnt" 6 8
  record_manifest "$label" nodesize "4k (minimum)"
  finalize_fs "$img" "$mnt" "$OUTDIR/05_nodesize_4k" "$label"
}

recipe_06_nodesize_64k() {
  local label="btrfs_06"
  local img="$WORKDIR/${label}.img" mnt="$WORKDIR/mnt_${label}"
  btrfs_mkfs "$img" "$IMG_SIZE" crc32c 64k single dup
  btrfs_mount "$img" "$mnt"
  populate_many_entries_dir "$mnt" 3000; populate_large_multiextent "$mnt" 6 8
  record_manifest "$label" nodesize "64k (maximum)"
  finalize_fs "$img" "$mnt" "$OUTDIR/06_nodesize_64k" "$label"
}

recipe_07_profile_dup_data_dup_meta() {
  local label="btrfs_07"
  local img="$WORKDIR/${label}.img" mnt="$WORKDIR/mnt_${label}"
  btrfs_mkfs "$img" "$IMG_SIZE" crc32c 16k dup dup
  btrfs_mount "$img" "$mnt"
  populate_compressible "$mnt"; populate_tiny "$mnt"
  record_manifest "$label" profiles "data=dup, metadata=dup"
  finalize_fs "$img" "$mnt" "$OUTDIR/07_profile_dup_data_dup_meta" "$label"
}

recipe_08_profile_single_meta() {
  local label="btrfs_08"
  local img="$WORKDIR/${label}.img" mnt="$WORKDIR/mnt_${label}"
  btrfs_mkfs "$img" "$IMG_SIZE" crc32c 16k single single
  btrfs_mount "$img" "$mnt"
  populate_compressible "$mnt"; populate_tiny "$mnt"
  record_manifest "$label" profiles "data=single, metadata=single -- no redundancy anywhere"
  finalize_fs "$img" "$mnt" "$OUTDIR/08_profile_single_meta" "$label"
}

recipe_09_inline_extent_threshold() {
  local label_default="btrfs_09a" label_noinline="btrfs_09b"
  local img_default="$WORKDIR/${label_default}.img" img_noinline="$WORKDIR/${label_noinline}.img"
  local mnt_default="$WORKDIR/mnt_${label_default}" mnt_noinline="$WORKDIR/mnt_${label_noinline}"

  btrfs_mkfs "$img_default" "$IMG_SIZE" crc32c 16k single dup
  btrfs_mount "$img_default" "$mnt_default"
  mkdir -p "$mnt_default/boundary"
  for size in 100 500 1000 1900 2000 2100 4096; do head -c "$size" /dev/urandom > "$mnt_default/boundary/f_${size}.bin"; done
  record_manifest "$label_default" note "default max_inline (~2048B) boundary files"
  finalize_fs "$img_default" "$mnt_default" "$OUTDIR/09_inline_extent_threshold/a_default_max_inline" "$label_default"

  btrfs_mkfs "$img_noinline" "$IMG_SIZE" crc32c 16k single dup
  btrfs_mount "$img_noinline" "$mnt_noinline" "max_inline=0"
  mkdir -p "$mnt_noinline/boundary"
  for size in 100 500 1000 1900 2000 2100 4096; do head -c "$size" /dev/urandom > "$mnt_noinline/boundary/f_${size}.bin"; done
  record_manifest "$label_noinline" note "max_inline=0 -- inlining disabled entirely"
  finalize_fs "$img_noinline" "$mnt_noinline" "$OUTDIR/09_inline_extent_threshold/b_max_inline_0" "$label_noinline"
}

recipe_10_directory_and_symlink_shapes() {
  local label="btrfs_10"
  local img="$WORKDIR/${label}.img" mnt="$WORKDIR/mnt_${label}"
  btrfs_mkfs "$img" "$IMG_SIZE" crc32c 16k single dup
  btrfs_mount "$img" "$mnt"
  populate_many_entries_dir "$mnt" 8000; populate_long_names "$mnt"; populate_structural "$mnt"; populate_xattrs "$mnt"
  record_manifest "$label" note "8000-entry directory, long filename, long symlink target, hardlinks, deep nesting"
  finalize_fs "$img" "$mnt" "$OUTDIR/10_directory_and_symlink_shapes" "$label"
}

recipe_11_corrupted_known_bad() {
  local base_destdir="$OUTDIR/11_corrupted_known_bad"
  mkdir -p "$base_destdir"
  local label="btrfs_11"
  local pristine="$WORKDIR/${label}_pristine.img" mnt="$WORKDIR/mnt_${label}"

  btrfs_mkfs "$pristine" "$IMG_SIZE" crc32c 16k single dup
  btrfs_mount "$pristine" "$mnt"
  dd if=/dev/urandom of="$mnt/target_data.bin" bs=4k count=8 status=none
  sync
  local phys_off; phys_off="$(find_physical_byte_offset "$mnt/target_data.bin")"
  [[ -z "$phys_off" ]] && log "WARN: [corrupted_known_bad] could not locate target_data.bin's physical offset -- variant a will be skipped"
  verify_scrub_btrfs "$mnt" "$WORKDIR/${label}_pristine_scrub.txt"
  btrfs_umount "$mnt"
  record_manifest "$label" note "pristine baseline for corrupted variants under 11_corrupted_known_bad/"

  # a) data block bitflip
  local va="$base_destdir/a_data_block_bitflip"; mkdir -p "$va"
  cp -a "$pristine" "$va/${label}.img"
  if [[ -n "$phys_off" ]] && flip_byte_at_offset "$va/${label}.img" "$phys_off"; then
    verify_check_offline "$va/${label}.img" "$va/EXPECTED_check_status.txt" || true
    if grep -qi "error\|csum" "$va/EXPECTED_check_status.txt" 2>/dev/null; then
      log "[a_data_block_bitflip] injected error looks detected by ground truth"
      record_expectation "$va/${label}.img" data_corrupt 1 "single-byte data corruption via direct offset write"
    else
      log "WARN: [a_data_block_bitflip] ground truth didn't obviously flag it -- marking unverified"
      record_expectation "$va/${label}.img" unverified 0 "injection could not be confirmed by ground truth"
    fi
  else
    log "SKIP [a_data_block_bitflip]"
  fi

  # b) metadata-tree leaf csum corrupted (DEV_TREE)
  #
  # Targets the DEV_TREE specifically, not the EXTENT_TREE: scrub-rs's
  # open() walks the chunk tree and the root tree (for the metadata-mirror
  # cross-check) and its per-sector scrub drives reads off the DEV_TREE
  # (which it also walks eagerly in scrub_driver to enumerate dev-extents).
  # An EXTENT_TREE leaf is only located, never walked for mirror checks, so
  # an extent-tree-only corruption is invisible to scrub-rs by design and
  # would make this case FAIL rather than be a clean `meta_corrupt` PASS.
  # btrfs check (which audits every tree's metadata csum) and btrfs scrub
  # (which the kernel drives across every tree) both see it regardless.
  local vb="$base_destdir/b_dev_tree_corrupted"; mkdir -p "$vb"
  cp -a "$pristine" "$vb/${label}.img"
  if corrupt_dev_tree_whole "$vb/${label}.img"; then
    verify_check_offline "$vb/${label}.img" "$vb/EXPECTED_check_status.txt"
    if [[ $? -ne 0 ]]; then
      log "[b_dev_tree_corrupted] confirmed failure, as expected"
      record_expectation "$vb/${label}.img" meta_corrupt 1 "DEV tree leaf csum corrupted via btrfs-map-logical + direct byte flip (targeting a tree scrub-rs walks)"
    else
      log "WARN: [b_dev_tree_corrupted] btrfs check exited 0 -- unexpected"
      record_expectation "$vb/${label}.img" unverified 0 "expected failure, check reported clean"
    fi
  else
    echo "SKIPPED: could not corrupt DEV tree leaf." > "$vb/EXPECTED_check_status.txt"
    record_expectation "$vb/${label}.img" unverified 0 "corruption attempt failed"
  fi

  # c) single superblock wiped -- should still work
  local vc="$base_destdir/c_single_superblock_wiped"; mkdir -p "$vc"
  cp -a "$pristine" "$vc/${label}.img"
  zero_range "$vc/${label}.img" "$SB_PRIMARY_OFFSET" "$SB_SIZE"
  verify_check_offline "$vc/${label}.img" "$vc/EXPECTED_check_status.txt"
  local rc_c=$?
  echo "EXPECTED: should still succeed via the 64MiB backup superblock." >> "$vc/EXPECTED_check_status.txt"
  if [[ $rc_c -eq 0 ]]; then
    record_expectation "$vc/${label}.img" clean 0 "primary superblock wiped, backup at 64MiB should cover it"
  else
    log "WARN: [c_single_superblock_wiped] check FAILED unexpectedly"
    record_expectation "$vc/${label}.img" unverified 0 "expected clean via backup superblock, check failed"
  fi

  # d) all superblocks wiped -- should fail outright
  local vd="$base_destdir/d_all_superblocks_wiped"; mkdir -p "$vd"
  cp -a "$pristine" "$vd/${label}.img"
  zero_range "$vd/${label}.img" "$SB_PRIMARY_OFFSET" "$SB_SIZE"
  zero_range "$vd/${label}.img" "$SB_BACKUP1_OFFSET" "$SB_SIZE"
  verify_check_offline "$vd/${label}.img" "$vd/EXPECTED_check_status.txt"
  local rc_d=$?
  echo "EXPECTED: should FAIL -- both reachable superblock copies zeroed." >> "$vd/EXPECTED_check_status.txt"
  if [[ $rc_d -ne 0 ]]; then
    record_expectation "$vd/${label}.img" unreadable 1 "both superblock copies zeroed"
  else
    log "WARN: [d_all_superblocks_wiped] check exited 0 unexpectedly"
    record_expectation "$vd/${label}.img" unverified 0 "expected unreadable, check reported clean"
  fi

  # e) truncated image -- should fail / report missing device
  local ve="$base_destdir/e_truncated_image"; mkdir -p "$ve"
  cp -a "$pristine" "$ve/${label}.img"
  local filesize; filesize=$(stat -c %s "$ve/${label}.img")
  truncate -s "$(( filesize * 60 / 100 ))" "$ve/${label}.img"
  verify_check_offline "$ve/${label}.img" "$ve/EXPECTED_check_status.txt"
  local rc_e=$?
  echo "EXPECTED: should FAIL or report a missing/short device -- truncated to 60%." >> "$ve/EXPECTED_check_status.txt"
  if [[ $rc_e -ne 0 ]]; then
    record_expectation "$ve/${label}.img" unreadable 1 "image truncated to 60% of original size"
  else
    log "WARN: [e_truncated_image] check exited 0 unexpectedly"
    record_expectation "$ve/${label}.img" unverified 0 "expected unreadable, check reported clean"
  fi

  cp -a "$pristine" "$base_destdir/pristine_baseline_${label}.img"
  record_manifest "$label" note "5 corrupted variants under 11_corrupted_known_bad/"
}

# ===========================================================================
# Recipe 12: DUP-copy-targeted corruption -- corrupt exactly one mirror vs.
# both, for data AND metadata. This is the direct test for self-heal
# classification and for "is corruption checked against the RIGHT physical
# copy" in general.
# ===========================================================================
recipe_12_dup_copy_targeted_corruption() {
  local base="$OUTDIR/12_dup_copy_targeted"
  mkdir -p "$base"
  local label="btrfs_12"
  local pristine="$WORKDIR/${label}_pristine.img" mnt="$WORKDIR/mnt_${label}"

  btrfs_mkfs "$pristine" "$IMG_SIZE" crc32c 16k dup dup
  btrfs_mount "$pristine" "$mnt"
  dd if=/dev/urandom of="$mnt/dup_target.bin" bs=1M count=2 status=none
  local inode; inode="$(stat -c %i "$mnt/dup_target.bin")"
  sync -f "$mnt" 2>/dev/null || sync
  btrfs_umount "$mnt"
  record_manifest "$label" note "dup/dup image for copy-targeted corruption variants under 12_dup_copy_targeted/"

  if ! command -v btrfs-map-logical >/dev/null 2>&1; then
    log "SKIP recipe 12: btrfs-map-logical not installed"
    for v in a_dup_data_one_copy b_dup_data_both_copies c_dup_meta_one_copy_pinned d_dup_meta_one_copy_global; do
      mkdir -p "$base/$v"; echo "SKIPPED: btrfs-map-logical not found." > "$base/$v/EXPECTED_check_status.txt"
      record_expectation "$base/$v/${label}.img" unverified 0 "btrfs-map-logical unavailable"
    done
    return
  fi

  local data_logical; data_logical="$(find_file_extent_logical_bytenr "$pristine" "$inode")"
  if [[ -z "$data_logical" ]]; then
    log "WARN: recipe 12 could not resolve dup_target.bin's logical bytenr via dump-tree -- data variants (a,b) will be unverified"
  fi

  # a) one DUP data copy corrupted -> a good mirror still exists
  local va="$base/a_dup_data_one_copy"; mkdir -p "$va"
  cp -a "$pristine" "$va/${label}.img"
  if [[ -n "$data_logical" ]] && corrupt_copy "$va/${label}.img" "$data_logical" 1; then
    verify_check_offline "$va/${label}.img" "$va/EXPECTED_check_status.txt" || true
    echo "EXPECTED: exactly one mirror of the logical range at $data_logical is corrupted; the other DUP copy is intact and should be usable to self-heal / recover the correct content." >> "$va/EXPECTED_check_status.txt"
    record_expectation "$va/${label}.img" self_heal_recoverable 1 "one of two DUP data copies corrupted at logical $data_logical"
  else
    record_expectation "$va/${label}.img" unverified 0 "could not resolve/corrupt logical bytenr"
  fi

  # b) BOTH DUP data copies corrupted -> unrecoverable
  local vb="$base/b_dup_data_both_copies"; mkdir -p "$vb"
  cp -a "$pristine" "$vb/${label}.img"
  if [[ -n "$data_logical" ]] && corrupt_copy "$vb/${label}.img" "$data_logical" 1 && corrupt_copy "$vb/${label}.img" "$data_logical" 2; then
    verify_check_offline "$vb/${label}.img" "$vb/EXPECTED_check_status.txt" || true
    echo "EXPECTED: BOTH DUP copies of logical $data_logical corrupted -- no good mirror exists, real unrecoverable data corruption." >> "$vb/EXPECTED_check_status.txt"
    record_expectation "$vb/${label}.img" data_corrupt 1 "both DUP data copies corrupted at logical $data_logical"
  else
    record_expectation "$vb/${label}.img" unverified 0 "could not resolve/corrupt logical bytenr"
  fi

  # c) one DUP copy of a ROOT_TREE metadata leaf corrupted.  We target the
  # ROOT_TREE (not FS_TREE/CSUM_TREE) deliberately: scrub-rs's open-time
  # metadata mirror check walks exactly the chunk tree and root tree, so a
  # corrupted DUP copy of a *root-tree* leaf is the self-heal-recoverable
  # scenario the tool can actually detect and report.  (Corrupting a tree the
  # scrub never walks would be an incoherent test -- it could never pass.)
  local vc="$base/c_dup_meta_one_copy_pinned"; mkdir -p "$vc"
  cp -a "$pristine" "$vc/${label}.img"
  local root_leaf; root_leaf="$(find_tree_leaf_bytenr "$vc/${label}.img" "$TREE_ROOT")"
  if [[ -n "$root_leaf" ]] && corrupt_copy "$vc/${label}.img" "$root_leaf" 1; then
    verify_check_offline "$vc/${label}.img" "$vc/EXPECTED_check_status.txt" || true
    echo "EXPECTED: one DUP copy of ROOT_TREE leaf $root_leaf corrupted. scrub-rs walks the root tree during open() and cross-checks each node's DUP mirrors in lockstep, so it should report this as a self-heal-recoverable mirror mismatch (one copy still csum-valid), never as a clean scrub." >> "$vc/EXPECTED_check_status.txt"
    record_expectation "$vc/${label}.img" self_heal_recoverable 1 "one DUP copy of a ROOT_TREE leaf corrupted, bytenr $root_leaf"
  else
    log "WARN: recipe 12c could not resolve a ROOT_TREE leaf bytenr"
    record_expectation "$vc/${label}.img" unverified 0 "could not resolve ROOT_TREE leaf bytenr"
  fi

  # d) one DUP copy of a CHUNK_TREE metadata leaf corrupted.  Same
  # self-heal-recoverable scenario, exercised on the other tree scrub-rs
  # walks during open() (the chunk tree).  Confirms the mirror check covers
  # both walked trees, not just one.
  local vd="$base/d_dup_meta_one_copy_global"; mkdir -p "$vd"
  cp -a "$pristine" "$vd/${label}.img"
  local chunk_leaf; chunk_leaf="$(find_tree_leaf_bytenr "$vd/${label}.img" "$TREE_CHUNK")"
  if [[ -n "$chunk_leaf" ]] && corrupt_copy "$vd/${label}.img" "$chunk_leaf" 1; then
    verify_check_offline "$vd/${label}.img" "$vd/EXPECTED_check_status.txt" || true
    echo "EXPECTED: one DUP copy of CHUNK_TREE leaf $chunk_leaf corrupted. scrub-rs walks the chunk tree during open() and cross-checks each node's DUP mirrors in lockstep, so it should report this as a self-heal-recoverable mirror mismatch (one copy still csum-valid)." >> "$vd/EXPECTED_check_status.txt"
    record_expectation "$vd/${label}.img" self_heal_recoverable 1 "one DUP copy of a CHUNK_TREE leaf corrupted, bytenr $chunk_leaf"
  else
    log "WARN: recipe 12d could not resolve a CHUNK_TREE leaf bytenr"
    record_expectation "$vd/${label}.img" unverified 0 "could not resolve CHUNK_TREE leaf bytenr"
  fi
}

# ===========================================================================
# Recipe 13: metadata-field and csum-tree anomalies that are NOT plain
# checksum-visible corruption -- edge cases a checksum-only implementation
# might miss entirely, included so that gap is at least documented.
# ===========================================================================
recipe_13_metadata_field_and_csum_anomalies() {
  local base="$OUTDIR/13_metadata_field_and_csum_anomalies"
  mkdir -p "$base"
  local label="btrfs_13"
  local pristine="$WORKDIR/${label}_pristine.img" mnt="$WORKDIR/mnt_${label}"

  btrfs_mkfs "$pristine" "$IMG_SIZE" crc32c 16k single dup
  btrfs_mount "$pristine" "$mnt"
  dd if=/dev/urandom of="$mnt/anomaly_target.bin" bs=1M count=1 status=none
  local inode; inode="$(stat -c %i "$mnt/anomaly_target.bin")"
  sync -f "$mnt" 2>/dev/null || sync
  btrfs_umount "$mnt"

  if ! command -v btrfs-map-logical >/dev/null 2>&1; then
    log "SKIP recipe 13: btrfs-map-logical not installed"
    for v in a_metadata_generation_only b_csum_entry_deleted; do
      mkdir -p "$base/$v"; echo "SKIPPED: btrfs-map-logical not found." > "$base/$v/EXPECTED_check_status.txt"
      record_expectation "$base/$v/${label}.img" unverified 0 "btrfs-map-logical unavailable"
    done
    return
  fi

  # a) corrupt ONLY the generation field of a global-tree (CSUM_TREE) leaf,
  # leaving the checksum untouched relative to the block's OWN content --
  # note this rewrites the header then the corrupt-block tool recomputes
  # and rewrites the block's checksum too as part of the same write, so
  # this is really "the on-disk generation no longer matches what a
  # parent pointer elsewhere expects," a pure staleness/generation-check
  # scenario distinct from a checksum mismatch. A checksum-only verifier
  # cannot see this at all -- documented here so that gap is explicit
  # rather than silently assumed away.
  local va="$base/a_metadata_generation_only"; mkdir -p "$va"
  cp -a "$pristine" "$va/${label}.img"
  local csum_leaf; csum_leaf="$(find_tree_leaf_bytenr "$va/${label}.img" "$TREE_CSUM")"
  if [[ -n "$csum_leaf" ]] && corrupt_metadata_field "$va/${label}.img" "$csum_leaf" generation; then
    verify_check_offline "$va/${label}.img" "$va/EXPECTED_check_status.txt" || true
    echo "EXPECTED: only the generation field of CSUM_TREE leaf $csum_leaf was altered via direct byte flip at header offset 80 (the block's own csum is now invalid, producing a checksum-visible mismatch)." >> "$va/EXPECTED_check_status.txt"
    record_expectation "$va/${label}.img" unverified 0 "generation field byte-flip of a global-tree leaf -- csum mismatch, inspect manually"
  else
    log "WARN: recipe 13a could not resolve a CSUM_TREE leaf bytenr"
    record_expectation "$va/${label}.img" unverified 0 "could not resolve CSUM_TREE leaf bytenr"
  fi

  # b) delete a csum entry for written data, leaving the data itself intact
  local vb="$base/b_csum_entry_deleted"; mkdir -p "$vb"
  cp -a "$pristine" "$vb/${label}.img"
  local data_logical; data_logical="$(find_file_extent_logical_bytenr "$vb/${label}.img" "$inode")"
  if [[ -n "$data_logical" ]] && delete_csum_entry "$vb/${label}.img" "$data_logical"; then
    verify_check_offline "$vb/${label}.img" "$vb/EXPECTED_check_status.txt" || true
    echo "EXPECTED: the csum leaf covering logical $data_logical has its first byte flipped, breaking csum verification for all data blocks whose csums reside in that leaf. This should be detected as a csum mismatch." >> "$vb/EXPECTED_check_status.txt"
    record_expectation "$vb/${label}.img" unverified 0 "csum leaf corrupted for otherwise-correct data -- classification edge case, inspect manually rather than strict pass/fail"
  else
    log "WARN: recipe 13b could not resolve anomaly_target.bin's logical bytenr"
    record_expectation "$vb/${label}.img" unverified 0 "could not resolve logical bytenr"
  fi

  record_manifest "$label" note "metadata-field and csum-tree anomaly variants under 13_metadata_field_and_csum_anomalies/ -- these are deliberately NOT strict pass/fail, see each EXPECTED_check_status.txt"
}

# ===========================================================================
# Recipe 14: size variants -- same baseline-ish population at two sizes far
# from the 512M default, to catch size-dependent chunk-layout assumptions.
# ===========================================================================
recipe_14_size_variants() {
  local label_small="btrfs_14a" label_large="btrfs_14b"
  local img_small="$WORKDIR/${label_small}.img" mnt_small="$WORKDIR/mnt_${label_small}"
  local img_large="$WORKDIR/${label_large}.img" mnt_large="$WORKDIR/mnt_${label_large}"

  btrfs_mkfs "$img_small" "$IMG_SIZE_SMALL" crc32c 16k single dup
  btrfs_mount "$img_small" "$mnt_small"
  populate_tiny "$mnt_small" 100
  populate_large_multiextent "$mnt_small" 2 4
  record_manifest "$label_small" note "small image ($IMG_SIZE_SMALL) -- tight chunk allocation, fewer/smaller chunks than default"
  finalize_fs "$img_small" "$mnt_small" "$OUTDIR/14_size_variants/a_small" "$label_small"

  btrfs_mkfs "$img_large" "$IMG_SIZE_LARGE" crc32c 16k single dup
  btrfs_mount "$img_large" "$mnt_large"
  populate_tiny "$mnt_large" 300
  populate_large_multiextent "$mnt_large" 8 20
  populate_many_entries_dir "$mnt_large" 4000
  record_manifest "$label_large" note "large image ($IMG_SIZE_LARGE) -- more chunks, more DEV_TREE entries to walk than default"
  finalize_fs "$img_large" "$mnt_large" "$OUTDIR/14_size_variants/b_large" "$label_large"
}

# ===========================================================================
# Recipe 15: near-full filesystem -- allocation behavior close to ENOSPC.
# ===========================================================================
recipe_15_near_full_filesystem() {
  local label="btrfs_15"
  local img="$WORKDIR/${label}.img" mnt="$WORKDIR/mnt_${label}"
  btrfs_mkfs "$img" "$IMG_SIZE_NEARFULL" crc32c 16k single dup
  btrfs_mount "$img" "$mnt"
  populate_tiny "$mnt" 100
  fill_to_capacity "$mnt" 92
  record_manifest "$label" note "filled to ~92% of $IMG_SIZE_NEARFULL -- chunk allocation under space pressure"
  finalize_fs "$img" "$mnt" "$OUTDIR/15_near_full_filesystem" "$label"
}

# ===========================================================================
# Recipe 16: feature-flag toggles -- best-effort, since disabling these is
# genuinely version-dependent in btrfs-progs. Each sub-variant is skipped
# with a clear note (not silently dropped) if mkfs rejects the flag.
# ===========================================================================
recipe_16_feature_flag_toggles() {
  local base="$OUTDIR/16_feature_flag_toggles"
  mkdir -p "$base"

  local label_a="btrfs_16a"
  local img_a="$WORKDIR/${label_a}.img" mnt_a="$WORKDIR/mnt_${label_a}"
  if btrfs_mkfs_soft "$img_a" "$IMG_SIZE" crc32c 16k single dup -O ^skinny-metadata; then
    btrfs_mount "$img_a" "$mnt_a"
    populate_tiny "$mnt_a" 100; populate_large_multiextent "$mnt_a" 3 4
    record_manifest "$label_a" note "skinny-metadata explicitly disabled (-O ^skinny-metadata) -- legacy EXTENT_ITEM_KEY path for tree-block extent records instead of METADATA_ITEM_KEY"
    finalize_fs "$img_a" "$mnt_a" "$base/a_no_skinny_metadata" "$label_a"
  else
    log "SKIP recipe 16a: this btrfs-progs build rejected -O ^skinny-metadata (version-dependent)"
    mkdir -p "$base/a_no_skinny_metadata"
    echo "SKIPPED: mkfs.btrfs on this system would not create a filesystem without skinny-metadata." > "$base/a_no_skinny_metadata/EXPECTED_check_status.txt"
  fi

  local label_b="btrfs_16b"
  local img_b="$WORKDIR/${label_b}.img" mnt_b="$WORKDIR/mnt_${label_b}"
  if btrfs_mkfs_soft "$img_b" "$IMG_SIZE" crc32c 16k single dup -O ^free-space-tree; then
    btrfs_mount "$img_b" "$mnt_b"
    populate_tiny "$mnt_b" 100; populate_large_multiextent "$mnt_b" 3 4
    record_manifest "$label_b" note "free-space-tree explicitly disabled (-O ^free-space-tree) -- forces the v1 free-space-cache path (special inode-backed data extents) instead of the FREE_SPACE_TREE"
    finalize_fs "$img_b" "$mnt_b" "$base/b_no_free_space_tree" "$label_b"
  else
    log "SKIP recipe 16b: this btrfs-progs build rejected -O ^free-space-tree (version-dependent)"
    mkdir -p "$base/b_no_free_space_tree"
    echo "SKIPPED: mkfs.btrfs on this system would not create a filesystem without the free-space-tree." > "$base/b_no_free_space_tree/EXPECTED_check_status.txt"
  fi
}

# ===========================================================================
# Recipe 17: nocow file (direct A/B against an equivalent cow file) and
# multiple compression algorithms coexisting in one image.
# ===========================================================================
recipe_17_nocow_and_mixed_compression() {
  local label="btrfs_17"
  local img="$WORKDIR/${label}.img" mnt="$WORKDIR/mnt_${label}"
  btrfs_mkfs "$img" "$IMG_SIZE" crc32c 16k single dup
  btrfs_mount "$img" "$mnt"
  populate_nocow_pair "$mnt"
  populate_compressible "$mnt" zstd compressible_zstd
  populate_compressible "$mnt" lzo compressible_lzo
  populate_compressible "$mnt" zlib compressible_zlib
  populate_compressible "$mnt" no compressible_none
  record_manifest "$label" note "nocow_file.bin (chattr +C) vs cow_file.bin with identical content, plus zstd/lzo/zlib/none compression coexisting in one image"
  finalize_fs "$img" "$mnt" "$OUTDIR/17_nocow_and_mixed_compression" "$label"
}

# ===========================================================================
# Recipe 18: empty and deleted-subvolume edge cases.
# ===========================================================================
recipe_18_subvolume_lifecycle_edge_cases() {
  local label="btrfs_18"
  local img="$WORKDIR/${label}.img" mnt="$WORKDIR/mnt_${label}"
  btrfs_mkfs "$img" "$IMG_SIZE" crc32c 16k single dup
  btrfs_mount "$img" "$mnt"
  btrfs subvolume create "$mnt/empty_subvol" >>"$LOGFILE" 2>&1
  btrfs subvolume create "$mnt/ephemeral_subvol" >>"$LOGFILE" 2>&1
  echo "will be deleted" > "$mnt/ephemeral_subvol/f.txt"
  sync -f "$mnt" 2>/dev/null || sync
  btrfs subvolume delete "$mnt/ephemeral_subvol" >>"$LOGFILE" 2>&1
  sync -f "$mnt" 2>/dev/null || sync
  populate_tiny "$mnt" 50
  record_manifest "$label" note "empty_subvol: created, never written. ephemeral_subvol: created, written, deleted+synced before finalize -- should be fully gone by the time this image is handed off"
  finalize_fs "$img" "$mnt" "$OUTDIR/18_subvolume_lifecycle_edge_cases" "$label"
}

# ===========================================================================
# Recipe 19: partial CSUM_TREE break -- the dangerous undercoverage case.
#
# The DEV_TREE stays fully walkable (so the data-scrub loop runs and
# `sectors checked` is non-zero), but ONE CSUM_TREE leaf (both DUP mirrors)
# has its header checksum broken.  The csum entries living in that leaf
# become unreachable, so scrub-rs silently skips those sectors.  The broken
# leaf must surface as `metadata_header_errors` (hard error, non-zero exit)
# so the undercoverage is NOT mistaken for a clean scrub.
#
# This is the partial-but-non-zero shape (distinct from the all-or-nothing
# DEV_TREE-broken case in 11b): we deliberately populate enough data to
# force a multi-leaf CSUM_TREE, then break a NON-first leaf, so coverage
# stays partial rather than collapsing to `sectors checked: 0`.
# ===========================================================================
recipe_19_csum_tree_partial_break() {
  local base="$OUTDIR/19_csum_tree_partial_break"
  mkdir -p "$base"
  local label="btrfs_19"
  local img="$WORKDIR/${label}.img" mnt="$WORKDIR/mnt_${label}"

  # `dup` metadata so the DUP cross-check is exercised; enough data to force
  # a multi-leaf CSUM_TREE (each leaf holds ~node_size / csum_width sectors
  # of csums; ~300 MiB of data at 4K sectors comfortably spans several
  # leaves).  `single` data keeps the data layout simple for the scrub.
  btrfs_mkfs "$img" "$IMG_SIZE_LARGE" crc32c 16k single dup
  btrfs_mount "$img" "$mnt"
  # ~300 MiB across several files so the CSUM_TREE spans multiple leaves.
  populate_large_multiextent "$mnt" 30 10
  record_manifest "$label" note "large image ($IMG_SIZE_LARGE) with ~300MiB data to force a multi-leaf CSUM_TREE; one non-first csum leaf broken (both DUP mirrors)"
  btrfs_umount "$mnt"

  if ! command -v btrfs-map-logical >/dev/null 2>&1; then
    log "SKIP recipe 19: btrfs-map-logical not installed"
    mkdir -p "$base/a_csum_leaf_broken"
    echo "SKIPPED: btrfs-map-logical not found." > "$base/a_csum_leaf_broken/EXPECTED_check_status.txt"
    record_expectation "$base/a_csum_leaf_broken/${label}.img" unverified 0 "btrfs-map-logical unavailable"
    return
  fi

  # a) one non-first CSUM_TREE leaf broken (both mirrors) -> partial
  # undercoverage that MUST surface as metadata_header_errors (non-zero exit)
  local va="$base/a_csum_leaf_broken"; mkdir -p "$va"
  cp -a "$img" "$va/${label}.img"
  if corrupt_csum_tree_whole "$va/${label}.img"; then
    verify_check_offline "$va/${label}.img" "$va/EXPECTED_check_status.txt" || true
    echo "EXPECTED: a non-first CSUM_TREE leaf (both DUP mirrors) has its header csum broken. The DEV_TREE is intact so the data-scrub loop runs (sectors checked > 0), but the csum entries in the broken leaf are unreachable and silently skipped. scrub-rs MUST report this as metadata_header_errors (>=1) with a non-zero exit, NOT a clean scrub." >> "$va/EXPECTED_check_status.txt"
    record_expectation "$va/${label}.img" meta_corrupt 1 "one non-first CSUM_TREE leaf broken (both DUP mirrors) -- partial undercoverage must surface as metadata_header_errors"
  else
    log "WARN: recipe 19a could not corrupt a CSUM_TREE leaf"
    record_expectation "$va/${label}.img" unverified 0 "could not corrupt CSUM_TREE leaf"
  fi
}

# ===========================================================================
# Main
# ===========================================================================
main() {
  require_root
  require_btrfs
  printf 'label\tfield\tvalue\n' > "$MANIFEST"
  printf 'image_path\texpect_result\texpect_min_mismatch\tdescription\n' > "$EXPECTATIONS"

  log "workdir: $WORKDIR"
  log "outdir:  $OUTDIR"
  if command -v btrfs-map-logical >/dev/null 2>&1; then
    log "btrfs-map-logical found: $(command -v btrfs-map-logical)"
  else
    log "btrfs-map-logical NOT found -- recipes 11b, 12, 13 will be skipped. May need btrfs-progs >= 4.x."
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
  recipe_12_dup_copy_targeted_corruption
  recipe_13_metadata_field_and_csum_anomalies
  recipe_14_size_variants
  recipe_15_near_full_filesystem
  recipe_16_feature_flag_toggles
  recipe_17_nocow_and_mixed_compression
  recipe_18_subvolume_lifecycle_edge_cases
  recipe_19_csum_tree_partial_break

  rm -rf "$WORKDIR"

  log "DONE. Filesystem images and manifests are under: $OUTDIR"
  log "Manifest (human notes):       $MANIFEST"
  log "Expectations (machine-readable, consumed by run_matrix.sh): $EXPECTATIONS"
  log ""
  log "To mount a generated image read-only, e.g.:"
  log "  sudo losetup -f --show $OUTDIR/01_baseline_crc32c/btrfs_01.img"
  log "  sudo mount -o ro,loop $OUTDIR/01_baseline_crc32c/btrfs_01.img /mnt/b"
}

main "$@"
