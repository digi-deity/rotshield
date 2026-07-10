echo "Creating asymmetric test disk images..."
echo ""
echo "Layout (disk size, partition offset, usable size):"
echo "  P    : 512MB  off 32K  -> ~512MB usable  (parity)"
echo "  Q    : 512MB  off 32K  -> ~512MB usable  (parity)"
echo "  disk1: 512MB  off 32K  -> ~512MB usable  (biggest data)"
echo "  disk2: 448MB  off 32M  -> ~416MB usable"
echo "  disk3: 384MB  off 64M  -> ~320MB usable"
echo "  disk4: 320MB  off 96M  -> ~224MB usable  (smallest)"
echo ""

sudo dd if=/dev/zero of=d_p  bs=1M count=512 status=progress
sudo dd if=/dev/zero of=d_q  bs=1M count=512 status=progress
sudo dd if=/dev/zero of=d_d1 bs=1M count=512 status=progress
sudo dd if=/dev/zero of=d_d2 bs=1M count=448 status=progress
sudo dd if=/dev/zero of=d_d3 bs=1M count=384 status=progress
sudo dd if=/dev/zero of=d_d4 bs=1M count=320 status=progress

disk_p=$(sudo losetup -fP --show d_p)
disk_q=$(sudo losetup -fP --show d_q)
disk1=$(sudo losetup -fP --show d_d1)
disk2=$(sudo losetup -fP --show d_d2)
disk3=$(sudo losetup -fP --show d_d3)
disk4=$(sudo losetup -fP --show d_d4)

echo "DISK_P=$disk_p" >> $GITHUB_ENV
echo "DISK_Q=$disk_q" >> $GITHUB_ENV
echo "DISK1=$disk1"   >> $GITHUB_ENV
echo "DISK2=$disk2"   >> $GITHUB_ENV
echo "DISK3=$disk3"   >> $GITHUB_ENV
echo "DISK4=$disk4"   >> $GITHUB_ENV

echo "Created loop devices: P=$disk_p Q=$disk_q 1=$disk1 2=$disk2 3=$disk3 4=$disk4"

# ── Create partitions with ascending offsets ────────────────────
# P, Q and disk1 use a standard 32K start and run to disk end.
# Data disks use larger partition offsets (32M, 64M, 96M) and the
# partition runs to disk end. This gives every data disk a distinct
# rdevOffset and a distinct rdevSize.
sudo sgdisk -o -a 8 -n 1:32K:0 $disk_p
sudo sgdisk -o -a 8 -n 1:32K:0 $disk_q
sudo sgdisk -o -a 8 -n 1:32K:0 $disk1
sudo sgdisk -o -a 8 -n 1:32M:0 $disk2
sudo sgdisk -o -a 8 -n 1:64M:0 $disk3
sudo sgdisk -o -a 8 -n 1:96M:0 $disk4

# ── Create symlinks for device discovery ─────────────────────────
sudo ln -s $disk_p /dev/disk/by-id/virtdisk-001
sudo ln -s $disk_q /dev/disk/by-id/virtdisk-002
sudo ln -s $disk1  /dev/disk/by-id/virtdisk-003
sudo ln -s $disk2  /dev/disk/by-id/virtdisk-004
sudo ln -s $disk3  /dev/disk/by-id/virtdisk-005
sudo ln -s $disk4  /dev/disk/by-id/virtdisk-006

# ── Make btrfs filesystems on each data disk ─────────────────────
sudo mkfs.btrfs -f -L "data1" "${disk1}p1"
sudo mkfs.btrfs -f -L "data2" "${disk2}p1"
sudo mkfs.btrfs -f -L "data3" "${disk3}p1"
sudo mkfs.btrfs -f -L "data4" "${disk4}p1"

        # ── Configure mount options ──────────────────────────────────────
# Compression on disk3 for variety (tests compressed-extent recovery
# on a disk with a non-standard partition offset).
sudo mkdir -p /etc/nonraid
echo "/dev/nmd1p1 /mnt/disk1 btrfs defaults                 0 2" | sudo tee -a /etc/nonraid/fstab
echo "/dev/nmd2p1 /mnt/disk2 btrfs defaults                 0 2" | sudo tee -a /etc/nonraid/fstab
echo "/dev/nmd3p1 /mnt/disk3 btrfs defaults,compress=zstd   0 2" | sudo tee -a /etc/nonraid/fstab
echo "/dev/nmd4p1 /mnt/disk4 btrfs defaults                 0 2" | sudo tee -a /etc/nonraid/fstab

echo "Test environment ready"

- name: Create, start and mount the NonRAID array
run: |
echo ">>> Creating NonRAID array with asymmetric disk assignments"
echo "    P: /dev/loop0, virtdisk-001, offset 64 sectors (32K)"
echo "    Q: /dev/loop1, virtdisk-002, offset 64 sectors (32K)"
echo "    1: /dev/loop2, virtdisk-003, offset 64 sectors (32K)"
echo "    2: /dev/loop3, virtdisk-004, offset 65536 sectors (32M)"
echo "    3: /dev/loop4, virtdisk-005, offset 131072 sectors (64M)"
echo "    4: /dev/loop5, virtdisk-006, offset 196608 sectors (96M)"
sudo ./nmdctl create --force \
P:/dev/loop0:virtdisk-001:64 \
Q:/dev/loop1:virtdisk-002:64 \
1:/dev/loop2:virtdisk-003:64 \
2:/dev/loop3:virtdisk-004:65536 \
3:/dev/loop4:virtdisk-005:131072 \
4:/dev/loop5:virtdisk-006:196608

echo ">>> [mk_array] Starting the NonRAID array"
echo 'y' | sudo nmdctl start
sleep 1

echo ">>> [mk_array] Running array check"
echo 'y' | sudo nmdctl check
sleep 1

echo ">>> [mk_array] Mounting array disks"
sudo nmdctl mount
sleep 1

echo ">>> [mk_array] Checking array status"
sudo nmdctl status

echo ">>> [mk_array] Verifying per-disk sizes and offsets from /proc/nmdstat"
grep -E '^rdevOffset\.[0-9]+=' /proc/nmdstat | sort -t. -k2 -n
grep -E '^rdevSize\.[0-9]+='   /proc/nmdstat | sort -t. -k2 -n
grep -E '^diskSize\.[0-9]+='   /proc/nmdstat | sort -t. -k2 -n

echo ">>> [mk_array] Array setup complete"