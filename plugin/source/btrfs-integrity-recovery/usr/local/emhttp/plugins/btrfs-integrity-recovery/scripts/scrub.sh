#!/bin/bash
# scripts/scrub.sh — backend runner for the btrfs-integrity-recovery plugin.
#
#   scrub.sh run      run scrub-rs against the configured device(s). Multiple
#                     disks (DEVICES, space-separated) are scrubbed SEQUENTIALLY
#                     — one after another. The full output is appended to
#                     /var/log/$PLUGIN.log and mirrored to a per-run file
#                     ($CONFIG_DIR/runs/run-<ts>.log; rotation keeps KEEP_RUNS).
#                     A run lock serialises manual and scheduled runs — if one
#                     is already active, the second invocation skips.
#   scrub.sh running  print 1 if a scrub is currently running, else 0.
#   scrub.sh devices  print the array's data-disk raw rdevs.
#   scrub.sh stop     terminate a running scrub (kills the runner + scrub-rs).
#
# Both the "Run Scrub Now" button and the scheduled cron job call `run`, so
# manual and scheduled runs are logged identically and show up the same way
# in the UI.

set -u
PLUGIN="btrfs-integrity-recovery"
PLUGIN_DIR="/usr/local/emhttp/plugins/${PLUGIN}"
CONFIG_DIR="/boot/config/plugins/${PLUGIN}"
CONFIG_FILE="${CONFIG_DIR}/config.cfg"
LOG="/var/log/${PLUGIN}.log"
RUNS_DIR="${CONFIG_DIR}/runs"
# Run lock: an atomic mkdir-based mutex that (a) serialises manual vs
# scheduled runs and (b) backs the `running` state check. `pid` inside the
# lock records the owning process so stale locks (crash / kill -9) can be
# detected and reclaimed.
LOCK_DIR="${RUNS_DIR}/.lock"
SCRUB="${PLUGIN_DIR}/bin/scrub-rs"
KEEP_RUNS=3

# Script-level flag: set when any device's scrub-rs run did NOT complete
# (failed to print its "scrub complete:" marker), so run() can surface the
# overall run as ERROR rather than trusting a misleading per-device rc.
# Kept at script scope (not a run()-local) so scrub_one_device can set it
# from any calling context without relying on bash dynamic scoping.
errored=0

# Config is sourced dynamically (key=value, both bash and PHP INI parsable).
# shellcheck disable=SC1090
[ -f "${CONFIG_FILE}" ] && . "${CONFIG_FILE}"

# Build the scrub-rs argument string from the structured config keys
# (DEVICE is positional and handled separately). The partition offset is
# auto-applied per device in run() via offset_for() (--offset +<rdevOffset>
# from /proc/nmdstat), so it is NOT part of build_args. The freeze mount is
# also auto-detected per device (see run()), so it is not here either.
# Recovery assessment is ALWAYS on (free + read-only); --repair is what
# opts in to writing reconstructed blocks back. Dry-run is the safe default,
# so we pass --repair ONLY when WRITE is explicitly enabled.
build_args() {
  local args=""
  [ "${WRITE:-0}" = "1" ] && args="${args} --repair"
  [ "${NO_FREEZE:-0}" = "1" ] && args="${args} --no-freeze"
  [ -n "${BATCH_MAX:-}" ]   && args="${args} --batch-max ${BATCH_MAX}"
  [ -n "${BATCH_IDLE:-}" ]  && args="${args} --batch-idle ${BATCH_IDLE}"
  [ -n "${EXTRA_OPTIONS:-}" ] && args="${args} ${EXTRA_OPTIONS}"
  echo "${args}"
}

# Resolve the live mountpoint for a device (used to auto-supply
# --freeze-mount during recovery writes). Returns nothing if the device is
# not currently mounted or findmnt is unavailable.
freeze_mount_for() {
  local dev="$1"
  command -v findmnt >/dev/null 2>&1 || return 0
  findmnt -n -o TARGET -S "$dev" 2>/dev/null | head -1
}

# Send a notification via the unRAID Dynamix notify script. notify writes
# its tickets to files / sends mail itself and never needs stdout, so we
# silence it — this makes it safe to call from inside the run() log-redirected
# block without polluting the run log.
notify_scrub() {
  local event="$1" subject="$2" description="$3" severity="$4"
  local notify_cmd="/usr/local/emhttp/webGui/scripts/notify"
  [ -x "${notify_cmd}" ] || return 0
  "${notify_cmd}" -e "$event" -s "$subject" -d "$description" -i "$severity" >/dev/null 2>&1
}

notify_scrub_started() {
  local device="$1" idx="$2" total="$3"
  notify_scrub \
    "${PLUGIN}_scrub_started" \
    "Scrub started" \
    "${device} (${idx}/${total})" \
    "normal"
}

notify_scrub_finished() {
  local device="$1" idx="$2" total="$3" rc="$4" completed="$5"
  local severity="normal"
  local subject="Scrub finished"
  local description="${device} (${idx}/${total}): ${completed}"

  case "$rc" in
    0)
      ;;
    3|4)
      severity="warning"
      ;;
    *)
      severity="alert"
      ;;
  esac

  notify_scrub \
    "${PLUGIN}_scrub_finished" \
    "$subject" \
    "$description" \
    "$severity"
}

notify_scrub_stopped() {
  local device="${1:-}"
  local description="Manual stop requested"
  [ -n "${device}" ] && description="${device}: ${description}"
  notify_scrub \
    "${PLUGIN}_scrub_stopped" \
    "Scrub stopped" \
    "${description}" \
    "warning"
}

recovery_note_for_log() {
  local device_log="$1"
  local recovered

  recovered="$(awk -F: '/^  recovered[[:space:]]*:/ {gsub(/^[[:space:]]+/, "", $2); print $2; exit}' "$device_log")"
  [ -z "$recovered" ] && return 0

  if [ "${WRITE:-0}" = "1" ]; then
    echo "; recovered ${recovered}"
  else
    echo "; recoverable ${recovered}"
  fi
}

# Atomically claim the run lock (mkdir is atomic, so exactly one process
# wins). Serialises manual + scheduled runs: a second invocation — e.g. the
# cron job firing while a manual run is active — finds the lock held by a
# live process and skips instead of starting a duplicate scrub of the same
# disk. Stale locks (crash / kill -9) are reclaimed. On success an EXIT trap
# releases the lock on every exit path.
acquire_lock() {
  if mkdir "${LOCK_DIR}" 2>/dev/null; then
    echo "$$" > "${LOCK_DIR}/pid"
    trap 'rm -rf "${LOCK_DIR}" 2>/dev/null' EXIT
    return 0
  fi
  local opid=""
  [ -f "${LOCK_DIR}/pid" ] && opid="$(cat "${LOCK_DIR}/pid" 2>/dev/null)"
  if [ -n "${opid}" ] && kill -0 "${opid}" 2>/dev/null; then
    echo "A scrub is already running (pid ${opid}). Skipping."
    return 1
  fi
  # Lock dir exists but its owner is dead — reclaim it.
  rm -rf "${LOCK_DIR}"
  if mkdir "${LOCK_DIR}" 2>/dev/null; then
    echo "$$" > "${LOCK_DIR}/pid"
    trap 'rm -rf "${LOCK_DIR}" 2>/dev/null' EXIT
    return 0
  fi
  echo "Could not acquire run lock. Skipping."
  return 1
}

# Map a device's exit code to its status label. `dev_log` is that device's
# captured output (used for the recovered/recoverable note). scrub-rs exit
# code contract (mode-independent — same disk => same code regardless of
# flags): 0 clean | 1 runtime/setup error | 2 usage error |
# 3 issues (plain) | 4 all recoverable | 5 some unrecoverable | 6 metadata
# fatal (offline `btrfs check --repair`).
device_status() {
  local rc="$1" dev_log="$2" note=""
  note="$(recovery_note_for_log "${dev_log}")"
  case "${rc}" in
    0) echo "OK (clean)${note}" ;;
    3) echo "ISSUES FOUND${note}" ;;
    4) echo "ISSUES FOUND - all recoverable${note}" ;;
    5) echo "ISSUES FOUND - some UNRECOVERABLE${note}" ;;
    6) echo "METADATA FATAL${note}" ;;
    *) echo "ERROR (rc=${rc})${note}" ;;
  esac
}

# Scrub one device: log its progress lines into the run log, send the
# started/finished notifications, and return scrub-rs's exit code. scrub-rs
# output is streamed live to the run log (tee) while captured in a per-device
# temp log for status parsing, so "View Logs" shows live progress.
scrub_one_device() {
  local device="$1" idx="$2" total="$3" opts="$4"
  local dev_opts="${opts}" off fm rc status dev_log

  # Target the RAW rdev: scrub-rs reads the btrfs superblock at the partition
  # offset, so we pass --offset +<rdevOffset> (sectors) from /proc/nmdstat. If
  # the offset is wrong it rejects the device early with a clear superblock
  # error — it never silently scrubs garbage. An array partition
  # (/dev/nmd<N>p<M>) is the one exception: its btrfs superblock lives at
  # offset 0 (the array driver already strips the per-disk header), so no
  # --offset is passed and no warning is warranted.
  off="$(offset_for "${device}")"
  if [ -n "${off}" ]; then
    dev_opts="${dev_opts} --offset +${off}"
    echo "[$(date '+%Y-%m-%d %H:%M:%S')] ($idx/${total}) ${device}: partition offset +${off} sectors"
  elif is_array_partition "${device}"; then
    echo "[$(date '+%Y-%m-%d %H:%M:%S')] ($idx/${total}) ${device}: array partition (superblock at offset 0)"
  else
    echo "[$(date '+%Y-%m-%d %H:%M:%S')] ($idx/${total}) ${device}: WARNING no rdevOffset found in nmdstat; scrub-rs will likely reject this device"
  fi
  # Auto-supply --freeze-mount when the disk is actually MOUNTED (findmnt
  # returns a target); an unmounted disk gets no freeze flag. NO_FREEZE
  # disables it entirely.
  if [ "${NO_FREEZE:-0}" != "1" ] && fm="$(freeze_mount_for "${device}")" && [ -n "${fm}" ]; then
    dev_opts="${dev_opts} --freeze-mount ${fm}"
    echo "[$(date '+%Y-%m-%d %H:%M:%S')] ($idx/${total}) ${device}: freeze mount auto-detected at ${fm}"
  fi

  echo "[$(date '+%Y-%m-%d %H:%M:%S')] ($idx/${total}) scrubbing ${device}"
  notify_scrub_started "${device}" "${idx}" "${total}"

  # Stream scrub-rs output live into the run log (tee's stdout feeds the
  # caller's redirected run log) while keeping a per-device copy for status
  # parsing. PIPESTATUS[0] is scrub-rs's exit code (tee always succeeds).
  dev_log="$(mktemp)"
  "${SCRUB}" "${device}" ${dev_opts} 2>&1 | tee "${dev_log}"
  rc="${PIPESTATUS[0]}"

  # Only trust the rc→status mapping when scrub-rs actually ran to completion
  # (printed its completion marker). Otherwise the exit code isn't a reliable
  # status — the tool aborted early (bad args, unopenable device, panic,
  # signal) — so report an error and flag the run so the finished: line is
  # ERROR rather than a misleading ISSUES/OK label.
  if grep -q "scrub complete:" "${dev_log}"; then
    status="$(device_status "${rc}" "${dev_log}")"
  else
    status="ERROR (rc=${rc})"
    errored=1
  fi
  rm -f "${dev_log}"
  echo "[$(date '+%Y-%m-%d %H:%M:%S')] ($idx/${total}) ${device}: ${status}"
  notify_scrub_finished "${device}" "${idx}" "${total}" "${rc}" "${status}"
  return "${rc}"
}

run() {
  mkdir -p "${RUNS_DIR}"
  acquire_lock || return 0

  # Resolve the device list: DEVICES (space-separated, multiple disks) takes
  # precedence; fall back to the single DEVICE key for backwards compat.
  local devlist="${DEVICES:-${DEVICE:-/dev/nmd1p1}}"
  local total; total=$(echo ${devlist} | wc -w)
  local opts; opts="$(build_args)"
  local start_ts; start_ts="$(date '+%Y-%m-%d %H:%M:%S')"
  local run_log
  run_log="${RUNS_DIR}/run-$(date '+%Y%m%d-%H%M%S').log"

  local overall_rc=0 idx=0 rc
  {
    echo "[${start_ts}] starting sequential scrub of: ${devlist} ${opts}"
    for device in ${devlist}; do
      idx=$((idx + 1))
      scrub_one_device "${device}" "${idx}" "${total}" "${opts}"
      rc=$?
      [ "${rc}" -gt "${overall_rc}" ] && overall_rc=${rc}
    done
    # If any device's tool aborted, surface the run as an error regardless of
    # the numeric codes.
    if [ "${errored}" -ne 0 ]; then
      echo "[$(date '+%Y-%m-%d %H:%M:%S')] finished: ERROR(rc=${overall_rc})"
    else
      # Overall outcome for the run (highest-priority non-clean code wins).
      case "${overall_rc}" in
        0) echo "[$(date '+%Y-%m-%d %H:%M:%S')] finished: OK" ;;
        6) echo "[$(date '+%Y-%m-%d %H:%M:%S')] finished: METADATA_FATAL(rc=6)" ;;
        5) echo "[$(date '+%Y-%m-%d %H:%M:%S')] finished: ISSUES_FOUND_UNRECOVERABLE(rc=5)" ;;
        4) echo "[$(date '+%Y-%m-%d %H:%M:%S')] finished: ISSUES_FOUND_RECOVERABLE(rc=4)" ;;
        3) echo "[$(date '+%Y-%m-%d %H:%M:%S')] finished: ISSUES_FOUND(rc=3)" ;;
        *) echo "[$(date '+%Y-%m-%d %H:%M:%S')] finished: ERROR(rc=${overall_rc})" ;;
      esac
    fi
  } > "${run_log}"

  # Keep the persistent plugin log simple: append the exact run log bytes.
  cat "${run_log}" >> "${LOG}"

  # Prune old run logs, keep the most recent KEEP_RUNS.  Filenames are
  # tool-generated (run-YYYYMMDD-HHMMSS.log) so `ls -1t` (newest first) is
  # safe here; the glob below is only used to find the candidates.
  local n=0
  # shellcheck disable=SC2045  # controlled, whitespace-free names
  for f in $(ls -1t "${RUNS_DIR}"/run-*.log 2>/dev/null); do
    n=$((n + 1))
    [ "${n}" -gt "${KEEP_RUNS}" ] && rm -f "${f}"
  done
  return "${overall_rc}"
}

# Live check: is a scrub currently running? Backs the page's "Stop Scrub"
# button and status line, and works for BOTH manual and scheduled runs —
# every run() acquires the same lock dir. A stale lock whose owner is dead
# reports 0.
running() {
  [ -d "${LOCK_DIR}" ] || { echo "0"; return 0; }
  local pid
  pid="$(cat "${LOCK_DIR}/pid" 2>/dev/null)"
  if [ -n "${pid}" ] && kill -0 "${pid}" 2>/dev/null; then
    echo "1"
  else
    echo "0"
  fi
}

# Read the array's data-disk table from /proc/nmdstat (or /proc/mdstat;
# PROC_NMDSTAT overrides for testing). We target the RAW rdev (e.g. /dev/sdX,
# /dev/loopX — whatever the kernel reports in rdevName.N), NOT the array
# partition (/dev/nmdNp1), because scrub-rs needs the raw device to (a) read
# the btrfs superblock at its partition offset and (b) write recovered blocks
# back through the array layer. Parity slots (0=P, 29=Q) are excluded — only
# btrfs data disks apply. Emits "name|offset" lines (name already qualified
# with /dev/ if needed).
data_devices() {
  local stat p
  for p in "${PROC_NMDSTAT:-}" /proc/nmdstat /proc/mdstat; do
    [ -n "$p" ] && [ -f "$p" ] && { stat="$p"; break; }
  done
  [ -z "$stat" ] && return 0
  local slot name off
  while IFS='=' read -r key val; do
    case "$key" in
      rdevName.*)
        slot="${key#rdevName.}"
        [ "$slot" = "0" ] && continue
        [ "$slot" = "29" ] && continue
        name="$(echo "$val" | xargs)"
        [ -z "$name" ] && continue
        case "$name" in
          /*) ;;
          *)  name="/dev/$name" ;;
        esac
        off="$(awk -F= -v s="$slot" '$1=="rdevOffset."s {print $2}' "$stat" | xargs)"
        echo "${name}|${off}"
        ;;
    esac
  done < "$stat"
}

# List the data-disk raw rdev paths (names only, one per line).
devices() {
  data_devices | cut -d'|' -f1
}

# True for array-partition device names (/dev/nmd<N>p<M>), where the btrfs
# superblock lives at offset 0 (the array driver strips the per-disk header).
# Mirrors scrub-rs's own slot_from_array_partition pattern (nmd + one-or-more
# digits + p + one-or-more digits). Raw rdevs (/dev/loopN, /dev/sdX) return
# false — they need their rdevOffset applied via --offset.
is_array_partition() {
  local name="${1##*/}"
  case "$name" in
    nmd[0-9]*p[0-9]*) return 0 ;;
  esac
  return 1
}

# Print the partition offset (in 512-byte sectors) for a given raw rdev, or
# nothing if it isn't a known data disk. scrub-rs accepts --offset +N as
# sector multiples.
offset_for() {
  local want="$1"
  data_devices | awk -F'|' -v w="$want" '$1==w {print $2; exit}'
}

stop() {
  [ -d "${LOCK_DIR}" ] || { echo "No scrub is currently running."; return 0; }
  local pid; pid="$(cat "${LOCK_DIR}/pid" 2>/dev/null)"
  if [ -z "${pid}" ] || ! kill -0 "${pid}" 2>/dev/null; then
    # Stale lock — nothing actually running. Clean it up.
    rm -rf "${LOCK_DIR}"
    echo "No scrub is currently running (stale lock removed)."
    return 0
  fi
  # Terminate ONLY the runner's process tree (runner + scrub-rs spawned in
  # pipeline subshells). Deliberately NOT the process group: the runner was
  # started as `nohup ... &` from PHP/emhttp, so it shares a process group
  # with webGui processes — a group kill would take unrelated jobs down
  # with it. Walking descendants via pgrep -P is precise: it reaches
  # scrub-rs wherever it sits in the runner's tree without touching any
  # unrelated scrub-rs process.
  kill_tree "${pid}" TERM
  # Give it a moment, then escalate to KILL if still alive.
  local waited=0
  while [ "${waited}" -lt 5 ] && kill -0 "${pid}" 2>/dev/null; do
    sleep 0.5
    waited=$((waited + 1))
  done
  if kill -0 "${pid}" 2>/dev/null; then
    kill_tree "${pid}" KILL
  fi
  # The run() EXIT trap clears the lock on its way out; remove it here too in
  # case the runner was killed without running the trap.
  rm -rf "${LOCK_DIR}"
  # A manual stop produces its own notification so the operator knows the
  # scrub was interrupted rather than completed.
  notify_scrub_stopped ""
  echo "Scrub stopped."
}

# Send a signal to a process and every descendant (recursively), so a stop
# reaches the whole runner tree — including scrub-rs spawned inside pipeline
# subshells — while never signalling unrelated processes.
kill_tree() {
  local pid="$1" sig="$2" child
  for child in $(pgrep -P "${pid}" 2>/dev/null); do
    kill_tree "${child}" "${sig}"
  done
  kill -"${sig}" "${pid}" 2>/dev/null
}

case "${1:-running}" in
  run)     run ;;
  running) running ;;
  devices) devices ;;
  stop)    stop ;;
  *) echo "usage: $0 run|running|devices|stop" ;;
esac
