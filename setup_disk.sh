#!/bin/bash

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

# Create test disk images
sudo dd if=/dev/urandom of=d1 bs=1M count=256 status=progress
sudo dd if=/dev/urandom of=d2 bs=1M count=256 status=progress
sudo dd if=/dev/urandom of=d3 bs=1M count=256 status=progress
sudo dd if=/dev/urandom of=d4 bs=1M count=128 status=progress

# Set up loop devices
disk1=$(sudo losetup -fP --show d1)
disk2=$(sudo losetup -fP --show d2)
disk3=$(sudo losetup -fP --show d3)
disk4=$(sudo losetup -fP --show d4)

echo "Created loop devices: $disk1, $disk2, $disk3, $disk4"

# Create partitions
sudo sgdisk -o -a 8 -n 1:32K:0 $disk1
sudo sgdisk -o -a 8 -n 1:32K:0 $disk2
sudo sgdisk -o -a 8 -n 1:32K:0 $disk3
sudo sgdisk -o -a 8 -n 1:32K:0 $disk4

sudo ln -s $disk1 /dev/disk/by-id/virtdisk-001
sudo ln -s $disk2 /dev/disk/by-id/virtdisk-002
sudo ln -s $disk3 /dev/disk/by-id/virtdisk-003
sudo ln -s $disk4 /dev/disk/by-id/virtdisk-004

sudo mkfs.btrfs -f -L "data3" "${disk3}p1"
sudo mkfs.btrfs -f -L "data4" "${disk4}p1"