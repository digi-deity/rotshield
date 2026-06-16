#!/bin/bash

# Ensure the script is run as root
if [ "$EUID" -ne 0 ]; then
  echo "Please run as root (sudo)."
  exit 1
fi

TARGET_FILE="/mnt/disk1/bigfile2.bin"
MOUNT_POINT="/mnt/disk1"

echo "=== Step 1: Creating a 300MB file filled with FF bytes ==="
dd if=/dev/zero bs=1M count=300 status=none | tr '\000' '\377' > "$TARGET_FILE"
sync # Force BTRFS to commit file metadata and data to disk

# Get the inode number
INODE=$(stat -c "%i" "$TARGET_FILE")
echo "File created at $TARGET_FILE (Inode: $INODE)"

echo -e "\n=== Step 2: Resolving BTRFS Physical Offset via Tree Dump ==="
REAL_DEV=$(findmnt -n -o SOURCE "$MOUNT_POINT")
echo "Active BTRFS Block Device: $REAL_DEV"

# Extract the virtual address from the FS_TREE
VIRT_ADDR=$(btrfs inspect-internal dump-tree -t FS_TREE "$REAL_DEV" | \
            awk -v inode="$INODE" '
            $0 ~ "key \\(" inode " EXTENT_DATA 0\\)" {found=1} 
            found && /extent data disk byte/ {print $5; exit}
            ')

if [ -z "$VIRT_ADDR" ] || [ "$VIRT_ADDR" -eq 0 ]; then
    echo "Error: Could not resolve BTRFS virtual address."
    exit 1
fi
echo "BTRFS Virtual Address space: $VIRT_ADDR"

# Dump the CHUNK tree and parse out the correct mapping
CHUNK_DATA=$(btrfs inspect-internal dump-tree -t CHUNK "$REAL_DEV" | \
awk -v virt="$VIRT_ADDR" '
    /CHUNK_ITEM/ {
        match($0, /CHUNK_ITEM [0-9]+/);
        if (RSTART > 0) {
            c_start = substr($0, RSTART + 11, RLENGTH - 11) + 0;
        }
    }
    /length/ && c_start != "" {
        c_len = $2 + 0;
    }
    /stripe 0 devid/ && c_start != "" {
        c_phys = $6 + 0;
        if (virt >= c_start && virt < (c_start + c_len)) {
            print c_start, c_phys;
            exit;
        }
    }
')

CHUNK_START=$(echo "$CHUNK_DATA" | awk '{print $1}')
PHYS_CHUNK_OFFSET=$(echo "$CHUNK_DATA" | awk '{print $2}')

if [ -z "$CHUNK_START" ] || [ -z "$PHYS_CHUNK_OFFSET" ]; then
    echo "Error: Could not map virtual address to an enclosing chunk."
    exit 1
fi

# Calculate exact relative physical byte position on the device
REAL_PHYS_OFFSET=$(( PHYS_CHUNK_OFFSET + (VIRT_ADDR - CHUNK_START) ))

echo "Enclosing Chunk Starts At : $CHUNK_START"
echo "Chunk Physical Base Offset: $PHYS_CHUNK_OFFSET"
echo "Exact physical byte offset of file on device: $REAL_PHYS_OFFSET"

echo -e "\n=== Step 3: Printing first 1024 bytes from physical disk ==="
# Convert to 2048-byte sector blocks to guarantee clean mathematical alignment
SKIP_BLOCKS=$(( REAL_PHYS_OFFSET / 2048 ))
cat "$REAL_DEV" | dd bs=2048 skip="$SKIP_BLOCKS" count=2 status=none | head -c 1024 | hexdump -C

echo -e "\n=== Step 4: Flipping the second byte to 00 directly on the raw disk ==="
# Second byte means byte offset = REAL_PHYS_OFFSET + 1
TARGET_BYTE_OFFSET=$(( REAL_PHYS_OFFSET + 1 ))

# Overwrite exactly one byte on the raw array device
echo -ne '\x00' | dd of="$REAL_DEV" bs=1 seek="$TARGET_BYTE_OFFSET" conv=notrunc status=none
echo "Injected 0x00 into array byte location: $TARGET_BYTE_OFFSET"

echo -e "\n=== Step 5: Clearing OS Page Cache & verifying change from the file ==="
sync
echo 3 > /proc/sys/vm/drop_caches

# Print the beginning of the file to see the flipped byte at index 00000001
sudo cat /dev/nmd1p1 | dd bs=2048 skip=$(( 565575680 / 2048 )) count=1 status=none | head -c 16 | hexdump -C
