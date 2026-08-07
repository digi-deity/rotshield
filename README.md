# Rotshield

[![License: Apache 2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)

A btrfs scrubber for unRAID/NonRAID arrays. It checks your data disks for
bitrot the same way `btrfs scrub` does, and on the rare occasion it finds
actual corruption, it rebuilds the bad blocks from the array's P/Q parity
instead of just reporting them.

## Why this exists

unRAID already supports scrubbing a btrfs disk, and that scrub will happily
tell you a block is corrupt. It just won't do anything about it. A
single-disk btrfs filesystem gets checksums and corruption *detection* from
btrfs, but not self-healing: there's no second copy on the same disk to
recover from. So a failed scrub leaves you with a report and two bad
options: resilver the entire disk from parity to fix one bad block, or go
digging through logs to figure out which file it was and pull it from a
backup, if you have one.

Meanwhile the fix is sitting one layer up, and neither side is set up to
reach it. btrfs sees a single disk: checksums, block layout, enough
information to know exactly which bytes are wrong. It has no idea an array
exists above it. unRAID sees the array: which disks make it up and
how to rebuild any one of them from the others. It has no idea what's
inside those disks, on purpose, since that ignorance is exactly what lets
one array hold btrfs, XFS, and anything else side by side. So you end up
with one side that knows what's broken and another that knows how to fix
it, and nothing connecting the two.

Rotshield exists because that mismatch was annoying enough to fix: most of
what it does is a plain scrub, walking the disk and checking every
checksum, the same as `btrfs scrub`. The difference only shows up on the
rare block that actually fails: instead of stopping at a report, it pulls
the correct data from the surviving disks and parity, verifies it, and
writes it back in place. Think of it as a first line of defense: if it
can't fix something, you're in exactly the same position you'd have been in
anyway, resilver or backup restore still on the table.

## Why you can trust this plugin

This tool repairs your data, so it is built the way a repair tool should
be: it never touches anything until it is sure, and it is designed so it
can never make a bad situation worse. Everything below is open for
inspection in the source code — but here it is in plain language.

**The checksum is leading.** A bad data block is rebuilt from the other disks,
and the filesystem checksums proves the rebuilt data is correct before it is
written. If the recovered block's checksum doesn't match, then we won't write.

**Rigourous testing.** Automated tests scrub dozens of disk images
 covering different array configurations, filesystem layouts and corruption
scenarios, compare the results against btrfs's own tools, run scrubs
while files are being actively written and deleted, and verify repairs
on a full test array in a controlled environment with parity disk(s).

**Every fix is verified after it's written.** After a repaired block is
written back, the tool reads it again and confirms the fix actually
landed on the disk.

**A repair only touches the targeted disk.** The other disks and the
parity disk(s) are never modified because it bypasses the array. If a disk
turns out to be beyond saving, the normal unRAID path still works perfectly:
pull the disk and rebuild it from parity, exactly as if this tool had never
run.

**It repairs data, never the filesystem's metadata.** To find
corruption, the tool has to read the disk's internal metadata. 
But it deliberately never *writes* to it: metadata is so sensitive that a
single wrong change can be catastrophic for the whole disk, far worse than the corruption
being fixed. If the bookkeeping itself turns out to be damaged, the
tool refuses to guess and tells you to run the official `btrfs check`
tool, which is designed to repair it safely.

**It checks its own understanding before it starts.** Before starting, 
the tool proves that its view of the array is correct by reconstructing 
a known block from parity and checking it. If something is wrong (wrong 
disk, wrong offset, misinterpreted array setup) it refuses to run and
tells you.

**It never mistakes normal activity for corruption.** Files being moved,
rewritten or deleted can briefly look like corruption. Before anything
is written back, the tool re-checks that the problem is still really
there and not just regular filesystem churn. This prevents accidental
and unwarranted recoveries.

**Repairs are race-free.** While a block is written back, the filesystem
is briefly paused so nothing else can write to that spot at the same
moment. The pause is short, covers a small batch of repairs, and is
always released automatically — even if the tool itself crashes.

**It stays useful when the hardware fails.** Corruption is rare, but
when it happens it is often the first sign that a disk is starting to
fail. This tool is built for that moment: one bad sector doesn't stop
the scan of the rest of the disk, parts the disk can no longer read can
still be rebuilt from parity, and it never keeps hammering a failing
disk sector.

**Memory stays modest on huge disks.** Scrubbing a 20 TB disk shouldn't
eat your NAS's RAM. Memory use stays bounded no matter how large the
disk, so a big array never turns a scrub into an out-of-memory risk.

**It cannot repair corruption baked in before it reaches the disk.** If
data is corrupted in memory or while travelling to your NAS — faulty
RAM, a bad cable, a failing controller, or a network transfer gone
wrong — the wrong bytes get written everywhere at once: the data
itself, the checksum meant to protect it, and the parity. Everything
then agrees, so nothing looks corrupt. This is outside any repair
tool's reach; the defence is in the hardware and in how data travels —
ECC memory, good cables, and verifying transfers from other machines.

## Repository layout

| Path | What it is |
|------|-----------|
| `scrub-rs/` | The Rust crate: `scrub-rs` (btrfs scrub + parity recovery CLI) and `craft-corrupt` (test-only corruption injector). See [`scrub-rs/README.md`](scrub-rs/README.md). |
| `plugins/` | The Rotshield unRAID plugin: CA wrapper (`plugins/rotshield.xml`), manifest (`plugins/rotshield.plg`), build tree (`plugins/source/`), and docs. See [`plugins/README.md`](plugins/README.md). |
| `ca_profile.xml` / `plugins/rotshield.xml` | Community Applications repository profile + plugin wrapper — what Apps shows in the store. |
| `rotshield.svg` / `LICENSE` | Repository icon referenced by the CA files; Apache-2.0 license. |
| `mk_array.sh` / `teardown_disk.sh` / `nmdctl` | Local test-array helpers: assemble a loop-device-backed NonRAID array (and tear it down) for manual experiments. |
| `.github/workflows/` | CI: Rust lint, btrfs matrix + array recovery tests, and the plugin build/release. |

The Rust source and the plugin live in separate trees on purpose: the plugin
bundles only the built binaries (staged into `plugins/…/bin/`), never the
source.

## Getting started

**Plugin users** — install the Rotshield plugin on unRAID, either from
**Apps** (Community Applications, once published) or manually, and drive a
scrub from the Settings page. See [`plugins/README.md`](plugins/README.md) for
the `.plg` / `.txz` bundle, install steps, and configuration keys.

**Developers** — build the tools, run the test matrix, and use
`mk_array.sh` for a local scratch array:

```sh
# Build the binaries
cd scrub-rs && cargo build --release
# → target/release/scrub-rs, target/release/craft-corrupt

# Run the Rust unit tests
cargo test

# Set up a local NonRAID test array (loop-backed image files)
./mk_array.sh

# Tear it down again (idempotent; keeps the image files)
./teardown_disk.sh
```

A typical standalone invocation (on a raw data-disk rdev, with the array
running) looks like:

```sh
sudo scrub-rs /dev/loop2 --offset +64 --repair
```

- `--offset +<sectors>` — btrfs partition start on the raw device (auto
  applied by the plugin from `/proc/nmdstat`).
- `--repair` — write reconstructed blocks back (default is dry-run).
- `--freeze-mount <path>` — freeze a live mountpoint while repairing.

See `scrub-rs --help` for the full flag list and the exit-code contract.

## Testing

Two layers of automated coverage run in CI (`.github/workflows/`):

- **btrfs matrix + live simulation** (`scrub-rs-tests.yml`) — generates a
  matrix of loop-backed btrfs images (checksum algorithms, node sizes,
  profiles, corruption/anomaly recipes), compares `scrub-rs` against
  `btrfs check` and `btrfs scrub`, and drives concurrent churn + live scrub.
- **Array recovery tests** (`scrub-rs-tests.yml`) — a 6-disk asymmetric
  NonRAID array (P, Q + 4 data disks with distinct offsets) exercising
  single-disk corruption through the P, Q, and combined-PQ recovery paths.

Run the shell harness locally from `scrub-rs/tests/integration/` (needs
root + `btrfs-progs` + loop devices) — see `scrub-rs/README.md`.

## Community Apps

This repository follows the
[unraid-community-apps-starter](https://github.com/unraid/unraid-community-apps-starter)
layout so it can be submitted to the Apps store:

- `ca_profile.xml` — repository profile (description, icon, support link).
- `plugins/rotshield.xml` — the plugin entry Apps displays; its `<PluginURL>`
  matches the `.plg`'s `pluginURL` exactly.
- `rotshield.svg` — the app icon shown in Apps.
- `LICENSE` — Apache-2.0 (OSI-approved, required for submission).

To submit, push to `main`, then run **Validate** and **Scan** in the
Community Apps submit flow (`/submit` in the unRAID webGUI).

## More documentation

- [`scrub-rs/README.md`](scrub-rs/README.md) — tool layout, build, and test
  commands.
- [`plugins/README.md`](plugins/README.md) — plugin layout, build, install,
  configuration keys, and the live status endpoint.
- [`scrub-rs/docs/EIO-robustness-design.md`](scrub-rs/docs/EIO-robustness-design.md)
  — the robustness design for read-error (EIO) handling.

## License

Licensed under the [Apache License 2.0](LICENSE). `scrub-rs` and
`craft-corrupt` are built from this repository's own source — no third-party
binaries are bundled with the plugin.
