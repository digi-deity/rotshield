#!/usr/bin/env bash
#
# btrfs_live_scrub_test.sh
#
# Builds a fresh single-device btrfs test image, mounts it, drives
# continuous write/delete/snapshot churn against it (via
# btrfs_live_workload.sh), and runs YOUR scrub tool concurrently -- this is
# the test the static btrfs_test_matrix.sh images can't provide, since
# those are all quiescent-at-scan-time by construction.
#
# Harmonized with btrfs_test_matrix.sh / run_matrix.sh: sources the same
# btrfs_test_lib.sh for mkfs/mount/corruption primitives, and writes a
# scenario-level EXPECTED note in the same spirit as the matrix's
# expectations.tsv (see SUMMARY.txt per scenario).
#
# Three scenarios, run independently:
#
#   false-positive (default): no corruption is ever injected. Your scrub
#   tool should report ZERO mismatches despite heavy concurrent churn. Any
#   reported mismatch here is very likely the owner-classification or
#   stale-root bug class -- cross-check its timestamp against the workload
#   log to see what the filesystem was doing at that moment.
#
#   true-positive: a real, single-byte corruption is injected into a
#   "canary" file the workload deliberately never touches again, at a
#   random point during the run. Expect EXACTLY ONE mismatch, matching the
#   canary file's known extent, despite everything else moving underneath
#   it. Catches a tool that's gone silently conservative under load rather
#   than correctly distinguishing real corruption from benign churn.
#
#   true-positive-dup-one-copy: same idea, but on a dup/dup-profile image,
#   using btrfs-corrupt-block to corrupt exactly ONE of the two DUP copies
#   of the canary file's data, live, while churn continues. Expect exactly
#   one mismatch reported as self-heal-recoverable (a good mirror exists),
#   not as an unrecoverable failure. Requires btrfs-corrupt-block; skipped
#   with a note if unavailable.
#
# This script does not know your scrub tool's CLI. Tell it via
# --scrub-cmd, using {DEVICE} and {OUTFILE} as placeholders, e.g.:
#
#   --scrub-cmd "/home/dev/scrub --device {DEVICE} --report {OUTFILE}"
#
# {DEVICE} is substituted with the backing image file's path. If your tool
# wants an actual /dev/loopN node instead, this script also exports
# SCRUB_TEST_LOOPDEV with that path for use in your own wrapper.
#
# Usage:
#   sudo ./btrfs_live_scrub_test.sh --scrub-cmd "..." [options]
#
# Options:
#   --outdir=PATH             default: /root/btrfs_live_scrub_test_<ts>
#   --mode=false-positive|true-positive|true-positive-dup-one-copy|all
#                              (default: all)
#   --warmup=SECONDS           churn time before the scrub run / corruption
#                              injection (default: 15)
#   --intensity=low|med|high   (default: med)
#   --enable-balance            also churn balance during the run (tests
#                              the exclusive-op guard)
#   --img-size=SIZE            default: 1G
#
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKLOAD_SCRIPT="$SCRIPT_DIR/btrfs_live_workload.sh"

SCRUB_CMD=""
OUTDIR="/root/btrfs_live_scrub_test_$(date +%Y%m%d_%H%M%S)"
MODE="all"
WARMUP=15
INTENSITY="med"
ENABLE_BALANCE=0
IMG_SIZE="1G"
DURATION=""

usage() { grep '^#' "$0" | sed -n '2,55p'; exit 1; }

[[ $EUID -eq 0 ]] || { echo "must run as root"; exit 1; }
[[ -f "$WORKLOAD_SCRIPT" ]] || { echo "expected $WORKLOAD_SCRIPT next to this script"; exit 1; }
chmod +x "$WORKLOAD_SCRIPT" 2>/dev/null || true

i=1
while [[ $i -le $# ]]; do
  arg="${!i}"
  case "$arg" in
    --scrub-cmd=*) SCRUB_CMD="${arg#*=}" ;;
    --scrub-cmd) i=$((i+1)); SCRUB_CMD="${!i}" ;;
    --outdir=*) OUTDIR="${arg#*=}" ;;
    --outdir) i=$((i+1)); OUTDIR="${!i}" ;;
    --mode=*) MODE="${arg#*=}" ;;
    --mode) i=$((i+1)); MODE="${!i}" ;;
    --warmup=*) WARMUP="${arg#*=}" ;;
    --warmup) i=$((i+1)); WARMUP="${!i}" ;;
    --intensity=*) INTENSITY="${arg#*=}" ;;
    --intensity) i=$((i+1)); INTENSITY="${!i}" ;;
    --img-size=*) IMG_SIZE="${arg#*=}" ;;
    --img-size) i=$((i+1)); IMG_SIZE="${!i}" ;;
    --duration=*) DURATION="${arg#*=}" ;;
    --duration) i=$((i+1)); DURATION="${!i}" ;;
    --enable-balance) ENABLE_BALANCE=1 ;;
    -h|--help) usage ;;
    *) echo "unknown option: $arg"; usage ;;
  esac
  i=$((i+1))
done
[[ -n "$SCRUB_CMD" ]] || { echo "--scrub-cmd is required"; usage; }

mkdir -p "$OUTDIR"
LOGFILE="$OUTDIR/orchestrator.log"
: > "$LOGFILE"

# shellcheck source=btrfs_test_lib.sh
source "$SCRIPT_DIR/btrfs_test_lib.sh" || { echo "FATAL: could not source $SCRIPT_DIR/btrfs_test_lib.sh"; exit 1; }
require_root
require_btrfs

# run_scenario <name>
run_scenario() {
  local scenario="$1"
  local sdir="$OUTDIR/$scenario"
  mkdir -p "$sdir"
  log "=== scenario: $scenario ==="

  local img="$sdir/test.img"
  local mnt="$sdir/mnt"

  if [[ "$scenario" == "true-positive-dup-one-copy" ]]; then
    btrfs_mkfs "$img" "$IMG_SIZE" crc32c 16k dup dup
  else
    btrfs_mkfs "$img" "$IMG_SIZE" crc32c 16k single dup
  fi

  mkdir -p "$mnt"
  local loopdev
  loopdev="$(losetup --show -f "$img")" || { log "FATAL: losetup failed"; return 1; }
  export SCRUB_TEST_LOOPDEV="$loopdev"
  mount "$loopdev" "$mnt" || { log "FATAL: mount failed"; losetup -d "$loopdev"; return 1; }

  # seed content, including a canary file the workload never touches again
  mkdir -p "$mnt/canary" "$mnt/seed"
  for i in 1 2 3; do head -c 200000 /dev/urandom > "$mnt/seed/f_$i.bin"; done
  dd if=/dev/urandom of="$mnt/canary/canary_data.bin" bs=1M count=4 status=none
  local canary_inode; canary_inode="$(stat -c %i "$mnt/canary/canary_data.bin")"
  sync -f "$mnt" 2>/dev/null || sync

  # start workload
  local stopfile="$sdir/workload.stop"
  local wl_logfile="$sdir/workload.log"
  "$WORKLOAD_SCRIPT" "$mnt" \
    --stopfile="$stopfile" \
    --logfile="$wl_logfile" \
    --intensity="$INTENSITY" \
    $( [[ "$ENABLE_BALANCE" -eq 1 ]] && echo --enable-balance ) \
    &
  local wl_pid=$!
  log "workload started (pid $wl_pid), log: $wl_logfile"

  log "warmup: sleeping ${WARMUP}s to let churn generate real commits"
  sleep "$WARMUP"

  local expected_result="clean" expected_min=0 expected_note=""

  case "$scenario" in
    true-positive)
      local off; off="$(find_physical_byte_offset "$mnt/canary/canary_data.bin")"
      if [[ -n "$off" ]] && flip_byte_at_offset "$img" "$off"; then
        log "injected single-byte corruption at physical offset $off"
        expected_result="data_corrupt"; expected_min=1
        expected_note="exactly one mismatch, in canary_data.bin, injected at physical byte offset $off"
      else
        log "WARN: byte flip failed -- scenario will be inconclusive"
        expected_result="unverified"
      fi
      ;;
    true-positive-dup-one-copy)
      if have_corrupt_block; then
        # NOTE: this requires the raw device to be free of the kernel's own
        # writeback for this exact block for the corruption to "stick" long
        # enough for the scrub run to observe it -- see SUMMARY.txt caveat.
        local logical; logical="$(find_file_extent_logical_bytenr "$img" "$canary_inode")"
        if [[ -n "$logical" ]] && corrupt_copy "$img" "$logical" 1; then
          log "injected corruption into DUP copy 1 of logical $logical"
          expected_result="self_heal_recoverable"; expected_min=1
          expected_note="exactly one mismatch, self-heal-recoverable, canary_data.bin DUP copy 1 at logical $logical -- copy 2 should be intact"
        else
          log "WARN: could not resolve/corrupt canary's logical bytenr -- scenario will be inconclusive"
          expected_result="unverified"
        fi
      else
        log "SKIP: btrfs-corrupt-block not installed, cannot target a specific DUP copy"
        expected_result="unverified"; expected_note="btrfs-corrupt-block unavailable"
      fi
      ;;
    false-positive)
      expected_result="clean"; expected_min=0
      expected_note="zero mismatches expected despite concurrent churn"
      ;;
  esac

  # run the scrub tool concurrently with ongoing churn
  local scrub_outfile="$sdir/scrub_output.txt"
  local cmd="${SCRUB_CMD//\{DEVICE\}/$img}"
  cmd="${cmd//\{OUTFILE\}/$scrub_outfile}"
  log "running scrub tool: $cmd"
  local scrub_start scrub_end
  scrub_start="$(date +%s)"
  eval "$cmd" > "$sdir/scrub_stdout.txt" 2> "$sdir/scrub_stderr.txt"
  local scrub_rc=$?
  scrub_end="$(date +%s)"
  log "scrub tool exited $scrub_rc after $((scrub_end - scrub_start))s"

  touch "$stopfile"
  wait "$wl_pid" 2>/dev/null
  log "workload stopped"

  sync
  umount "$mnt" 2>/dev/null || umount -l "$mnt" 2>/dev/null
  losetup -d "$loopdev" 2>/dev/null
  if command -v btrfs >/dev/null 2>&1; then
    btrfs check --readonly --check-data-csum "$img" > "$sdir/offline_check.txt" 2>&1
    log "offline btrfs check exit=$? -- see $sdir/offline_check.txt"
  fi

  {
    echo "scenario: $scenario"
    echo "expected_result: $expected_result"
    echo "expected_min_mismatch: $expected_min"
    echo "expected_note: $expected_note"
    echo "scrub exit code: $scrub_rc"
    echo "scrub stdout/stderr: $sdir/scrub_stdout.txt / $sdir/scrub_stderr.txt"
    [[ -f "$scrub_outfile" ]] && echo "scrub report: $scrub_outfile"
    echo "workload log: $wl_logfile"
    echo "offline btrfs check: $sdir/offline_check.txt"
    case "$expected_result" in
      clean) echo "Any reported mismatch here is a strong signal for the owner-classification or stale-root bug class -- cross-reference its timestamp against $wl_logfile." ;;
      data_corrupt) echo "Zero mismatches reported means the tool is failing to detect real corruption under concurrent write load -- check whether it's being overly conservative (treating everything as skipped_stale) rather than correctly narrow." ;;
      self_heal_recoverable) echo "Expect the tool to report this as recoverable/self-heal (a clean mirror exists), not as unrecoverable corruption. Reporting zero mismatches at all means it either isn't checking DUP copies independently, or the injected byte landed somewhere the concurrent churn already moved past -- rerun if inconclusive." ;;
      unverified) echo "INCONCLUSIVE: injection could not be confirmed. Rerun, or inspect manually." ;;
    esac
  } | tee "$sdir/SUMMARY.txt"
}

case "$MODE" in
  false-positive) run_scenario "false-positive" ;;
  true-positive)  run_scenario "true-positive" ;;
  true-positive-dup-one-copy) run_scenario "true-positive-dup-one-copy" ;;
  all)
    run_scenario "false-positive"
    run_scenario "true-positive"
    run_scenario "true-positive-dup-one-copy"
    ;;
  *) echo "unknown --mode: $MODE"; usage ;;
esac

log "DONE. Results under: $OUTDIR"
