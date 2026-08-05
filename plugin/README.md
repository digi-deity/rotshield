# btrfs-integrity-recovery — unRAID plugin

> This document covers the unRAID plugin half of the project. For the
> project overview, the `scrub-rs` tool, and the integration-test harness,
> start at the repository root: [`../README.md`](../README.md).

An unRAID plugin that ships the [`scrub-rs`](../scrub-rs) btrfs scrubber and
`craft-corrupt` test tool, and exposes them through a **Settings page** where
you can run a scrub on demand or on a schedule. The binaries are always
bundled inside this plugin — there is no separate distribution.

Unlike a typical daemon plugin, this is an **on-demand tool**: there is no
long-running service. The Settings page (Utilities → btrfs-integrity-recovery)
triggers `scripts/scrub.sh`, which runs `scrub-rs` against the configured device
and writes a per-run log the page can show. An optional schedule is applied via
a `cron.d` entry managed by the `rc` script.

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
│       ├── btrfs-integrity-recovery.page # menu entry + Settings UI
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
# 1. Build the binaries (gnu target, matching unRAID's glibc runtime and the
#    integration-test binaries — see build-plugin.yml)
cd scrub-rs
cargo build --release
cd ..

# 2. Stage them into the plugin tree
BIN=plugin/source/btrfs-integrity-recovery/usr/local/emhttp/plugins/btrfs-integrity-recovery/bin
cp scrub-rs/target/release/scrub-rs    "$BIN"/
cp scrub-rs/target/release/craft-corrupt "$BIN"/
chmod 755 "$BIN"/scrub-rs "$BIN"/craft-corrupt

# 3. Package the bundle
cd plugin/source/btrfs-integrity-recovery
bash pkg_build.sh
# -> ../../../archive/btrfs-integrity-recovery-<version>-x86_64-1.txz
#    (<version> is read from the .plg's <!ENTITY version>)
```

## Test

- **Rust unit + integration tests**: see
  [`../.github/workflows/scrub-rs-tests.yml`](../.github/workflows/scrub-rs-tests.yml).
  Run locally with `cd scrub-rs && cargo test` (integration tests also need
  root + `btrfs-progs` + loop devices).
- **Plugin packaging**: see
  [`../.github/workflows/build-plugin.yml`](../.github/workflows/build-plugin.yml).
  It builds the gnu binaries, runs `cargo test` as a gate, packages the
  bundle, uploads it as an artifact, and (on `v*` tags) publishes it to a
  GitHub Release.

## Install on unRAID

1. Grab `btrfs-integrity-recovery-<version>-x86_64-1.txz` (the version
   matching your `.plg`) from the matching GitHub Release (or build it
   locally per above).
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
| `DEVICES` | space-separated list of **raw array data-disk rdevs** to scrub **sequentially** (e.g. `/dev/sdX`, `/dev/nvmeXnY`, `/dev/loopX` — whatever `/proc/nmdstat` reports in `rdevName.N` for the data slots; parity slots 0 and 29 are excluded). Scrub order = the listed order (config is the single source of truth); the Settings page stores them alphabetically. **No target is preselected on a fresh install** (empty list = the table stays empty and scrubbing is skipped until you pick a disk) | `<device>` (one per run) |
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
| `STATUS_PORT` | localhost HTTP port for the live status endpoint (`0` disables; default `9101`) | `--status-port` |
| `EXTRA_OPTIONS` | appended verbatim (advanced) | — |
| `SCHEDULE` | `disabled` / `weekly` / `monthly` / `custom` | (cron) |
| `CRON` | 5-field cron time spec (`min hour day month weekday`), used only when `SCHEDULE=custom` (e.g. `0 4 1 * *` = 04:00 on the 1st). `daily` is intentionally not offered — a scrub can take days and would overlap its own previous run. | (cron) |

### Live status + per-disk progress table

While a scrub runs, scrub-rs serves the live error counters over a
localhost-only HTTP endpoint so the Settings page shows real-time numbers
instead of just "running / idle". This is on by default (`STATUS_PORT=9101`)
and binds to `127.0.0.1` only — never the network. `scrub.sh status` prints
the payload, and the Settings page polls it every 5 s. The page renders the
counters as a **progress table** (rows = statistics grouped by hierarchy,
columns = disks): the disk currently being scrubbed gets live values, and
each finished disk keeps its exact final counters.

```
$ curl -s http://127.0.0.1:9101/status
state=running
device=/dev/nmd1p1
sectors_checked=123456
sectors_ok=123455
sectors_mismatch=1
sectors_stale=0
sectors_no_csum=0
sectors_read_error=0
bytes_checked=505937920
metadata_header_errors=0
metadata_mirror_mismatches=0
metadata_read_errors=0
recovered=1
failed=0
skipped=0
recovery=1
progress_total=1073741824
progress_done=268435456
progress_pct=25.00
```

`progress_pct` is a coarse scrub-completion percentage emitted as a float
with two decimal places (0.00–100.00, monotonic non-decreasing): the numerator
is the physical length of data dev-extents already fully scrubbed (the scrub is
a single front-to-back pass over the DEV_TREE, so each completed extent advances
it), the denominator is the total length of all DATA dev-extents, known up front
with no scan. It's coarse (extent-granular, ~1 GiB steps on a default
filesystem), which is plenty on real disks (1 TiB → ~1000 steps → 0.1%
resolution) and only visibly chunky on tiny test images.

`key=value` lines, shell-parseable (e.g. `curl -s …/status | awk -F= '$1=="recovered"{print $2}'`).
Set `STATUS_PORT=0` to disable. A busy port is logged and skipped by scrub-rs,
never fatal.

**Final counters survive the process**: every scrub-rs run ends by printing a
`status:` marker followed by the same `key=value` payload to stdout, so the
run log (written by `scrub.sh` under `…/runs/run-*.log`) carries each device's
exact end-of-run numbers. A manual stop records one extra `status:` block with
`state=cancelled` and the last live counters (fetched just before the kill,
falling back to the Settings page's last-received payload if the fetch fails),
so an aborted disk keeps its numbers instead of dropping back to empty.
`status.php` merges those final blocks with the live
endpoint, which is how the table keeps finished disks populated even though
the status server dies with each device's process — and it works even with
`STATUS_PORT=0`. The payload's `recovery=1|0` tells the table whether the
recovered/failed/skipped counters are meaningful (1 = array present, parity
recovery ran; 0 = plain scrub, the table shows n/a).

## Releasing

Bump `version` / `bundleversion` in `btrfs-integrity-recovery.plg` (and add a
CHANGES entry), then push a tag `v<version>` (e.g. `v2026.08.05b`). The
`build-plugin.yml` workflow builds, packages and attaches
`btrfs-integrity-recovery-<version>-x86_64-1.txz` to that release with
auto-generated release notes — and verifies the tag matches the `.plg`
version, so the `.plg`'s `bundleURL` (`…/releases/download/v<version>/…`)
always resolves.

The bundle filename is versioned on purpose: unRAID's plugin manager skips
re-downloading a bundle file that already exists on the flash drive
("skipping: … already exists"), so a fixed name would silently leave the old
php pages and binaries in place after an update. The versioned package is
installed with plain `upgradepkg --install-new`, which replaces the previous
package (dropping files that no longer ship) — and `install.sh` prunes stale
bundles from the flash drive so they don't accumulate.
