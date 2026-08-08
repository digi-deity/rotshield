# scrub-rs

A standalone, read-only btrfs scrub tool plus a NonRAID/Unraid parity-aware
recovery utility. It reads btrfs on-disk metadata directly from a block device
(or a regular file image) and verifies the checksum of every data sector, then
uses the array's P/Q parity to recover single-disk corruption that btrfs
itself cannot self-heal.

## Layout

```
scrub-rs/
├── Cargo.toml            # crate manifest (lib + scrub-rs + craft-corrupt bins)
├── Cargo.lock            # committed (binary crate)
├── src/                  # Rust source
│   ├── lib.rs            # library crate root (scrub_rs)
│   ├── main.rs           # `scrub-rs` binary (CLI entry point)
│   ├── bin/
│   │   └── craft_corrupt.rs   # `craft-corrupt` binary (injects test corruptions)
│   ├── array/            # parity-array layer — btrfs-agnostic
│   │   ├── config.rs     # /proc/nmdstat parsing; slot/rdev lookups
│   │   ├── parity.rs     # P/Q syndrome computation
│   │   └── stripe.rs     # aligned stripe-chunk reads/writes in raw-rdev space
│   ├── btrfs/            # raw btrfs on-disk parser — array-agnostic
│   │   ├── chunk.rs      # chunk-tree mapping (logical → physical stripes)
│   │   ├── csum.rs       # lazy CSUM_TREE walker (bounded memory)
│   │   ├── csum_strategy.rs  # csum_type → concrete hash (crc32c/xxhash/sha256/blake2)
│   │   ├── dev_extent.rs # dev-tree extent enumeration
│   │   ├── extent.rs     # EXTENT_DATA file extents
│   │   ├── key.rs        # well-known tree objectids + key helpers
│   │   ├── node.rs       # node/leaf parsing
│   │   ├── open.rs       # filesystem open (superblock + backup fallback)
│   │   ├── reader.rs     # pread-based block reader with prefetch hints
│   │   ├── root.rs       # ROOT_ITEM parsing (tree-root resolution)
│   │   ├── scrub.rs      # physical-order data scrub pass
│   │   ├── scrub_driver.rs   # scrub orchestration behind the fs contract
│   │   ├── superblock.rs # superblock parsing
│   │   ├── tree.rs       # iterative tree walks (leaves, key-range pruned)
│   │   └── util.rs       # positioned reads + little-endian helpers
│   ├── recovery/         # parity recovery — GF(2^8) math, cascade, result model
│   │   ├── engine.rs     # I/O-free, checksum-agnostic recovery engine
│   │   ├── gf.rs         # GF(2^8) arithmetic
│   │   └── model.rs      # recovery input / outcome model
│   ├── batch_recover.rs  # batched recovery: dedup → freeze → re-confirm → recover → write-back
│   ├── canary.rs         # startup array probe (parity canary)
│   ├── freeze.rs         # FIFREEZE / FITHAW snapshot-freeze helpers
│   ├── fs.rs             # filesystem seam (open/scrub contract)
│   └── status.rs         # localhost-only HTTP live-status server
├── tests/
│   ├── csum_dedup.rs     # Rust integration tests (CSUM_TREE dedup accounting)
│   └── integration/      # shell-based integration harness (sourced by CI)
│       ├── btrfs_test_lib.sh        # shared mkfs/mount/corruption primitives
│       ├── btrfs_test_matrix.sh     # generate the single-device image matrix
│       ├── run_matrix.sh            # run scrub-rs over the matrix vs expectations.tsv
│       ├── cmp_utilities.sh         # 3-way compare: scrub-rs vs btrfs check vs btrfs scrub
│       ├── btrfs_live_scrub_test.sh # concurrent-churn + live scrub scenarios
│       └── btrfs_live_workload.sh   # standalone write/delete/snapshot churn generator
└── target/               # cargo build output (git-ignored)
```

Unit tests live inline in the source files (`#[cfg(test)]` modules under
`src/array`, `src/recovery`, …) and run with `cargo test`.

## Build

```sh
cargo build --release
# binaries: target/release/scrub-rs, target/release/craft-corrupt
# (craft-corrupt is a test-only corruption injector: it is built for the
#  CI test workflows but is never shipped with the unRAID plugin)
```

## Test

```sh
# Rust unit tests
cargo test

# Integration matrix (requires root + btrfs-progs + loop devices)
sudo tests/integration/btrfs_test_matrix.sh ./btrfs_test_images
sudo -E tests/integration/run_matrix.sh \
    --scrub-cmd "$PWD/target/release/scrub-rs {DEVICE}" ./btrfs_test_images
```

The full CI pipeline (array recovery tests + btrfs matrix + cmpUtilities +
live simulation) is defined at the repo root in
`.github/workflows/scrub-rs-tests.yml`.

## Run output contract

Every run ends by printing a `status:` marker followed by the same
`key=value` payload the `--status-port` live server serves (`state`,
`device`, all counters, `recovery`, `progress_*`). It is emitted
unconditionally — no flag needed — so scripts (e.g. the unRAID plugin's
`status.php`) can read a device's exact final counters from the run log
with the same parser they use for the live endpoint. `state` is `done` for
a completed run (whatever the exit code) or `error` for a run that failed
mid-scrub; the `recovery` flag (1 = parity-recovery pipeline attached,
0 = plain scrub) tells consumers whether `recovered`/`failed`/`skipped` are
meaningful.
