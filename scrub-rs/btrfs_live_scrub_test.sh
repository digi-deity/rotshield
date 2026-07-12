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
# Two scenarios, run independently:
#
#   false-positive check (default): no corruption is ever injected. Your
#   scrub tool should report ZERO mismatches despite heavy concurrent
#   churn. Any reported mismatch here is very likely the
#   owner-classification or stale-root bug class, not real corruption --
#   cross-check the timestamp against the workload log to see what the
#   filesystem was doing at that moment.
#
#   true-positive check: a real, single-byte corruption is injected into a
#   "canary" file that the workload deliberately never touches again, at a
#   random point during the run. Your scrub tool should report EXACTLY ONE
#   mismatch, matching the canary file's known extent, despite everything
#   else on the filesystem moving underneath it. This is the check that
#   catches a tool that's silently gone conservative/blind under load
#   rather than correctly distinguishing real corruption from benign churn.
#
# This script does not know your scrub tool's CLI. Tell it via
# --scrub-cmd, using {DEVICE} and {OUTFILE} as placeholders, e.g.:
#
#   --scrub-cmd "/home/dev/scrub --device {DEVICE} --report {OUTFILE}"
#
# {DEVICE} is substituted with the backing image file's path (equivalent
# to the loop device's contents; if your tool wants an actual /dev/loopN
# node instead, this script also exports SCRUB_TEST_LOOPDEV with that path
# so you can reference it directly in your own wrapper if {DEVICE} alone
# isn't sufficient).
#
# Usage:
#   sudo ./btrfs_live_scrub_test.sh --scrub-cmd "..." [options]
#
# Options:
#   --outdir=PATH            default: /root/btrfs_live_scrub_test_<ts>
#   --mode=false-positive|true-positive|both   (default: both)
#   --duration=SECONDS       total workload runtime per scenario (default: 90)
#   --warmup=SECONDS         churn time before the scrub run starts, and
#                            before corruption injection in true-positive
#                            mode (default: 15)
#   --intensity=low|med|high (default: med)
#   --enable-balance         also churn balance during the run (tests the
#                            exclusive-op guard; expect your tool to abort
#                            cleanly rather than report bogus mismatches)
#   --img-size=SIZE          default: 1G (needs headroom for balance churn
#                            and large-file churn beyond the 512M matrix
#                            default)
#
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKLOAD_SCRIPT="$SCRIPT_DIR/btrfs_live_workload.sh"

SCRUB_CMD=""
OUTDIR="/root/btrfs_live_scrub_test_$(date +%Y%m%d_%H%M%S)"
MODE="both"
DURATION=90
WARMUP=15
INTENSITY="med"
ENABLE_BALANCE=0
IMG_SIZE="1G"

usage() { grep '^#' "$0" | sed -n '2,50p'; exit 1; }

[[ $EUID -eq 0 ]] || { echo "must run as root"; exit 1; }
[[ -x "$WORKLOAD_SCRIPT" || -f "$WORKLOAD_SCRIPT" ]] || { echo "expected $WORKLOAD_SCRIPT next to this script"; exit 1; }
chmod +x "$WORKLOAD_SCRIPT" 2>/dev/null || true

# Argument parser supporting both `--opt=val` and `--opt val` forms.
# `takeval` consumes the next positional arg when the `=` form isn't used.
takeval() {
  local name="$1" val="$2"
  if [[ -n "$val" ]]; then
    printf '%s' "$val"
  else
    # next argument is the value
    shift 2
    printf '%s' "${1:-}"
  fi
}

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
    --duration=*) DURATION="${arg#*=}" ;;
    --duration) i=$((i+1)); DURATION="${!i}" ;;
    --warmup=*) WARMUP="${arg#*=}" ;;
    --warmup) i=$((i+1)); WARMUP="${!i}" ;;
    --intensity=*) INTENSITY="${arg#*=}" ;;
    --intensity) i=$((i+1)); INTENSITY="${!i}" ;;
    --img-size=*) IMG_SIZE="${arg#*=}" ;;
    --img-size) i=$((i+1)); IMG_SIZE="${!i}" ;;
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
log() { echo "[$(date +%H:%M:%S)] $*" | tee -a "$LOGFILE"; }

command -v mkfs.btrfs >/dev/null 2>&1 || { log "FATAL: mkfs.btrfs not found"; exit 1; }

flip_byte_at_offset() {
  local file="$1" offset="$2"
  python3 - "$file" "$offset" <<'PYEOF'
import sys
path, off = sys.argv[1], int(sys.argv[2])
with open(path, 'r+b') as f:
    f.seek(off)
    b = f.read(1)
    if not b:
        sys.exit(1)
    f.seek(off)
    f.write(bytes([b[0] ^ 0xFF]))
PYEOF
}

find_physical_byte_offset() {
  local f="$1"
  local line
  line="$(filefrag -v "$f" 2>/dev/null | awk '/^[[:space:]]*0:/{print; exit}')"
  [[ -z "$line" ]] && return 1
  local physblock
  physblock="$(awk -F'[.:]+' '{gsub(/ /,"",$5); print $5}' <<<"$line")"
  [[ -z "$physblock" || ! "$physblock" =~ ^[0-9]+$ ]] && return 1
  echo $(( physblock * 4096 ))
}

# run_scenario <name: false-positive|true-positive>
run_scenario() {
  local scenario="$1"
  local sdir="$OUTDIR/$scenario"
  mkdir -p "$sdir"
  log "=== scenario: $scenario ==="

  local img="$sdir/test.img"
  local mnt="$sdir/mnt"
  truncate -s "$IMG_SIZE" "$img"
  mkfs.btrfs -f -q --csum crc32c -n 16k -d single -m dup "$img" >>"$LOGFILE" 2>&1 \
    || { log "FATAL: mkfs.btrfs failed"; return 1; }

  mkdir -p "$mnt"
  local loopdev
  loopdev="$(losetup --show -f "$img")" || { log "FATAL: losetup failed"; return 1; }
  export SCRUB_TEST_LOOPDEV="$loopdev"
  mount "$loopdev" "$mnt" || { log "FATAL: mount failed"; losetup -d "$loopdev"; return 1; }

  # seed content, including a canary file the workload never touches again
  mkdir -p "$mnt/canary" "$mnt/seed"
  for i in 1 2 3; do head -c 200000 /dev/urandom > "$mnt/seed/f_$i.bin"; done
  dd if=/dev/urandom of="$mnt/canary/canary_data.bin" bs=1M count=4 status=none
  sync -f "$mnt"

  local canary_offset=""
  if [[ "$scenario" == "true-positive" ]]; then
    canary_offset="$(find_physical_byte_offset "$mnt/canary/canary_data.bin")"
    if [[ -z "$canary_offset" ]]; then
      log "WARN: could not locate canary physical offset via filefrag -- true-positive scenario will be inconclusive"
    else
      log "canary physical byte offset: $canary_offset"
    fi
  fi

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

  if [[ "$scenario" == "true-positive" && -n "$canary_offset" ]]; then
    log "injecting corruption: flipping one byte at offset $canary_offset in $img"
    if flip_byte_at_offset "$img" "$canary_offset"; then
      log "corruption injected"
    else
      log "WARN: byte flip failed -- scenario will be inconclusive"
    fi
  fi

  # run the scrub tool concurrently with ongoing churn
  local device_path="$img"
  local scrub_outfile="$sdir/scrub_output.txt"
  local cmd="${SCRUB_CMD//\{DEVICE\}/$device_path}"
  cmd="${cmd//\{OUTFILE\}/$scrub_outfile}"
  log "running scrub tool: $cmd"
  local scrub_start scrub_end
  scrub_start="$(date +%s)"
  eval "$cmd" > "$sdir/scrub_stdout.txt" 2> "$sdir/scrub_stderr.txt"
  local scrub_rc=$?
  scrub_end="$(date +%s)"
  log "scrub tool exited $scrub_rc after $((scrub_end - scrub_start))s"

  # stop workload
  touch "$stopfile"
  wait "$wl_pid" 2>/dev/null
  log "workload stopped"

  # offline ground truth, now that writes have actually stopped
  sync
  umount "$mnt" 2>/dev/null || umount -l "$mnt" 2>/dev/null
  losetup -d "$loopdev" 2>/dev/null
  if command -v btrfs >/dev/null 2>&1; then
    btrfs check --readonly --check-data-csum "$img" > "$sdir/offline_check.txt" 2>&1
    log "offline btrfs check exit=$? -- see $sdir/offline_check.txt"
  fi

  # summarize
  {
    echo "scenario: $scenario"
    echo "scrub exit code: $scrub_rc"
    echo "scrub stdout/stderr: $sdir/scrub_stdout.txt / $sdir/scrub_stderr.txt"
    if [[ -f "$scrub_outfile" ]]; then
      echo "scrub report: $scrub_outfile"
    fi
    echo "workload log: $wl_logfile"
    echo "offline btrfs check: $sdir/offline_check.txt"
    if [[ "$scenario" == "false-positive" ]]; then
      echo "EXPECTED: zero mismatches reported. Any reported mismatch is"
      echo "a strong signal for the owner-classification or stale-root"
      echo "bug class -- cross-reference its timestamp against $wl_logfile."
    elif [[ "$scenario" == "true-positive" ]]; then
      if [[ -n "$canary_offset" ]]; then
        echo "EXPECTED: exactly one mismatch, in canary_data.bin, injected"
        echo "at physical byte offset $canary_offset."
        echo "Zero mismatches reported here means the tool is failing to"
        echo "detect real corruption under concurrent write load -- check"
        echo "whether it's being overly conservative (treating everything"
        echo "as skipped_stale) rather than correctly narrow."
      else
        echo "INCONCLUSIVE: corruption injection could not be confirmed."
      fi
    fi
  } | tee "$sdir/SUMMARY.txt"
}

case "$MODE" in
  false-positive) run_scenario "false-positive" ;;
  true-positive)  run_scenario "true-positive" ;;
  both)           run_scenario "false-positive"; run_scenario "true-positive" ;;
  *) echo "unknown --mode: $MODE"; usage ;;
esac

log "DONE. Results under: $OUTDIR"
