# Rotshield

Heal corrupted files on unRAID / NonRAID arrays that btrfs can detect but
cannot fix — by reading the raw block devices underneath the filesystem and
rebuilding the bad blocks from the array's P/Q parity.

This repository is the home of the whole project. It contains:

- **`scrub-rs`** — a standalone Rust tool that scrubs a btrfs filesystem's
  checksums and uses the NonRAID array's P/Q parity to recover single-disk
  corruption that btrfs itself can't self-heal.
- **The Rotshield unRAID plugin** — a thin wrapper that ships the tool to unRAID as a
  Settings-page utility (run a scrub on demand or on a schedule). The
  binaries are always bundled inside the plugin; there is no separate
  distribution.
- **A shell integration-test harness** that validates the tool against a
  diverse set of loop-device-backed btrfs images and live NonRAID arrays.

## Why this exists

Non-parity (single-disk) btrfs disks in an unRAID/NonRAID array are each
their own filesystem. btrfs gives them checksums and error *detection*, but
**not** self-healing — there is no second copy of the data to rebuild from.
The array's P/Q parity disks *do* hold the information needed to rebuild any
single corrupted block, but the unRAID parity layer operates on whole disks:
it can rebuild a *missing* disk, not a *corrupted file* on a present one.

This project bridges that gap. It reads the raw block device underneath the
filesystem, finds every sector whose btrfs checksum fails, and reconstructs
those exact sectors from the surviving data disks plus P/Q parity — healing
the file in place while the disk stays in the array.

## How it works

```
┌─────────────────────────────────────────────────────────────┐
│ btrfs filesystem (mounted, e.g. /mnt/disk1)                 │
└──────────────────────────┬──────────────────────────────────┘
                           │ data disk partition (/dev/loop2p1, /dev/nmd1p1, …)
┌──────────────────────────▼──────────────────────────────────┐
│ NonRAID array layer — P (XOR) + Q (Reed–Solomon) parity      │
└──────────────────────────┬──────────────────────────────────┘
                           │ raw block device
┌──────────────────────────▼──────────────────────────────────┐
│ scrub-rs: read on-disk btrfs metadata directly (no kernel)   │
│ 1. verify every data-sector checksum                          │
│ 2. map failing extents to physical offsets (chunk tree)      │
│ 3. reconstruct exact bad blocks from other disks + P/Q       │
│ 4. optionally write them back (--repair, under a freeze)     │
└─────────────────────────────────────────────────────────────┘
```

Recovery assessment is **always on**: every checksum mismatch is rebuilt
from parity (read-only, free) so you learn whether the corruption is
repairable. Writing the reconstruction back to the disk is opt-in via
`--repair`.

## Built on checks and balances

This tool repairs your data, so it is built the way a repair tool should
be: it never touches anything until it is sure, and it is designed so it
can never make a bad situation worse. Everything below is open for
inspection in the source code — but here it is in plain language.

**It only looks, until you tell it to fix.** Every scrub first checks
the disk and reports what it finds. Nothing is ever written without your
explicit choice (`--repair`, or the plugin's write setting); the default
is a dry run that shows what *could* be fixed and changes nothing.

**It checks its own understanding before it starts.** Before repairing
anything, the tool proves that its view of the array is correct by
reconstructing a known block from the other disks and checking it. If
something is wrong — wrong disk, wrong offset, misinterpreted setup — it
refuses to run and tells you, rather than risk repairing on a broken
foundation.

**It never mistakes normal activity for corruption.** Files being moved,
rewritten or deleted can briefly look like corruption. Before anything
is written back, the tool re-checks that the problem is still really
there — this false-positive guard means a repair can never overwrite a
healthy, recently-changed file.

**Parity repairs, the checksum decides.** A bad block is rebuilt from
the other disks (P and Q parity), and the filesystem checksums proves
the rebuilt data is correct before it is written. Arrays with two
parity disks can even be repaired when two disks are affected at the
same time.

**Every fix is verified after it's written.** After a repaired block is
written back, the tool reads it again and confirms the fix actually
landed on the disk. A repair that doesn't verify is reported — never
silently assumed to be done.

**It repairs data, never the filesystem's own bookkeeping.** To find
corruption, the tool has to read the disk's internal bookkeeping
(metadata) — and it does, thoroughly. But it deliberately never
*writes* to it: metadata is so sensitive that a single wrong change
can be catastrophic for the whole disk, far worse than the corruption
being fixed. If the bookkeeping itself turns out to be damaged, the
tool refuses to guess and tells you to run the official `btrfs check`
tool, which is designed to repair it safely.

**A repair only touches the one targeted disk.** The other disks and the
parity disks are never modified. So if a disk turns out to be beyond
saving, the normal unRAID path still works perfectly: pull the disk and
rebuild it from parity, exactly as if this tool had never run.

**Built for real-world unRAID arrays.** Real arrays mix disk sizes and
disk types. The tool is built for that and is tested against an array
with mixed disk sizes and layouts — not just identical toy disks.

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

**You always get an honest verdict.** The result is one of: clean ·
problems found but fixable · problems found and fixed · problems found
but not fixable · or damaged metadata, in which case the tool will not
touch it and points you to the official `btrfs check`. Problems caused
by failing hardware are reported as hardware problems — never hidden
as "clean", never blamed on the filesystem.

**The plugin keeps you safe by default.** Manual and scheduled scrubs
can never overlap, a scrub can be stopped safely, your disks' settings
are detected automatically, live progress stays on your machine, and
you get a notification with severity when a scrub finishes — so a
problem never quietly slips by.

**It's tested against real damage.** Automated tests scrub dozens of
disk images covering different filesystem layouts and corruption
scenarios, compare the results against btrfs's own tools, run scrubs
while files are being actively written and deleted, and verify repairs
on a full test array in a controlled environment with parity disk(s).

**Honest limits.** This is not a backup: it repairs what your parity can
reconstruct. It does not repair a parity disk itself, and each scrub
only heals the disk it is pointed at.

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
| `plugin/` | The Rotshield unRAID plugin that bundles the binaries behind a Settings-page UI. See [`plugin/README.md`](plugin/README.md). |
| `mk_array.sh` / `teardown_disk.sh` / `nmdctl` | Local test-array helpers: assemble a loop-device-backed NonRAID array (and tear it down) for manual experiments. |
| `.github/workflows/` | CI: Rust lint, btrfs matrix + array recovery tests, and the plugin build/release. |

The Rust source and the plugin live in separate trees on purpose: the plugin
bundles only the built binaries (staged into `plugin/…/bin/`), never the
source.

## Getting started

**Plugin users** — install the Rotshield plugin on unRAID and drive a scrub
from the Settings page. See [`plugin/README.md`](plugin/README.md) for the
`.plg` / `.txz` bundle, install steps, and configuration keys.

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

## More documentation

- [`scrub-rs/README.md`](scrub-rs/README.md) — tool layout, build, and test
  commands.
- [`plugin/README.md`](plugin/README.md) — plugin layout, build, install,
  configuration keys, and the live status endpoint.
- [`scrub-rs/docs/EIO-robustness-design.md`](scrub-rs/docs/EIO-robustness-design.md)
  — the robustness design for read-error (EIO) handling.

## License

(Add your license here.)
