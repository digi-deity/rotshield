#!/bin/bash
# Full teardown for the nonraid virtual disk test setup.
# Removes loop devices, symlinks, and disk image files.

SUPERBLOCK_FILE="/nonraid.dat"   # change if you used -s with a custom path
BYID_PREFIX="virtdisk-"
MOUNT_PREFIX="/mnt/disk"
IMAGE_FILES="d_p d_q d_d1 d_d2 d_d3 d_d4"

if [ "$EUID" -ne 0 ]; then
  echo "Please run as root (sudo)"
  exit 1
fi

echo "=== 1. Unmounting array disks ==="
nmdctl -u umount 2>/dev/null && echo "  Unmounted." || echo "  Nothing to unmount."

echo "=== 2. Stopping array ==="
nmdctl -u stop 2>/dev/null && echo "  Array stopped." || echo "  Array was not running."

echo "=== 3. Unloading NonRAID kernel modules (optional) ==="
modprobe -r md_nonraid 2>/dev/null && echo "  md_nonraid unloaded." || echo "  md_nonraid not loaded / still in use, skipping."
modprobe -r nonraid6_pq 2>/dev/null && echo "  nonraid6_pq unloaded." || echo "  nonraid6_pq not loaded / still in use, skipping."

echo "=== 4. Removing superblock file ==="
if [ -f "$SUPERBLOCK_FILE" ]; then
    rm -f "$SUPERBLOCK_FILE" && echo "  Removed $SUPERBLOCK_FILE."
else
    echo "  $SUPERBLOCK_FILE not present, skipping."
fi

echo "=== 5. Detaching loop devices and removing by-id symlinks ==="
for link in /dev/disk/by-id/${BYID_PREFIX}*; do
    [ -e "$link" ] || continue
    target=$(readlink -f "$link")
    echo "  Found $link -> $target"

    if [ -b "$target" ]; then
        losetup -d "$target" 2>/dev/null && echo "    Detached $target." || echo "    Could not detach $target (busy or already gone)."
    fi

    rm -f "$link" && echo "    Removed symlink $link."
done

echo "=== 6. Sweeping any leftover loop devices backed by d_* image files ==="
losetup -l --noheadings -O BACK-FILE,NAME 2>/dev/null | while read -r backing dev; do
    [ -z "$dev" ] && continue
    basename_backing=$(basename "$backing")
    case "$basename_backing" in
        d_*|d[0-9]*)
            echo "  Detaching leftover $dev (backed by $backing)..."
            losetup -d "$dev" 2>/dev/null || echo "    Failed to detach $dev."
            ;;
    esac
done

echo "=== 6b. Cleaning up any remaining partition devices (loop*p1) ==="
for partdev in /dev/loop*p1; do
    [ -b "$partdev" ] || continue
    # Extract parent loop device: /dev/loop0p1 -> /dev/loop0
    parent="${partdev%p1}"
    if [ -b "$parent" ]; then
        echo "  Partition $partdev still present, detaching parent $parent..."
        losetup -d "$parent" 2>/dev/null && echo "    Detached $parent." || echo "    Failed to detach $parent."
    else
        echo "  Orphaned partition $partdev (no parent $parent)."
    fi
done

echo "=== 7. Removing disk image files ==="
for img in $IMAGE_FILES; do
    if [ -e "$img" ]; then
        rm -f "$img" && echo "  Removed $img."
    fi
done

echo "=== 8. Removing empty nmdctl mountpoint directories ==="
for d in ${MOUNT_PREFIX}*; do
    [ -d "$d" ] || continue
    rmdir "$d" 2>/dev/null && echo "  Removed empty $d." || echo "  $d not empty/removable, skipping."
done

udevadm settle 2>/dev/null

echo "=== Teardown complete. Disk images removed. ==="
echo "----"
losetup -a
ls -l /dev/disk/by-id/ 2>/dev/null | grep -i virtdisk || echo "No virtdisk by-id symlinks remain."
cat /proc/nmdstat 2>/dev/null || echo "NonRAID driver not loaded."
