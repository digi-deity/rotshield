#!/bin/bash
# Full teardown for the nonraid virtual disk test setup.
# Removes loop devices, symlinks, and disk image files.

SUPERBLOCK_FILE="/nonraid.dat"   # change if you used -s with a custom path
BYID_PREFIX="virtdisk-"
MOUNT_PREFIX="/mnt/disk"

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

echo "=== 6. Sweeping any leftover loop devices backed by d[0-9]* image files ==="
losetup -a 2>/dev/null | while IFS=: read -r dev _ backing; do
    backing_file=$(echo "$backing" | sed -e 's/^[[:space:]]*(//' -e 's/)[[:space:]]*$//')
    case "$(basename "$backing_file")" in
        d[0-9]*)
            echo "  Detaching leftover $dev (backed by $backing_file)..."
            losetup -d "$dev" 2>/dev/null || echo "    Failed to detach $dev."
            ;;
    esac
done

echo "=== 7. Removing disk image files ==="
IMAGE_DIR="/root/nonraid-test"
for img in "$IMAGE_DIR"/d[0-9]*; do
    [ -e "$img" ] || continue
    rm -f "$img" && echo "  Removed $img."
done
# Remove the directory if empty now
rmdir "$IMAGE_DIR" 2>/dev/null && echo "  Removed empty $IMAGE_DIR." || echo "  $IMAGE_DIR not empty/removable, skipping."

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
