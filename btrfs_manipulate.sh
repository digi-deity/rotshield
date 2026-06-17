#!/bin/bash
# btrfs_manipulate.sh
#
# Injects a "silent" corruption into a btrfs file to reproduce the exact
# failure mode this project exists to solve:
#
#   btrfs detects the damage via its per-block checksums
#   ↓  but  ↓
#   the NonRAID parity disks are stale and cannot heal it
#
# "Silent" = we write directly to the raw block device, bypassing both
# the btrfs filesystem layer and the NonRAID array layer.  Neither sees
# the write, so neither updates its checksums or parity.

if [ "$EUID" -ne 0 ]; then
  echo "Please run as root (sudo)."
  exit 1
fi

TARGET_FILE="/mnt/disk1/bigfile2.bin"
MOUNT_POINT="/mnt/disk1"

# Helper: hex-dump a 6 KB window centred just before $REAL_PHYS_OFFSET on any
# device.  We show one block of context before the file's start so it's easy
# to see exactly where the file data begins and where the corruption lands.
# Called in Step 3 (before) and Step 5 (after) to keep the repetition DRY.
dump_window() {
  local dev="$1" label="$2"
  # Skip to one block before the file's physical start.  Using 2048-byte
  # blocks means the division is always clean for any btrfs-aligned offset.
  local skip=$(( REAL_PHYS_OFFSET / 2048 - 1 ))
  echo "--- $label ($dev) ---"
  dd if="$dev" bs=2048 skip="$skip" count=3 status=none | hexdump -C
}

# ─────────────────────────────────────────────────────────────────────────────
# Step 1 — Create a recognisable 300 MB test file (all 0xFF)
# ─────────────────────────────────────────────────────────────────────────────
# We fill the file with 0xFF so that any corruption is trivially visible in a
# hex dump: a single 0x00 byte stands out immediately against a wall of FF's.
#
# Size matters: btrfs packs small files into inline extents or mixed metadata
# nodes, which makes the virtual→physical address arithmetic in Step 2 much
# messier.  300 MB forces btrfs to allocate a dedicated data chunk for this
# file, giving us a single clean extent to resolve.
#
# How the pipeline works:
#   /dev/zero produces an infinite stream of 0x00 bytes.
#   `tr '\000' '\377'` flips every 0x00 to 0xFF (octal 377).
#   dd reads 300 MB of that and writes it to the file.
#
# `sync` is essential: it flushes btrfs's in-memory B-trees to disk.  Without
# it the FS_TREE and CHUNK_TREE we query in Step 2 may not yet contain the
# new file's extent records, causing the virtual address lookup to fail.

echo "=== Step 1: Creating 300 MB test file (all 0xFF) ==="
rm -f /mnt/disk1/bigfile*
dd if=/dev/zero bs=1M count=300 status=progress | tr '\000' '\377' > "$TARGET_FILE"
sync

INODE=$(stat -c "%i" "$TARGET_FILE")
echo "Created $TARGET_FILE  (inode $INODE)"


# ─────────────────────────────────────────────────────────────────────────────
# Step 2a — Find the file's btrfs virtual address (FS_TREE)
# ─────────────────────────────────────────────────────────────────────────────
# btrfs does not store file data at raw disk byte offsets.  It uses its own
# internal virtual address space: every data extent is assigned a virtual
# (logical) address, and a separate CHUNK_TREE (Step 2b) translates that
# virtual address to a physical byte offset on a real device.
#
# The FS_TREE is the per-subvolume B-tree that stores inode records and extent
# pointers.  We look for an EXTENT_DATA record keyed at offset 0 for our inode
# — that's the record for the first (and only) extent of the file.  Its "disk
# byte" field is the virtual address we need.

echo ""
echo "=== Step 2a: Looking up btrfs virtual address for inode $INODE (FS_TREE) ==="

# findmnt gives us the block device btrfs is mounted on (e.g. /dev/nmd1p1).
# We use this device for all tree dumps in Steps 2a and 2b.
REAL_DEV=$(findmnt -n -o SOURCE "$MOUNT_POINT")
echo "btrfs device: $REAL_DEV"

VIRT_ADDR=$(btrfs inspect-internal dump-tree -t FS_TREE "$REAL_DEV" | awk -v inode="$INODE" '
  # Wait for the EXTENT_DATA record at file offset 0 for our inode
  $0 ~ "key \\(" inode " EXTENT_DATA 0\\)" { found=1 }
  # The very next "extent data disk byte" line carries the virtual address ($5)
  found && /extent data disk byte/ { print $5; exit }
')

[[ -z "$VIRT_ADDR" || "$VIRT_ADDR" -eq 0 ]] && {
  echo "ERROR: No EXTENT_DATA found for inode $INODE in FS_TREE" >&2; exit 1
}
echo "Virtual address: $VIRT_ADDR"


# ─────────────────────────────────────────────────────────────────────────────
# Step 2b — Map virtual address → physical offset on device (CHUNK_TREE)
# ─────────────────────────────────────────────────────────────────────────────
# The CHUNK_TREE maps virtual address ranges to physical storage locations.
# Each CHUNK_ITEM record covers a contiguous virtual range and says where
# that range physically lives on a specific device.  The output looks like:
#
#   item N key (... CHUNK_ITEM <c_start>)
#       length <c_len>
#       stripe 0 devid <c_devid> offset <c_phys>
#
# We scan chunk items until we find the one whose virtual range
# [c_start, c_start + c_len) contains our VIRT_ADDR, then compute:
#
#   physical byte offset = c_phys + (VIRT_ADDR − c_start)
#
# c_phys is relative to the underlying data partition (e.g. /dev/loop2p1),
# NOT to the NonRAID array device (/dev/nmd1p1).  That distinction is why
# we need to find the real device in Step 2c and write there in Step 4.

echo ""
echo "=== Step 2b: Mapping virtual address to physical offset (CHUNK_TREE) ==="

# Parse all three values in a single awk pass and assign them with `read`
# rather than re-parsing CHUNK_DATA three times with separate subshells.
read -r CHUNK_START PHYS_CHUNK_OFFSET CHUNK_DEVID < <(
  btrfs inspect-internal dump-tree -t CHUNK "$REAL_DEV" | awk -v virt="$VIRT_ADDR" '
    /CHUNK_ITEM/ {
      # The virtual start of this chunk is the number after "CHUNK_ITEM" on
      # the key line, e.g. "key (... CHUNK_ITEM 1234567890)".
      # We use the two-argument POSIX match() + substr() rather than the
      # three-argument gawk extension, so this works under mawk as well.
      match($0, /CHUNK_ITEM [0-9]+/)
      c_start = substr($0, RSTART + 11, RLENGTH - 11) + 0
      c_len   = 0         # reset so a stale value from the previous chunk is not reused
    }
    $1 == "length" { c_len = $2 + 0 }
    /stripe 0 devid/ {
      # $4 = devid, $6 = physical offset.
      # Only emit if we have a complete chunk record AND it contains our address.
      if (c_start > 0 && virt >= c_start && virt < (c_start + c_len)) {
        print c_start, $6, $4
        exit
      }
    }
  '
)

[[ -z "$CHUNK_START" ]] && {
  echo "ERROR: No chunk found that contains virtual address $VIRT_ADDR" >&2; exit 1
}

REAL_PHYS_OFFSET=$(( PHYS_CHUNK_OFFSET + (VIRT_ADDR - CHUNK_START) ))

echo "Chunk virtual start  : $CHUNK_START"
echo "Chunk physical base  : $PHYS_CHUNK_OFFSET"
echo "File physical offset : $REAL_PHYS_OFFSET"


# ─────────────────────────────────────────────────────────────────────────────
# Step 2c — Translate btrfs devid → actual block device path
# ─────────────────────────────────────────────────────────────────────────────
# The CHUNK_TREE identifies devices by btrfs's own internal device ID (devid),
# not by a kernel path.  We need the real path to open and write to the device
# directly in Step 4.
#
# The NonRAID kernel module exposes /proc/nmdstat, which contains lines of the
# form:
#   rdevName.<slot>=<name>
# where <slot> corresponds to the btrfs devid for data disks, and <name> is a
# bare device name without a /dev/ prefix (e.g. "loop2p1").  We normalise it
# to a full path after the lookup.

echo ""
echo "=== Step 2c: Resolving btrfs devid $CHUNK_DEVID → block device path ==="

UNDERLYING_DEV=$(awk -F= -v devid="$CHUNK_DEVID" \
  '$1 == "rdevName." devid { print $2 }' /proc/nmdstat 2>/dev/null)

# nmdstat may store a bare name ("loop2p1") or a full path ("/dev/loop2p1").
# Normalise to a full path so the block-device check and later writes work.
[[ -n "$UNDERLYING_DEV" && "$UNDERLYING_DEV" != /* ]] && UNDERLYING_DEV="/dev/$UNDERLYING_DEV"

[[ -z "$UNDERLYING_DEV" ]] && {
  echo "ERROR: rdevName.$CHUNK_DEVID not found in /proc/nmdstat" >&2; exit 1
}
[[ ! -b "$UNDERLYING_DEV" ]] && {
  echo "ERROR: $UNDERLYING_DEV is not a block device" >&2; exit 1
}

echo "devid $CHUNK_DEVID → $UNDERLYING_DEV"


# ─────────────────────────────────────────────────────────────────────────────
# Step 3 — Sanity check: confirm both devices show 0xFF at the target offset
# ─────────────────────────────────────────────────────────────────────────────
# Before corrupting anything, we read a raw byte window around REAL_PHYS_OFFSET
# from both devices.  Both should show solid 0xFF — the data we wrote in Step 1.
# If either shows anything else, the virtual→physical calculation in Step 2 is
# wrong; stop and debug before proceeding.
#
# We read from both devices to establish a baseline for the comparison in Step 5:
#   Array device ($REAL_DEV):          what btrfs sees when reading the file
#   Underlying partition (UNDERLYING_DEV): the raw on-disk bytes
#
# For a healthy array, normal reads go directly to the underlying partition —
# the parity disks are only consulted when a disk is missing or marked failed.
# So at this point both views should be byte-for-byte identical.

echo ""
echo "=== Step 3: Sanity check — both devices should show 0xFF at offset $REAL_PHYS_OFFSET ==="
dump_window "$REAL_DEV"       "Array device"
dump_window "$UNDERLYING_DEV" "Underlying partition"


# ─────────────────────────────────────────────────────────────────────────────
# Step 4 — Inject the silent corruption
# ─────────────────────────────────────────────────────────────────────────────
# We flip the second byte of the file's data (REAL_PHYS_OFFSET + 1) from
# 0xFF to 0x00.  The write goes directly to the underlying partition, deliberately
# bypassing both layers above it:
#
#   Bypassing btrfs:
#     btrfs already computed and stored a checksum for this 4 KB block based on
#     0xFF.  Changing one byte here invalidates that checksum.  On the next
#     read, btrfs will detect the mismatch and report a checksum error — this
#     is the signal our recovery tool will eventually act on.
#
#   Bypassing the NonRAID array:
#     The parity disks are NOT updated.  They still encode the original 0xFF
#     at this position.  This means they can NOT reconstruct the correct data
#     and cannot be used to heal the corruption through the normal parity path.
#     This "parity-blind" corruption is exactly the scenario this project
#     exists to address.
#
# We target byte +1 (not byte 0) because the very first byte of a btrfs data
# block can coincide with internal block header bytes depending on alignment;
# byte +1 is reliably inside the raw file payload.

echo ""
echo "=== Step 4: Writing 0x00 to $UNDERLYING_DEV at byte offset $((REAL_PHYS_OFFSET + 1)) ==="

printf '\x00' | dd of="$UNDERLYING_DEV" bs=1 \
                   seek="$(( REAL_PHYS_OFFSET + 1 ))" \
                   conv=notrunc status=none

echo "Flipped byte on $UNDERLYING_DEV"
echo "Array device $REAL_DEV was NOT touched — parity remains stale"


# ─────────────────────────────────────────────────────────────────────────────
# Step 5 — Drop page cache and confirm the corruption is visible on disk
# ─────────────────────────────────────────────────────────────────────────────
# Linux caches recently-read disk data in RAM (the page cache).  Without
# flushing it, reads from either device might return the old in-memory bytes
# rather than the now-corrupted on-disk bytes, making the hex dump misleading.
#
# After the cache is cleared, both devices should show 0x00 at position +1
# within the file's extent, with everything around it still 0xFF.  Both agree
# because normal reads through the array device go straight to the underlying
# partition — the same bytes we just corrupted.
#
# At this point the corruption is complete:
#   btrfs        — will report a checksum error on the next read of $TARGET_FILE
#   Parity disks — are stale and cannot reconstruct the correct data

echo ""
echo "=== Step 5: Drop page cache and verify corruption is on disk ==="
sync
echo 3 > /proc/sys/vm/drop_caches

dump_window "$REAL_DEV"       "Array device (post-corruption)"
dump_window "$UNDERLYING_DEV" "Underlying partition (post-corruption)"

echo ""
echo "=== Summary ==="
echo "Corrupted byte  : $UNDERLYING_DEV at offset $(( REAL_PHYS_OFFSET + 1 ))"
echo "Parity state    : stale — cannot heal this"
echo "btrfs           : will report checksum error on next read of $TARGET_FILE"