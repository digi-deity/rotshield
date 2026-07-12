#!/usr/bin/env bash
#
# btrfs_live_workload.sh
#
# Generates continuous, concurrent write/delete/snapshot churn against an
# ALREADY-MOUNTED btrfs filesystem, specifically to exercise the parts of a
# raw-device scrub tool's design that only misbehave under active write
# load and are invisible on a quiet filesystem:
#
#   - TREE_LOG churn          -> wl_fsync_storm   (per-file fsync fast path)
#   - EXTENT_TREE/CSUM_TREE   -> wl_small_file_churn, wl_large_file_churn
#     churn (ordinary commits move these global tree roots constantly)
#   - ROOT_TREE / refcount    -> wl_snapshot_churn (create+delete snapshots
#     edge cases                of a subvolume that's ALSO being actively
#                                written by other workload functions)
#   - DATA_RELOC_TREE /       -> wl_balance_churn (opt-in: --enable-balance)
#     exclusive-op guard
#   - DEV_TREE churn          -> wl_large_file_churn (chunk alloc/free from
#                                growing/truncating/deleting big files)
#
# This script does NOT know anything about your scrub tool. It just makes
# the filesystem underneath it move. Pair it with btrfs_live_scrub_test.sh,
# or point it at any mounted image and run your own tool by hand alongside
# it.
#
# Usage:
#   sudo ./btrfs_live_workload.sh <mountpoint> [options]
#
# Options:
#   --duration=SECONDS      run for this long then stop cleanly (default:
#                            run until SIGINT/SIGTERM or --stopfile appears)
#   --intensity=low|med|high  op pacing (default: med)
#   --enable-balance         also periodically run `btrfs balance start`
#                            (off by default -- heavier, and actively
#                            relocates extents; use this specifically to
#                            test your exclusive-op guard, Phase 5)
#   --stopfile=PATH          external stop signal (default: under /tmp,
#                            printed at startup). touch it to stop cleanly
#                            from another shell/script.
#   --logfile=PATH           default: <mountpoint-basename>_workload.log
#                            next to the stopfile
#   --target-subvol=NAME     subvolume under the mountpoint used for
#                            wl_snapshot_churn (default: livevol, created
#                            if missing)
#
# Deliberately left alone by all churn functions: anything under
# <mountpoint>/canary/ -- reserve that directory yourself for files you
# want to stay byte-stable across the run (e.g. a corruption-injection
# target for a true-positive test).
#
set -uo pipefail

MNT=""
DURATION=""
INTENSITY="med"
ENABLE_BALANCE=0
STOPFILE=""
LOGFILE=""
TARGET_SUBVOL="livevol"

usage() { grep '^#' "$0" | sed -n '2,40p'; exit 1; }

[[ $EUID -eq 0 ]] || { echo "must run as root"; exit 1; }
[[ $# -ge 1 ]] || usage
MNT="$1"; shift
[[ -d "$MNT" ]] || { echo "not a directory: $MNT"; exit 1; }
mountpoint -q "$MNT" || echo "WARN: $MNT does not look like a mountpoint -- continuing anyway"

for arg in "$@"; do
  case "$arg" in
    --duration=*) DURATION="${arg#*=}" ;;
    --intensity=*) INTENSITY="${arg#*=}" ;;
    --enable-balance) ENABLE_BALANCE=1 ;;
    --stopfile=*) STOPFILE="${arg#*=}" ;;
    --logfile=*) LOGFILE="${arg#*=}" ;;
    --target-subvol=*) TARGET_SUBVOL="${arg#*=}" ;;
    -h|--help) usage ;;
    *) echo "unknown option: $arg"; usage ;;
  esac
done

RUNTAG="$(basename "$MNT")_$$"
STOPFILE="${STOPFILE:-/tmp/btrfs_live_workload_${RUNTAG}.stop}"
LOGFILE="${LOGFILE:-/tmp/btrfs_live_workload_${RUNTAG}.log}"
: > "$LOGFILE"

case "$INTENSITY" in
  low)  SLEEP_LO=0.5; SLEEP_HI=1.0 ;;
  med)  SLEEP_LO=0.1; SLEEP_HI=0.3 ;;
  high) SLEEP_LO=0.01; SLEEP_HI=0.05 ;;
  *) echo "unknown --intensity: $INTENSITY (want low|med|high)"; exit 1 ;;
esac

log() { echo "[$(date +%H:%M:%S.%N | cut -c1-15)] $*" | tee -a "$LOGFILE" >&2; }

rand_sleep() {
  # portable-enough fractional sleep between $SLEEP_LO and $SLEEP_HI
  awk -v lo="$SLEEP_LO" -v hi="$SLEEP_HI" 'BEGIN{srand(); printf "%.3f", lo+rand()*(hi-lo)}' \
    | xargs sleep
}

should_stop() { [[ -f "$STOPFILE" ]]; }

# ---------------------------------------------------------------------------
# Workload functions -- each is meant to be backgrounded and looped until
# should_stop(). Kept independent of each other so any subset can be run.
# ---------------------------------------------------------------------------

wl_small_file_churn() {
  local dir="$MNT/small_churn"
  mkdir -p "$dir"
  local n=0
  while ! should_stop; do
    local f="$dir/f_$((RANDOM % 200))"
    if (( RANDOM % 5 == 0 )); then
      rm -f "$f" 2>/dev/null
    else
      head -c $(( 64 + RANDOM % 900 )) /dev/urandom > "$f" 2>/dev/null
    fi
    n=$((n+1))
    (( n % 500 == 0 )) && log "small_file_churn: $n ops"
    rand_sleep
  done
  log "small_file_churn: stopped after $n ops"
}

wl_large_file_churn() {
  local dir="$MNT/large_churn"
  mkdir -p "$dir"
  local n=0
  while ! should_stop; do
    local f="$dir/big_$((RANDOM % 6)).bin"
    case $(( RANDOM % 3 )) in
      0) dd if=/dev/urandom of="$f" bs=1M count=$((1 + RANDOM % 4)) \
            seek=$((RANDOM % 8)) conv=notrunc status=none 2>/dev/null ;;
      1) truncate -s $((1 + RANDOM % 8))M "$f" 2>/dev/null ;;
      2) rm -f "$f" 2>/dev/null ;;
    esac
    n=$((n+1))
    (( n % 50 == 0 )) && { sync -f "$MNT" 2>/dev/null; log "large_file_churn: $n ops (forced commit)"; }
    rand_sleep; rand_sleep
  done
  log "large_file_churn: stopped after $n ops"
}

# Per-file fsync on a small rotating set -- this is what actually drives
# btrfs's fsync fast path (the tree-log), as distinct from ordinary commits.
wl_fsync_storm() {
  local dir="$MNT/fsync_storm"
  mkdir -p "$dir"
  local n=0
  while ! should_stop; do
    local f="$dir/f_$((RANDOM % 50))"
    dd if=/dev/urandom of="$f" bs=512 count=1 oflag=sync conv=notrunc status=none 2>/dev/null
    n=$((n+1))
    (( n % 500 == 0 )) && log "fsync_storm: $n fsyncs"
    rand_sleep
  done
  log "fsync_storm: stopped after $n fsyncs"
}

wl_metadata_churn() {
  local dir="$MNT/metadata_churn"
  mkdir -p "$dir"
  local n=0
  while ! should_stop; do
    case $(( RANDOM % 4 )) in
      0) mkdir -p "$dir/d_$((RANDOM % 100))" 2>/dev/null ;;
      1) rmdir "$dir/d_$((RANDOM % 100))" 2>/dev/null ;;
      2)
        local a="$dir/r_$((RANDOM % 100))" b="$dir/r_$((RANDOM % 100))"
        [[ -e "$a" ]] && mv "$a" "$b" 2>/dev/null || : > "$a"
        ;;
      3)
        local f="$dir/x_$((RANDOM % 100))"
        : > "$f" 2>/dev/null
        command -v setfattr >/dev/null 2>&1 && \
          setfattr -n user.churn -v "$RANDOM" "$f" 2>/dev/null
        ;;
    esac
    n=$((n+1))
    (( n % 500 == 0 )) && log "metadata_churn: $n ops"
    rand_sleep
  done
  log "metadata_churn: stopped after $n ops"
}

# Creates and deletes snapshots of $TARGET_SUBVOL, which is ALSO being
# written by wl_small_file_churn_in_subvol below. This exercises ROOT_TREE
# ROOT_ITEM churn and the refcount lifecycle a scrub tool's own pinning
# snapshots must be independent of: deleting one of THESE snapshots must
# never affect extents still pinned by the scrub tool's own (differently
# named) held snapshot, if one is in progress concurrently.
wl_snapshot_churn() {
  local sv="$MNT/$TARGET_SUBVOL"
  if [[ ! -d "$sv" ]]; then
    btrfs subvolume create "$sv" >/dev/null 2>&1 || { log "snapshot_churn: could not create $sv, skipping"; return; }
  fi
  local n=0 kept=()
  while ! should_stop; do
    local snap="$MNT/.workload_snap_$$_$n"
    if btrfs subvolume snapshot -r "$sv" "$snap" >/dev/null 2>&1; then
      kept+=("$snap")
    fi
    if (( ${#kept[@]} > 3 )); then
      local old="${kept[0]}"
      kept=("${kept[@]:1}")
      btrfs subvolume delete "$old" >/dev/null 2>&1
    fi
    n=$((n+1))
    (( n % 20 == 0 )) && log "snapshot_churn: $n snapshot cycles"
    sleep 2
  done
  # best-effort cleanup of whatever's left
  for s in "${kept[@]}"; do btrfs subvolume delete "$s" >/dev/null 2>&1; done
  log "snapshot_churn: stopped after $n cycles"
}

wl_small_file_churn_in_subvol() {
  local dir="$MNT/$TARGET_SUBVOL"
  mkdir -p "$dir"
  local n=0
  while ! should_stop; do
    head -c $(( 64 + RANDOM % 900 )) /dev/urandom > "$dir/f_$((RANDOM % 50))" 2>/dev/null
    n=$((n+1))
    rand_sleep; rand_sleep
  done
  log "small_file_churn_in_subvol: stopped after $n ops"
}

# Opt-in: exercises the exclusive-op guard (balance actively relocates
# extents, invalidating physical addresses a scrub run may have already
# resolved). Kept short/cheap via usage filters so it doesn't dominate.
wl_balance_churn() {
  local n=0
  while ! should_stop; do
    log "balance_churn: starting a balance pass"
    btrfs balance start -dusage=20 -musage=20 "$MNT" >>"$LOGFILE" 2>&1
    n=$((n+1))
    log "balance_churn: pass $n complete"
    sleep 15
  done
  log "balance_churn: stopped after $n passes"
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

PIDS=()
cleanup() {
  log "stop requested -- writing stopfile and waiting for workload functions to exit"
  touch "$STOPFILE"
  wait "${PIDS[@]}" 2>/dev/null
  log "all workload functions stopped. log: $LOGFILE"
}
trap cleanup EXIT INT TERM

log "starting live workload against $MNT"
log "intensity=$INTENSITY  balance=$ENABLE_BALANCE  target_subvol=$TARGET_SUBVOL"
log "stopfile: $STOPFILE  (touch this file from another shell to stop cleanly)"
log "logfile:  $LOGFILE"

wl_small_file_churn & PIDS+=($!)
wl_large_file_churn & PIDS+=($!)
wl_fsync_storm & PIDS+=($!)
wl_metadata_churn & PIDS+=($!)
wl_snapshot_churn & PIDS+=($!)
wl_small_file_churn_in_subvol & PIDS+=($!)
if [[ "$ENABLE_BALANCE" -eq 1 ]]; then
  wl_balance_churn & PIDS+=($!)
fi

if [[ -n "$DURATION" ]]; then
  sleep "$DURATION"
else
  # block until stopfile appears (e.g. touched by an orchestrating script)
  # or a signal arrives
  while ! should_stop; do sleep 1; done
fi
