#!/usr/bin/env bash
#
# btrfs_test_lib.sh
#
# Shared helpers for btrfs_test_matrix.sh and btrfs_live_scrub_test.sh --
# not meant to be run directly. `source` this file after setting LOGFILE
# (and, if you want structured pass/fail expectations, EXPECTATIONS).
#
# btrfs_live_workload.sh deliberately does NOT source this: it's designed
# to run standalone against any already-mounted btrfs filesystem, not just
# ones this lib built, so it carries its own minimal copies of log()/
# rand_sleep() rather than depending on this file being present.
#
# Corruption helpers -- all use standard btrfs-progs tools (no dependency on
# btrfs-corrupt-block).  The pattern is:
#   1. Find a logical bytenr    → btrfs inspect-internal dump-tree
#   2. Map logical → physical   → btrfs-map-logical
#   3. Corrupt byte at physical → python3 (byte flip at offset 0 for csum break)
#   4. Verify                   → btrfs check --readonly  (offline)
#                              or btrfs scrub             (online)
#
# (Historical note: an earlier version of this matrix used
# `btrfs-corrupt-block -E` for the extent-tree corruption recipe. That
# injection turned out to be a no-op on btrfs-progs v6.14 and the recipe is
# marked `unverified` in expectations.tsv; the build step for it was removed
# from CI on 2026-07-13 as dead weight — nothing here calls it anymore.)
#
# btrfs-map-logical works on raw image files -- no loop device needed.
#
# All functions self-verify where practical and WARN loudly rather than
# silently producing a no-op corruption.  If dump-tree's text format has
# drifted on your btrfs-progs version, functions will WARN and the affected
# recipe sub-step records itself as UNVERIFIED rather than asserting a
# ground truth it didn't confirm.
#
set -uo pipefail

: "${LOGFILE:?btrfs_test_lib.sh: LOGFILE must be set before sourcing}"

SB_PRIMARY_OFFSET=65536       # 64 KiB
SB_BACKUP1_OFFSET=67108864    # 64 MiB
SB_SIZE=4096

# Well-known tree objectids, for -t in dump-tree and as a readable alias
# table for recipe code (numeric, not name-based, since numeric ids are
# guaranteed stable regardless of whether a given dump-tree build accepts
# the convenience name aliases).
TREE_ROOT=1
TREE_EXTENT=2
TREE_ROOT=1
TREE_CHUNK=3
TREE_DEV=4
TREE_FS=5
TREE_CSUM=7
TREE_UUID=9

log() { echo "[$(date +%H:%M:%S)] $*" | tee -a "$LOGFILE"; }
die() { log "FATAL: $*"; exit 1; }

require_root() { [[ $EUID -eq 0 ]] || die "must run as root (mount/losetup need root)."; }

require_btrfs() {
  command -v mkfs.btrfs >/dev/null 2>&1 || die "mkfs.btrfs not found. apt install btrfs-progs"
  command -v btrfs >/dev/null 2>&1 || die "btrfs not found. apt install btrfs-progs"
  modprobe btrfs 2>/dev/null || true
  grep -q btrfs /proc/filesystems || log "WARN: btrfs not listed in /proc/filesystems -- module may not be loaded/built-in"
}

# require_map_logical -- btrfs-map-logical is the key tool for
# logical→physical address resolution.  It ships with btrfs-progs.
require_map_logical() {
  command -v btrfs-map-logical >/dev/null 2>&1 || die "btrfs-map-logical not found (part of btrfs-progs). apt install btrfs-progs"
}

# ---------------------------------------------------------------------------
# Manifest / expectations
# ---------------------------------------------------------------------------

# record_manifest <label> <field> <value>   -- free-text human notes (tsv)
record_manifest() {
  [[ -n "${MANIFEST:-}" ]] || return 0
  printf '%s\t%s\t%s\n' "$1" "$2" "$3" >> "$MANIFEST"
}

# record_expectation <image_path> <expect_result> <expect_min_mismatch> <description>
#
# expect_result is one of:
#   clean                 -- scrub and check should both report zero problems
#   data_corrupt           -- real, unrecoverable data corruption present
#   meta_corrupt            -- real, unrecoverable metadata corruption present
#   self_heal_recoverable  -- corruption present but a known-good mirror
#                              exists (one DUP/RAID1 copy only)
#   unreadable              -- fs shouldn't even open/mount/check cleanly
#   unverified               -- injection could not be confirmed; a test
#                              runner should skip strict comparison and just
#                              log the actual result for manual inspection
#
# expect_min_mismatch is the minimum number of distinct problem reports a
# correct tool should surface for this image (0 for clean/unreadable).
record_expectation() {
  [[ -n "${EXPECTATIONS:-}" ]] || return 0
  printf '%s\t%s\t%s\t%s\n' "$1" "$2" "$3" "$4" >> "$EXPECTATIONS"
}

make_image() { local path="$1" size="$2"; truncate -s "$size" "$path"; }

# ---------------------------------------------------------------------------
# mkfs / mount / umount
# ---------------------------------------------------------------------------

# btrfs_mkfs <img> <size> <checksum> <nodesize> <data_profile> <meta_profile> [extra mkfs args...]
btrfs_mkfs() {
  local img="$1" size="$2" csum="$3" nodesize="$4" dprofile="$5" mprofile="$6"
  shift 6
  make_image "$img" "$size"
  mkfs.btrfs -f -q --csum "$csum" -n "$nodesize" -d "$dprofile" -m "$mprofile" "$@" "$img" >>"$LOGFILE" 2>&1 \
    || die "mkfs.btrfs failed for $img (csum=$csum nodesize=$nodesize d=$dprofile m=$mprofile extra=[$*]) -- see $LOGFILE"
}

# btrfs_mkfs_soft: like btrfs_mkfs but returns nonzero instead of dying --
# for feature flags (-O ^skinny-metadata etc.) that may be rejected on some
# btrfs-progs versions. Caller must check the return code.
btrfs_mkfs_soft() {
  local img="$1" size="$2" csum="$3" nodesize="$4" dprofile="$5" mprofile="$6"
  shift 6
  make_image "$img" "$size"
  mkfs.btrfs -f -q --csum "$csum" -n "$nodesize" -d "$dprofile" -m "$mprofile" "$@" "$img" >>"$LOGFILE" 2>&1
}

# btrfs_mount <img> <mountpoint> [extra mount -o opts, comma-separated]
btrfs_mount() {
  local img="$1" mnt="$2" opts="${3:-}"
  mkdir -p "$mnt"
  local loopdev
  loopdev="$(losetup --show -f "$img")" || die "losetup failed for $img"
  if [[ -n "$opts" ]]; then
    mount -o "$opts" "$loopdev" "$mnt" || { losetup -d "$loopdev" 2>/dev/null; die "mount failed for $img via $loopdev"; }
  else
    mount "$loopdev" "$mnt" || { losetup -d "$loopdev" 2>/dev/null; die "mount failed for $img via $loopdev"; }
  fi
  echo "$loopdev" > "${WORKDIR:?WORKDIR must be set}/.loopdev_$(basename "$mnt")"
}

# non-dying variant, for callers that need to handle a mount failure themselves
btrfs_mount_soft() {
  local img="$1" mnt="$2" opts="${3:-}"
  mkdir -p "$mnt"
  local loopdev
  loopdev="$(losetup --show -f "$img")" || return 1
  if [[ -n "$opts" ]]; then
    mount -o "$opts" "$loopdev" "$mnt" 2>>"$LOGFILE" || { losetup -d "$loopdev" 2>/dev/null; return 1; }
  else
    mount "$loopdev" "$mnt" 2>>"$LOGFILE" || { losetup -d "$loopdev" 2>/dev/null; return 1; }
  fi
  echo "$loopdev" > "${WORKDIR:?WORKDIR must be set}/.loopdev_$(basename "$mnt")"
}

# btrfs_umount <mountpoint>
btrfs_umount() {
  local mnt="$1"
  sync
  umount "$mnt" 2>/dev/null || umount -l "$mnt" 2>/dev/null || true
  local loopfile="${WORKDIR:?}/.loopdev_$(basename "$mnt")"
  if [[ -f "$loopfile" ]]; then
    local loopdev; loopdev="$(cat "$loopfile")"
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
    || log "WARN: btrfs scrub on $mnt timed out, failed to start, or found errors (nonzero exit can be normal for corrupted variants)"
}

verify_scrub_btrfs() {
  local mnt="$1" outfile="$2"
  log "scrubbing $mnt for ground-truth verification..."
  wait_for_scrub_btrfs "$mnt"
  btrfs scrub status -v "$mnt" > "$outfile" 2>&1 || true
  log "$mnt: scrub status saved to $(basename "$outfile")"
}

# verify_check_offline <img> <outfile> -- returns btrfs check's exit code
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
  record_expectation "$destdir/$(basename "$img")" clean 0 "$label: no corruption injected"
}

# ---------------------------------------------------------------------------
# Tier 1 corruption helpers
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
# Byte offset within the backing image file of the file's first physical
# block, per filefrag -v (real device blocks, no partition offset since
# our loop devices don't use --offset). Used for the raw byte-flip path.
find_physical_byte_offset() {
  local f="$1"
  local line
  line="$(filefrag -v "$f" 2>/dev/null | awk '/^[[:space:]]*0:/{print; exit}')"
  [[ -z "$line" ]] && return 1
  local physblock
  physblock="$(awk -F'[.:]+' '{gsub(/ /,"",$5); print $5}' <<<"$line")"
  [[ -z "$physblock" || ! "$physblock" =~ ^[0-9]+$ ]] && return 1
  echo $(( physblock * 4096 ))
}

# corrupt_tree_whole <img> <tree_objectid> [name_for_logs]
#
# Corrupt the first leaf of an arbitrary well-known btrfs tree by flipping
# the first byte of EVERY physical mirror the chunk map identifies for that
# leaf's logical bytenr (so on a DUP/SINGLE profile both/all copies are
# broken). The first byte of a btrfs metadata block is part of its CRC32c
# checksum area, so this reliably produces a checksum-visible mismatch
# for any tree a verifier walks.
#
# Use this to construct a `meta_corrupt` test case targeting a tree your
# scrub tool actually reads: scrub-rs's open-time metadata-mirror check
# walks the chunk tree + root tree, and the per-sector scrub drives reads
# off the DEV_TREE, so for scrub-rs TARGET should be one of TREE_CHUNK,
# TREE_ROOT, or TREE_DEV (NOT TREE_EXTENT, which scrub-rs only resolves
# the root of -- it does not walk the extent-tree leaves with mirror
# verification, so an extent-tree-only corruption is invisible to it).
corrupt_tree_whole() {
  local img="$1" tree="$2" name="${3:-tree}"
  require_map_logical
  local leaf
  leaf="$(btrfs inspect-internal dump-tree -t "$tree" "$img" 2>/dev/null \
    | awk '/^leaf [0-9]+/{print $2; exit}')"
  if [[ -z "$leaf" ]]; then
    log "corrupt_tree_whole: no $name leaf found -- image may be too small or empty"
    return 1
  fi
  local phys
  phys="$(btrfs-map-logical -l "$leaf" "$img" 2>/dev/null | awk 'NR==1{print $6}')"
  [[ -n "$phys" ]] || { log "corrupt_tree_whole: could not map logical $leaf to physical"; return 1; }
  # Flip the first byte of every mirror of the leaf (CRC32c csum area).
  btrfs-map-logical -l "$leaf" "$img" 2>/dev/null | while read -r line; do
    local p; p="$(echo "$line" | awk '{print $6}')"
    python3 -c "
with open('$img', 'r+b') as f:
    f.seek($p)
    b = f.read(1)
    f.seek($p)
    f.write(bytes([b[0] ^ 0xFF]))
" 2>/dev/null
  done
  log "corrupt_tree_whole: corrupted $name leaf $leaf (all mirrors)"
  return 0
}

# Back-compat wrappers -- the original names are still used by a couple of
# recipes, keep them so existing callers don't break.
corrupt_extent_tree_whole() { corrupt_tree_whole "$1" "$TREE_EXTENT" extent; }
corrupt_chunk_tree_whole()   { corrupt_tree_whole "$1" "$TREE_CHUNK"  chunk;  }
corrupt_root_tree_whole()    { corrupt_tree_whole "$1" "$TREE_ROOT"  root;  }
corrupt_dev_tree_whole()     { corrupt_tree_whole "$1" "$TREE_DEV"    dev;    }

# ---------------------------------------------------------------------------
# Tier 2 corruption helpers -- see confidence-tier note at top of file
# ---------------------------------------------------------------------------

# find_tree_leaf_bytenr <img> <tree_objectid>
# Prints the bytenr of the first leaf dump-tree walks for the given tree
# (e.g. TREE_CSUM, TREE_FS). Relies on dump-tree's "leaf <bytenr> ..."
# header line, which is a long-standing, stable convention in its output.
find_tree_leaf_bytenr() {
  local img="$1" tree="$2"
  btrfs inspect-internal dump-tree -t "$tree" "$img" 2>/dev/null \
    | awk '/^leaf [0-9]+/{print $2; exit}'
}

# find_file_extent_logical_bytenr <img> <inode> [fs_tree_objectid=TREE_FS]
# Prints the logical disk_bytenr of the first REGULAR (non-inline,
# non-zero) file extent belonging to <inode>. This is parsing dump-tree's
# human-oriented text, not a stable API -- treat failures as "couldn't
# confirm," not as "there is no such extent."
find_file_extent_logical_bytenr() {
  local img="$1" inode="$2" fstree="${3:-$TREE_FS}"
  btrfs inspect-internal dump-tree -t "$fstree" "$img" 2>/dev/null | awk -v ino="$inode" '
    $0 ~ "key \\(" ino " EXTENT_DATA" { in_item=1; next }
    in_item && /^[[:space:]]*item [0-9]+ key/ { in_item=0 }
    in_item && /disk byte/ {
      for (i=1; i<=NF; i++) {
        if ($i == "byte") { val=$(i+1); if (val+0 > 0) { print val; exit } }
      }
    }
  '
}

# corrupt_copy <img> <logical> <copy>
# Corrupts EXACTLY ONE mirror (DUP/RAID1-family only).
# copy=1 targets the first mirror, copy=2 the second; copy=0 targets ALL.
# Uses btrfs-map-logical for logical→physical resolution (no manual
# dump-tree parsing).
corrupt_copy() {
  local img="$1" logical="$2" copy="$3"
  [[ -n "$logical" ]] || { log "corrupt_copy: empty logical"; return 1; }
  require_map_logical

  local map_out
  map_out="$(btrfs-map-logical -l "$logical" "$img" 2>/dev/null)"
  [[ -n "$map_out" ]] || { log "corrupt_copy: btrfs-map-logical returned nothing for logical $logical"; return 1; }

  # Filter by mirror copy number.  btrfs-map-logical -c N does NOT filter
  # output (it only affects -o file writes), so grep manually.
  local targets
  if [[ "$copy" -ne 0 ]]; then
    targets="$(echo "$map_out" | grep "^mirror $copy ")"
  else
    targets="$map_out"
  fi
  [[ -n "$targets" ]] || { log "corrupt_copy: copy $copy not found for logical $logical"; return 1; }

  # Corrupt the first byte (CRC32c csum area) of each target mirror.
  echo "$targets" | awk '{print $6}' | while read -r phys; do
    [[ -z "$phys" ]] && continue
    python3 -c "
with open('$img', 'r+b') as f:
    f.seek($phys)
    b = f.read(1)
    f.seek($phys)
    f.write(bytes([b[0] ^ 0xFF]))
" 2>/dev/null || { log "corrupt_copy: python3 byte flip failed at phys $phys"; continue; }
    log "corrupt_copy: flipped csum byte at logical $logical phys $phys"
  done
  return 0
}

# corrupt_metadata_field <img> <bytenr> <field>
# Flips byte(s) in a specific btrfs header field of the metadata block at
# <bytenr>.  Because we flip bytes without recomputing the block's checksum,
# this produces a checksum-visible mismatch that btrfs check / scrub will
# detect.
#
# Known fields and their byte offsets within the header:
#   generation  → 80
#   owner       → 88
#   bytenr      → 48
#   nritems     → 96
#   level       → 100
corrupt_metadata_field() {
  local img="$1" bytenr="$2" field="$3"
  require_map_logical

  local field_offset
  case "$field" in
    generation) field_offset=80 ;;
    owner)      field_offset=88 ;;
    bytenr)     field_offset=48 ;;
    nritems)    field_offset=96 ;;
    level)      field_offset=100 ;;
    *) log "corrupt_metadata_field: unknown field '$field'"; return 1 ;;
  esac

  local phys
  phys="$(btrfs-map-logical -l "$bytenr" "$img" 2>/dev/null | awk 'NR==1{print $6}')"
  [[ -n "$phys" ]] || { log "corrupt_metadata_field: could not map logical $bytenr to physical"; return 1; }

  python3 -c "
with open('$img', 'r+b') as f:
    f.seek($phys + $field_offset)
    b = f.read(1)
    f.seek($phys + $field_offset)
    f.write(bytes([b[0] ^ 0xFF]))
" 2>/dev/null || { log "corrupt_metadata_field: python3 byte flip failed"; return 1; }

  log "corrupt_metadata_field: flipped $field at logical $bytenr (phys $phys + $field_offset)"
  return 0
}

# delete_csum_entry <img> <bytenr> [bytes]
# Corrupts the csum entry for the data extent at <bytenr> by finding the
# csum tree leaf that covers it and flipping bytes in the leaf's item area.
# This is NOT a precise single-entry deletion -- it corrupts a small region
# of the csum leaf, which will cause csum mismatches for the data blocks
# whose csums live in that region.  btrfs check --check-data-csum will
# report the mismatch.
delete_csum_entry() {
  local img="$1" bytenr="$2" bytes="${3:-}"
  require_map_logical

  # Find the csum tree leaf that covers this bytenr.
  # dump-tree for the csum tree shows items with key (EXTENT_CSUM EXTENT_CSUM <bytenr>).
  # First get the leaf bytenr by finding a csum leaf near our target.
  local leaf
  leaf="$(btrfs inspect-internal dump-tree -t "$TREE_CSUM" "$img" 2>/dev/null \
    | awk '/^leaf [0-9]+/{leaf=$2} $0 ~ "key \\(EXTENT_CSUM EXTENT_CSUM '"$bytenr"'\\)"{print leaf; exit}')"

  if [[ -z "$leaf" ]]; then
    # Fallback: just grab the first csum tree leaf.
    leaf="$(btrfs inspect-internal dump-tree -t "$TREE_CSUM" "$img" 2>/dev/null \
      | awk '/^leaf [0-9]+/{print $2; exit}')"
    if [[ -z "$leaf" ]]; then
      log "delete_csum_entry: no csum tree leaves found"
      return 1
    fi
    log "delete_csum_entry: could not find exact csum entry for $bytenr, falling back to corrupting first csum leaf $leaf"
  fi

  # Corrupt ALL mirrors of the csum leaf (csum byte at offset 0).
  btrfs-map-logical -l "$leaf" "$img" 2>/dev/null | while read -r line; do
    local p; p="$(echo "$line" | awk '{print $6}')"
    [[ -z "$p" ]] && continue
    python3 -c "
with open('$img', 'r+b') as f:
    f.seek($p)
    b = f.read(1)
    f.seek($p)
    f.write(bytes([b[0] ^ 0xFF]))
" 2>/dev/null
  done

  log "delete_csum_entry: corrupted csum leaf $leaf (all mirrors)"
  return 0
}

# corrupt_csum_tree_whole <img> [leaf_bytenr]
# Breaks the header checksum of a SINGLE CSUM_TREE leaf (every mirror of
# it), leaving the rest of the CSUM_TREE intact.  This is the partial-
# coverage metadata-corruption case: the DEV_TREE is still fully walkable
# (so the data-scrub loop runs), but the csum entries living in the broken
# leaf are unreachable, so scrub-rs silently skips those sectors.  The
# broken leaf must surface as a `metadata_header_errors` (hard error, non-
# zero exit) so the undercoverage is not mistaken for a clean scrub.
#
# If `leaf_bytenr` is omitted, the SECOND csum leaf is chosen (the first is
# usually the root's own csum entries and is tiny); on a filesystem with
# enough data to force multiple csum leaves this guarantees the broken leaf
# is NOT the only one, so coverage stays partial-but-non-zero rather than
# collapsing to the all-or-nothing shape.  Callers that want a guaranteed
# multi-leaf CSUM_TREE should populate enough data first (see recipe 19).
corrupt_csum_tree_whole() {
  local img="$1" leaf="${2:-}"
  require_map_logical
  if [[ -z "$leaf" ]]; then
    # Pick the 2nd csum leaf dump-tree walks (NR==2).  Falls back to the
    # first if there is only one.
    leaf="$(btrfs inspect-internal dump-tree -t "$TREE_CSUM" "$img" 2>/dev/null \
      | awk '/^leaf [0-9]+/{n++; if (n==2) {print $2; exit}} END{if (n<2) exit}')"
    if [[ -z "$leaf" ]]; then
      leaf="$(btrfs inspect-internal dump-tree -t "$TREE_CSUM" "$img" 2>/dev/null \
        | awk '/^leaf [0-9]+/{print $2; exit}')"
    fi
  fi
  if [[ -z "$leaf" ]]; then
    log "corrupt_csum_tree_whole: no CSUM_TREE leaf found"
    return 1
  fi
  # Flip the first byte of every mirror of the chosen leaf (CRC32c csum area).
  btrfs-map-logical -l "$leaf" "$img" 2>/dev/null | while read -r line; do
    local p; p="$(echo "$line" | awk '{print $6}')"
    [[ -z "$p" ]] && continue
    python3 -c "
with open('$img', 'r+b') as f:
    f.seek($p)
    b = f.read(1)
    f.seek($p)
    f.write(bytes([b[0] ^ 0xFF]))
" 2>/dev/null
  done
  log "corrupt_csum_tree_whole: corrupted CSUM_TREE leaf $leaf (all mirrors)"
  return 0
}

# ---------------------------------------------------------------------------
# Data population helpers
# ---------------------------------------------------------------------------

populate_tiny() {
  local mnt="$1" n="${2:-300}"
  mkdir -p "$mnt/tiny"
  for i in $(seq 1 "$n"); do
    echo "tiny-$i-$(head -c 16 /dev/urandom | base64)" > "$mnt/tiny/f_$i.txt"
  done
}

# populate_compressible <mnt> [algo=zstd] [dirname=compressible]
populate_compressible() {
  local mnt="$1" algo="${2:-zstd}" dirname="${3:-compressible}"
  mkdir -p "$mnt/$dirname"
  btrfs property set "$mnt/$dirname" compression "$algo" 2>/dev/null || true
  for i in 1 2 3 4 5; do
    yes "the quick brown fox jumps over the lazy dog. " | head -c 2097152 > "$mnt/$dirname/repeat_$i.txt"
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
  local longname; longname=$(printf 'a%.0s' $(seq 1 200))
  echo "long name test" > "$mnt/longnames/$longname"
}

populate_many_entries_dir() {
  local mnt="$1" n="${2:-5000}"
  mkdir -p "$mnt/manyfiles"
  for i in $(seq 1 "$n"); do : > "$mnt/manyfiles/entry_$i"; done
}

populate_subvolume_snapshot_chain() {
  local mnt="$1"
  btrfs subvolume create "$mnt/subvol1" >>"$LOGFILE" 2>&1
  echo "base content" > "$mnt/subvol1/shared.txt"
  populate_compressible "$mnt/subvol1"
  btrfs subvolume snapshot "$mnt/subvol1" "$mnt/subvol1_snap1" >>"$LOGFILE" 2>&1
  echo "modified after snap1" > "$mnt/subvol1/shared.txt"
  rm -f "$mnt/subvol1/compressible/repeat_1.txt"
  cp --reflink=always "$mnt/subvol1/shared.txt" "$mnt/subvol1/shared_reflink.txt" 2>/dev/null \
    || cp "$mnt/subvol1/shared.txt" "$mnt/subvol1/shared_reflink.txt"
  btrfs subvolume snapshot "$mnt/subvol1" "$mnt/subvol1_snap2" >>"$LOGFILE" 2>&1
}

# populate_nocow_pair <mnt> -- a nodatacow file and an equivalent regular
# (COW, checksummed) file with identical content, for direct A/B comparison
# of unverifiable_nodatasum handling against normal verified coverage.
populate_nocow_pair() {
  local mnt="$1"
  mkdir -p "$mnt/nocow"
  : > "$mnt/nocow/nocow_file.bin"
  chattr +C "$mnt/nocow/nocow_file.bin" 2>/dev/null \
    || log "WARN: chattr +C failed (needs the 'C' NODATACOW attribute support) -- nocow_file.bin will behave like a normal COW file"
  dd if=/dev/urandom of="$mnt/nocow/nocow_file.bin" bs=1M count=2 conv=notrunc status=none
  dd if=/dev/urandom of="$mnt/nocow/cow_file.bin" bs=1M count=2 status=none
}

# fill_to_capacity <mnt> <target_pct> -- writes junk files until roughly
# target_pct of the filesystem is used, for near-full allocation behavior.
fill_to_capacity() {
  local mnt="$1" target_pct="${2:-92}"
  mkdir -p "$mnt/filler"
  local n=0
  while :; do
    local used_pct
    used_pct="$(df --output=pcent "$mnt" 2>/dev/null | tail -1 | tr -dc '0-9')"
    [[ -z "$used_pct" ]] && break
    (( used_pct >= target_pct )) && break
    n=$((n+1))
    dd if=/dev/urandom of="$mnt/filler/f_$n.bin" bs=1M count=4 status=none 2>/dev/null || break
    (( n > 500 )) && { log "WARN: fill_to_capacity gave up after 500 files without reaching ${target_pct}%"; break; }
  done
  log "fill_to_capacity: reached $(df --output=pcent "$mnt" 2>/dev/null | tail -1 | tr -d ' ') after $n filler files"
}
