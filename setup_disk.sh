#!/bin/bash

DISK_COUNT=4
SIZE_MB=256
WORKDIR="/root/nonraid-test"

if [ "$EUID" -ne 0 ]; then
  echo "Please run as root (sudo)"
  exit 1
fi

mkdir -p "$WORKDIR"
cd "$WORKDIR"

# Make sure btrfs tools are available for formatting the data disks
if ! command -v mkfs.btrfs >/dev/null 2>&1; then
    echo "Installing btrfs-progs..."
    apt-get update && apt-get install -y btrfs-progs
fi

for i in $(seq 1 $DISK_COUNT); do
    IMG="d$i"
    LINKNAME="virtdisk-00$i"
    NEW_IMAGE=0

    if [ ! -f "$IMG" ]; then
        echo "Creating $IMG ($SIZE_MB MB)..."
        dd if=/dev/zero of="$IMG" bs=1M count=$SIZE_MB status=progress
        NEW_IMAGE=1
    fi

    LOOP_DEV=$(losetup -j "$IMG" | cut -d: -f1)
    if [ -z "$LOOP_DEV" ]; then
        LOOP_DEV=$(losetup -fP --show "$IMG")
    fi
    echo "Disk $i -> $LOOP_DEV"

    # Create GPT partition table (32K-aligned, single partition to end of disk)
    sgdisk -o -a 8 -n 1:32K:0 "$LOOP_DEV"
    partprobe "$LOOP_DEV" 2>/dev/null
    udevadm settle

    PART_DEV="${LOOP_DEV}p1"

    # Disks 3 and 4 are the data disks; disks 1 and 2 stay zero'd as parity disks.
    if [ "$i" -ge 3 ] && [ "$NEW_IMAGE" -eq 1 ]; then
        echo "Formatting $PART_DEV as btrfs..."
        mkfs.btrfs -f -L "data$((i-2))" "$PART_DEV"
    fi

    # nmdctl requires a /dev/disk/by-id entry for each disk
    ln -sf "$LOOP_DEV" "/dev/disk/by-id/$LINKNAME"
done

udevadm settle

echo "----"
lsblk
echo "----"
ls -l /dev/disk/by-id/ | grep virtdisk
