# Unraid / Nonraid btrfs file integrity & recovery

## Project motive

The purpose of this project is to mimic an unraid / nonraid array of disks with parity. Non-parity disks in this array are of a single-disk filesystems, often btrfs. The btrfs filesystem is used to provide data integrity, however in an array like this they lack self-healing capabilities.

The parity disk(s) are for redundancy, but they cannot be used to heal single filesystem disks. Instead, they can only be used to recover entire disks. This project is an attempt to read the underlying blockdevice, circumventing the filesystem, and use the parity disk(s) to recover data for which btrfs filesystem is reporting errors.

## Environment

We are in a safe virtualized environment, so we can experiment with the block devices without risking data loss. The block devices are represented as files in the filesystem, and we can read and write to them as if they were actual disks.

## Architecture

The array consists of 4 virtual disks backed by loop-device image files in `/root/nonraid-test/`:

| Disk | Image file | Role | Filesystem |
|------|-----------|------|------------|
| d1   | `nonraid-test/d1` | Parity P | Raw (zeroed) |
| d2   | `nonraid-test/d2` | Parity Q | Raw (zeroed) |
| d3   | `nonraid-test/d3` | Data disk 1 | btrfs |
| d4   | `nonraid-test/d4` | Data disk 2 | btrfs |

Each image is 1 GB. Loop devices are created with a GPT partition table (32K-aligned, single partition). The `nmdctl` tool manages the array and requires `/dev/disk/by-id/virtdisk-00X` symlinks pointing to each loop device.

The merged array device appears as `/dev/nmd1` with partition `/dev/nmd1p1`, which is mounted at `/mnt/disk1`.

## Scripts

### `nmdctl` — NonRAID array control tool (binary, at repo root)
The array management CLI. Used directly by CI and the scripts below. Requires
`/dev/disk/by-id/virtdisk-00X` symlinks pointing to each loop device. Key
subcommands: `create --force`, `start`, `check`, `mount`, `status`, `umount`,
`stop`.

### `mk_array.sh` — Assemble and start the NonRAID array (at repo root)
- Uses `nmdctl create --force` with disk assignments: `P:/dev/loop0p1`, `Q:/dev/loop1p1`, `1:/dev/loop2p1`, `2:/dev/loop3p1`.
- Starts the array with `nmdctl start`.
- Verifies with `nmdctl check NOCORRECT`.
- This is the manual counterpart to the array job in `.github/workflows/scrub-rs-tests.yml` (which inlines the same `nmdctl` calls).

### `teardown_disk.sh` — Full teardown (idempotent, safe to re-run, at repo root)
- Unmounts array disks (`nmdctl -u umount`).
- Stops the array (`nmdctl -u stop`).
- Unloads NonRAID kernel modules (`md_nonraid`, `nonraid6_pq`).
- Removes the superblock file (`/nonraid.dat`).
- Detaches all loop devices backed by `d[0-9]*` image files and removes their by-id symlinks.
- Cleans up empty mount-point directories under `/mnt/disk*`.
- **Preserves** the image files in `nonraid-test/` for reuse.

### `scrub-rs/` — Rust scrub + recovery tool (the main deliverable)
A Cargo crate providing a read-only btrfs scrubber and parity-aware recovery
utility. Build with `cargo build --release` (binaries: `scrub-rs`,
`craft-corrupt`). Unit tests live inline in `src/` and run with `cargo test`.

### `scrub-rs/tests/integration/` — Shell-based integration harness
Sourced by CI (`.github/workflows/scrub-rs-tests.yml`):
- `btrfs_test_lib.sh` — shared mkfs/mount/corruption primitives (sourced by the others).
- `btrfs_test_matrix.sh` — generates the single-device btrfs image matrix + `expectations.tsv`.
- `run_matrix.sh` — runs `scrub-rs` over the matrix and compares to `expectations.tsv`.
- `cmp_utilities.sh` — 3-way compare of `scrub-rs` vs `btrfs check` vs `btrfs scrub`.
- `btrfs_live_scrub_test.sh` / `btrfs_live_workload.sh` — concurrent-churn + live-scrub scenarios.

> Note: the original Python implementation (`setup_disk.sh`, `btrfs_manipulate.sh`,
> and the `btrfs-recon` Python package) was ported to Rust and the Python sources
> have been removed. The Rust equivalents are `scrub-rs` + `craft-corrupt` and the
> `tests/integration/` harness above.

## Key concepts

- **Device layering (bottom → top)**: There are four layers of devices to keep track of:
  1. **Raw image files** (`nonraid-test/d1`–`d4`) attached as **loop devices** (`/dev/loop0`–`/dev/loop3`). These are the bare 1 GB backing stores.
  2. **GPT partitions** (`/dev/loop0p1`–`/dev/loop3p1`). Each loop device has a GPT table with a single 32K-aligned partition. The actual filesystem (btrfs or raw parity) lives on the partition, not the whole loop device.
  3. **NonRAID array device** (`/dev/nmd1` → `/dev/nmd1p1`). The `nmdctl` tool combines the partition devices into a merged array with parity protection. Every write to the array device is reflected in the parity disks in real time. The array device is what gets mounted.
  4. **Mounted filesystem** (`/mnt/disk1`). The btrfs filesystem on the data disk partition is mounted here. All normal file I/O goes through this mount point — writes propagate down through the array layer to update parity, and reads are served from the underlying data disk.
- **BTRFS virtual → physical mapping**: BTRFS uses its own virtual address space. The `FS_TREE` gives the virtual extent offset; the `CHUNK_TREE` maps virtual ranges to physical device offsets. Both must be traversed to find where file data actually lives on disk. **Important**: The physical offset reported by the chunk tree is relative to the *data disk's partition* (e.g., `/dev/loop2p1` for d3), not the array device. When reading/writing through the array device (`/dev/nmd1p1`), the physical offset of the data on the underlying partition may differ due to array-level striping or offsetting — you must account for which device is being accessed and any offset implied by the array layout.
- **Parity-based recovery**: The NonRAID array stores XOR (and possibly Reed-Solomon) parity on dedicated disks. Parity can reconstruct an entire missing disk but cannot heal individual corrupted files within a btrfs filesystem — that's the gap this project aims to address.
- **Direct block-device writes**: Writing directly to the underlying block device (e.g., `/dev/loop2p1`) bypasses the filesystem and nonraid array entirely. This is how we simulate silent corruption in a single file which invalidates the btrfs checksum and keeps the parity drive intact — the array layer never sees the write, so parity is not updated.