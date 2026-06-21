#!/usr/bin/env bash
set -euo pipefail

# Create files on /mnt/disk1 and inject many corruptions.
#
# Patterns:
# - same-window: all offsets are equally spaced within one scrub window
#   (guarantees multiple corruptions in a single scrub read window).
# - whole-file: offsets are equally spaced across the whole file.

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
# With N corruptions at SECTOR_SPACING bytes apart, we need roughly N * SECTOR_SPACING bytes.
total_span=$((CORRUPTIONS_PER_FILE * SECTOR_SPACING))
# Recovery window is ±64 sectors = ±262144 bytes. Total recoverable = 129 * 4096 = 528384 bytes.
max_recovery_span=$((129 * BTRFS_SECTOR_SIZE))
if [[ "$total_span" -gt "$max_recovery_span" ]]; then
  echo "ERROR: Cannot fit $CORRUPTIONS_PER_FILE corruptions at spacing $SECTOR_SPACING within recovery window (max span: $max_recovery_span bytes)"
  exit 1
fi

echo ">>> [multi] Targeting disk 1 mount only: $MOUNT_POINT"
echo ">>> [multi] files=$FILES_COUNT corruptions_per_file=$CORRUPTIONS_PER_FILE size_mb=$FILE_SIZE_MB"
echo ">>> [multi] pattern=$CORRUPTION_PATTERN sector_spacing=$SECTOR_SPACING (each corruption in different 4KB sector)"
echo ">>> [multi] recovery window: ±64 sectors = ±262144 bytes (~512 KB total)"

for file_idx in $(seq 1 "$FILES_COUNT"); do
  target_file="$MOUNT_POINT/${FILE_PREFIX}_${file_idx}.bin"
  echo
  echo ">>> [multi] Preparing $target_file"

  file_size_bytes=$((FILE_SIZE_MB * 1024 * 1024))
  if [[ "$CORRUPTION_PATTERN" == "same-window" ]]; then
    # Pack all corruptions into a small window (each in its own 4KB sector)
    # such that they all fall within the recovery.py ±64 sector scan window.
    window_size=$((CORRUPTIONS_PER_FILE * SECTOR_SPACING))
    if [[ "$window_size" -ge "$file_size_bytes" ]]; then
      echo "ERROR: window size ($window_size bytes) exceeds file size ($file_size_bytes bytes)"
      exit 1
    fi

    # Pick a deterministic window in the middle of the file.
    window_start=$((file_size_bytes / 2 - window_size / 2))
    if [[ "$window_start" -lt 0 ]]; then
      window_start=0
    fi

    # Each corruption at a sector boundary.
    first_offset=$((window_start + SECTOR_SPACING))
    echo ">>> [multi] window_start=$window_start window_size=$window_size spacing=$SECTOR_SPACING"
  else
    # whole-file: spread across the entire file, maintaining sector spacing.
    spacing=$((file_size_bytes / (CORRUPTIONS_PER_FILE + 1)))
    # Ensure spacing is at least one sector.
    if [[ "$spacing" -lt "$BTRFS_SECTOR_SIZE" ]]; then
      spacing=$BTRFS_SECTOR_SIZE
    fi
    if [[ "$spacing" -lt 1 ]]; then
      echo "ERROR: CORRUPTIONS_PER_FILE too high for file size"
      exit 1
    fi
    first_offset="$spacing"
  fi

  # First corruption creates or refreshes the file.
  echo ">>> [multi] Corrupting offset $first_offset"
  python3 btrfs_manipulate.py \
    "$target_file" \
    --size-mb "$FILE_SIZE_MB" \
    --overwrite \
    --file-offset "$first_offset"

  # Remaining corruptions reuse the same file and place offsets at sector boundaries.
  if [[ "$CORRUPTIONS_PER_FILE" -gt 1 ]]; then
    for corruption_idx in $(seq 2 "$CORRUPTIONS_PER_FILE"); do
      if [[ "$CORRUPTION_PATTERN" == "same-window" ]]; then
        offset=$((window_start + corruption_idx * SECTOR_SPACING))
      else
        # whole-file: use pre-computed spacing from above
        offset=$((corruption_idx * spacing))
      fi

      # Clamp to a valid in-file offset in pathological size/spacing combos.
      if [[ "$offset" -ge "$file_size_bytes" ]]; then
        offset=$((file_size_bytes - 1))
      fi

      echo ">>> [multi] Corrupting offset $offset"
      python3 btrfs_manipulate.py \
        "$target_file" \
        --size-mb "$FILE_SIZE_MB" \
        --file-offset "$offset"
    done
  fi
done

echo
echo ">>> [multi] Completed multi-file, sector-spaced corruption injection on disk 1"
echo ">>> [multi] All corruptions fit within recover.py's ±64 sector recovery window"
