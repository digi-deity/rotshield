#!/usr/bin/env bash
set -euo pipefail

# Create a file on a btrfs mount point and inject equally-spaced corruptions.
#
# Patterns:
# - same-window: all offsets are equally spaced within one scrub window
#   (guarantees multiple corruptions in a single scrub read window).
# - whole-file: offsets are equally spaced across the whole file.
#
# Called once per file; the CI workflow invokes this script in a loop to
# create and corrupt many files that together fill the disk.

MOUNT_POINT="/mnt/disk1"
FILES_COUNT="${FILES_COUNT:-1}"
CORRUPTIONS_PER_FILE="${CORRUPTIONS_PER_FILE:-24}"
FILE_SIZE_MB="${FILE_SIZE_MB:-50}"
FILE_PREFIX="${FILE_PREFIX:-multi_corrupt_file}"
CORRUPTION_PATTERN="${CORRUPTION_PATTERN:-same-window}"
BTRFS_SECTOR_SIZE=4096
# Recovery window in recover.py scans ±64 sectors (±262144 bytes).
# Space corruptions at SECTOR boundaries so each gets its own checksum,
# but keep all within the recovery window (±64 sectors).
SECTOR_SPACING="${SECTOR_SPACING:-$BTRFS_SECTOR_SIZE}"

if [[ ! -d "$MOUNT_POINT" ]]; then
  echo "ERROR: mount point $MOUNT_POINT does not exist"
  exit 1
fi

if ! mountpoint -q "$MOUNT_POINT"; then
  echo "ERROR: $MOUNT_POINT is not mounted"
  exit 1
fi

if [[ "$FILES_COUNT" -lt 1 || "$CORRUPTIONS_PER_FILE" -lt 1 || "$FILE_SIZE_MB" -lt 1 ]]; then
  echo "ERROR: FILES_COUNT, CORRUPTIONS_PER_FILE and FILE_SIZE_MB must be >= 1"
  exit 1
fi

if [[ "$CORRUPTION_PATTERN" != "same-window" && "$CORRUPTION_PATTERN" != "whole-file" ]]; then
  echo "ERROR: CORRUPTION_PATTERN must be one of: same-window, whole-file"
  exit 1
fi

if [[ "$SECTOR_SPACING" -lt $BTRFS_SECTOR_SIZE ]]; then
  echo "ERROR: SECTOR_SPACING ($SECTOR_SPACING) must be >= $BTRFS_SECTOR_SIZE"
  exit 1
fi

# Compute the total span needed for all corruptions.
total_span=$((CORRUPTIONS_PER_FILE * SECTOR_SPACING))
max_recovery_span=$((129 * BTRFS_SECTOR_SIZE))
if [[ "$total_span" -gt "$max_recovery_span" ]]; then
  echo "ERROR: Cannot fit $CORRUPTIONS_PER_FILE corruptions at spacing $SECTOR_SPACING within recovery window (max span: $max_recovery_span bytes)"
  exit 1
fi

echo ">>> [multi] $MOUNT_POINT  files=$FILES_COUNT  corruptions_per_file=$CORRUPTIONS_PER_FILE  size_mb=$FILE_SIZE_MB  pattern=$CORRUPTION_PATTERN"

# Build the corruption offset list once (deterministic, reused per file).
file_size_bytes=$((FILE_SIZE_MB * 1024 * 1024))
declare -a OFFSETS=()
if [[ "$CORRUPTION_PATTERN" == "same-window" ]]; then
  window_size=$((CORRUPTIONS_PER_FILE * SECTOR_SPACING))
  if [[ "$window_size" -ge "$file_size_bytes" ]]; then
    echo "ERROR: window size ($window_size bytes) exceeds file size ($file_size_bytes bytes)"
    exit 1
  fi
  window_start=$((file_size_bytes / 2 - window_size / 2))
  [[ "$window_start" -lt 0 ]] && window_start=0
  for corruption_idx in $(seq 1 "$CORRUPTIONS_PER_FILE"); do
    OFFSETS+=($((window_start + corruption_idx * SECTOR_SPACING)))
  done
else
  spacing=$((file_size_bytes / (CORRUPTIONS_PER_FILE + 1)))
  [[ "$spacing" -lt "$BTRFS_SECTOR_SIZE" ]] && spacing=$BTRFS_SECTOR_SIZE
  if [[ "$spacing" -lt 1 ]]; then
    echo "ERROR: CORRUPTIONS_PER_FILE too high for file size"
    exit 1
  fi
  for corruption_idx in $(seq 1 "$CORRUPTIONS_PER_FILE"); do
    OFFSETS+=($((corruption_idx * spacing)))
  done
fi

for file_idx in $(seq 1 "$FILES_COUNT"); do
  target_file="$MOUNT_POINT/${FILE_PREFIX}_${file_idx}.bin"

  # First corruption creates or refreshes the file.
  first_offset=${OFFSETS[0]}
  echo ">>> [multi] Creating ${FILE_SIZE_MB}MB file, corrupting offset $first_offset"
  python3 btrfs_manipulate.py \
    "$target_file" \
    --size-mb "$FILE_SIZE_MB" \
    --overwrite \
    --file-offset "$first_offset"

  # Remaining corruptions reuse the same file.
  if [[ "$CORRUPTIONS_PER_FILE" -gt 1 ]]; then
    for corruption_idx in $(seq 2 "$CORRUPTIONS_PER_FILE"); do
      offset=${OFFSETS[$((corruption_idx - 1))]}
      [[ "$offset" -ge "$file_size_bytes" ]] && offset=$((file_size_bytes - 1))
      python3 btrfs_manipulate.py \
        "$target_file" \
        --size-mb "$FILE_SIZE_MB" \
        --file-offset "$offset"
    done
  fi
done

echo ">>> [multi] Done: $FILES_COUNT file(s), $CORRUPTIONS_PER_FILE corruptions each"
