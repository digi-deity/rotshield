# btrfs-integrity-recovery — unRAID plugin

An unRAID plugin that ships the [`scrub-rs`](../scrub-rs) btrfs scrubber and
`craft-corrupt` test tool, and exposes them through a **Settings page** where
you can run a scrub on demand or on a schedule.

Unlike a typical daemon plugin, this is an **on-demand tool**: there is no
long-running service. The Settings page (Utilities → btrfs-integrity-recovery)
triggers `scripts/scrub.sh`, which runs `scrub-rs` against the configured device
and writes a status file the page polls. An optional schedule is applied via a
`cron.d` entry managed by the `rc` script.

## Layout

```
plugin/
├── btrfs-integrity-recovery.plg          # plugin manifest (installs the bundle)
├── README.md                             # this file
├── source/btrfs-integrity-recovery/      # files packaged into the .txz bundle
│   ├── pkg_build.sh                      # tars the tree -> archive/*.txz
│   ├── install/slack-desc
│   ├── etc/rc.d/rc.btrfs-integrity-recovery   # schedule manager (cron.d)
│   └── usr/local/emhttp/plugins/btrfs-integrity-recovery/
│       ├── btrfs-integrity-recovery.page # menu entry
│       ├── btrfs-integrity-recovery.php  # Settings UI (run / schedule)
│       ├── images/btrfs-integrity-recovery.png
│       ├── scripts/
│       │   ├── install.sh                # .plg install step
│       │   ├── uninstall.sh              # .plg remove step
│       │   └── scrub.sh                  # backend runner (run / status)
│       └── bin/                          # shipped binaries (git-ignored; CI fills)
│           ├── scrub-rs
│           └── craft-corrupt
└── archive/                              # built .txz (git-ignored; produced by CI)
```

The Rust source lives separately under [`../scrub-rs`](../scrub-rs) and is
**not** part of the bundle source tree.

## How the binary gets shipped

We own and build the binary ourselves, so it is **bundled inside the `.txz`**
rather than downloaded at install time (the rathole-unraid reference downloads
a prebuilt binary from upstream releases). The `.plg` install step therefore
only runs `upgradepkg` + `scripts/install.sh`; there is no `curl` to GitHub.

## Build locally

```sh
# 1. Build the static binaries (musl, portable on unRAID's older glibc)
cd scrub-rs
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
cd ..

# 2. Stage them into the plugin tree
BIN=plugin/source/btrfs-integrity-recovery/usr/local/emhttp/plugins/btrfs-integrity-recovery/bin
cp scrub-rs/target/x86_64-unknown-linux-musl/release/scrub-rs    "$BIN"/
cp scrub-rs/target/x86_64-unknown-linux-musl/release/craft-corrupt "$BIN"/
chmod 755 "$BIN"/scrub-rs "$BIN"/craft-corrupt

# 3. Package the bundle
cd plugin/source/btrfs-integrity-recovery
bash pkg_build.sh
# -> ../../../archive/btrfs-integrity-recovery-x86_64-1.txz
```

## Test

- **Rust unit + integration tests**: see
  [`../.github/workflows/scrub-rs-tests.yml`](../.github/workflows/scrub-rs-tests.yml).
  Run locally with `cd scrub-rs && cargo test` (integration tests also need
  root + `btrfs-progs` + loop devices).
- **Plugin packaging**: see
  [`../.github/workflows/build-plugin.yml`](../.github/workflows/build-plugin.yml).
  It builds the musl binaries, runs `cargo test` as a gate, packages the
  bundle, uploads it as an artifact, and (on `v*` tags) publishes it to a
  GitHub Release.

## Install on unRAID

1. Grab `btrfs-integrity-recovery-x86_64-1.txz` from the latest GitHub Release
   (or build it locally per above).
2. Install the plugin via the unRAID webUI (Plugins → Install Plugin, paste the
   raw `.plg` URL), or drop the `.plg` on the flash drive and install it.
3. Open **Settings → btrfs-integrity-recovery**, select the target raw data
   disk(s) (auto-discovered from the array's `/proc/nmdstat`; the partition
   offset and freeze mount are applied automatically), choose recovery mode
   and write vs dry-run, optionally pick a schedule, and click **Run Scrub
   Now**.

## Configuration keys (`/boot/config/plugins/btrfs-integrity-recovery/config.cfg`)

The Settings page writes these INI-style keys; `scripts/scrub.sh` turns them
into `scrub-rs` arguments (see `scrub-rs/src/main.rs` for the full flag list):

| Key | Meaning | scrub-rs flag |
|-----|---------|---------------|
| `DEVICES` | space-separated list of **raw array data-disk rdevs** to scrub **sequentially** (e.g. `/dev/sdX`, `/dev/nvmeXnY`, `/dev/loopX` — whatever `/proc/nmdstat` reports in `rdevName.N` for the data slots; parity slots 0 and 29 are excluded) | `<device>` (one per run) |
| `DEVICE` | first entry of `DEVICES` (kept for backwards compatibility) | `<device>` |
| `WRITE` | `1` writes reconstructed blocks back (`--repair`); `0` (default) dry-run assessment only | `--repair` |
| `NO_FREEZE` | `1` disables freeze (unsafe with repair) | `--no-freeze` |

Recovery assessment is **always on** (free + read-only): every csum mismatch is
reconstructed from parity so the operator learns whether the corruption is
repairable. The `--repair` flag is the only opt-in — it writes the
reconstruction back to the failing disk.

The `--freeze-mount` argument is **auto-detected per device** at run time:
`scrub.sh` looks up each disk's live mountpoint via `findmnt` and passes it to
`scrub-rs`, so there is no manual freeze-mount setting. Freeze only engages
when repairing (`--repair`) on a mounted filesystem; unmounted images are never
frozen.

The partition offset (`--offset`) is **auto-applied per device** from
`/proc/nmdstat`: `scrub.sh` reads `rdevOffset.N` for each target rdev and passes
`--offset +<sectors>` to `scrub-rs`. This is required because the btrfs
superblock lives at that offset on the raw device (not at 0, as it would on the
array partition `/dev/nmdNp1`). There is nothing for the user to set — and no
fallback, because without a valid offset recovery is impossible anyway.
| `BATCH_MAX` | max candidates per recovery batch | `--batch-max` |
| `BATCH_IDLE` | idle-seconds flush threshold | `--batch-idle` |
| `EXTRA_OPTIONS` | appended verbatim (advanced) | — |
| `SCHEDULE` | `disabled` / `weekly` / `monthly` / `custom` | (cron) |
| `CRON` | 5-field cron time spec (`min hour day month weekday`), used only when `SCHEDULE=custom` (e.g. `0 4 1 * *` = 04:00 on the 1st). `daily` is intentionally not offered — a scrub can take days and would overlap its own previous run. | (cron) |

## Releasing

Push a tag `vX.Y.Z`. The `build-plugin.yml` workflow builds, packages, and
attaches `btrfs-integrity-recovery-x86_64-1.txz` to a GitHub Release with
auto-generated release notes. The `.plg`'s `bundleURL` points at
`…/releases/latest/download/…`, so no manifest edit is needed between releases
(only bump `version`/`bundleversion` for the changelog display).
