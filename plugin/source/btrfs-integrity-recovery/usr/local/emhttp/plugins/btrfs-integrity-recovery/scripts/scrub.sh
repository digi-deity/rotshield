#!/bin/bash
# scripts/scrub.sh — backend runner for the btrfs-integrity-recovery plugin.
#
#   scrub.sh run      run scrub-rs against the configured device(s). Multiple
#                     disks (DEVICES, space-separated) are scrubbed SEQUENTIALLY
#                     — one after another, each with its own log file. The
#                     combined output is also appended to /var/log/$PLUGIN.log,
#                     and a status JSON file the settings page polls is updated
#                     (it records the full device list and per-disk progress).
#                     Refuses to start if a run is already in progress.
#   scrub.sh status   print the last-run status JSON.
#   scrub.sh lastlog  print the path of the most recent run's log file.
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
STATUS_FILE="${CONFIG_DIR}/lastrun.json"
PID_FILE="${RUNS_DIR}/current.pid"
SCRUB="${PLUGIN_DIR}/bin/scrub-rs"
KEEP_RUNS=3

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

write_status() {
  # $1=running(0/1) $2=status $3=exit_code $4=run_log
  # $5=current device $6=index $7=total
  local running="$1" status="$2" rc="$3" run_log="$4"
  local cur="$5" idx="$6" total="$7"
  cat > "${STATUS_FILE}" <<EOF
{
  "started": "${START_TS:-null}",
  "finished": "${FINISH_TS:-null}",
  "devices": "${DEVICES_LIST:-/dev/nmd1p1}",
  "current": "${cur:-}",
  "progress": "${idx:-0}/${total:-1}",
  "options": "${OPTIONS:-}",
  "running": ${running},
  "status": "${status}",
  "exit_code": ${rc},
  "log": "${run_log}"
}
EOF
}

# Append a COMPACT summary to the persistent log (/var/log/$PLUGIN.log),
# which is the stream the schedule/cron and syslog consume.  We deliberately
# do NOT dump the full scrub output here — that stays in the per-run file
# (viewable on demand).  The summary extracts, per device, the
# "scrub complete:" stats block and the "recovery summary:" block, plus the
# final finished line, so an operator scanning the schedule log sees only
# the outcome numbers, not hundreds of per-sector lines.
write_summary() {
  local run_log="$1" rc="$2" end_ts="$3"
  {
    echo "===== scrub-rs run ${end_ts} (rc=${rc}) ====="
    # Each device's summary is bounded by "scrub complete:" ... up to the
    # next "scrubbing" / "starting sequential" / EOF.  Extract those blocks.
    awk '
      /^scrub complete:/ { capture=1 }
      /^scrubbing |^\[.*\] starting sequential scrub/ { capture=0 }
      capture { print }
    ' "${run_log}"
    # Also pull the final finished line for a one-glance outcome.
    grep -E "^\[.*\] finished:" "${run_log}" | tail -1
    echo ""
  } >> "${LOG}"
}

run() {
  mkdir -p "${RUNS_DIR}"

  # Guard: don't overlap with an in-progress run.
  if [ -f "${PID_FILE}" ] && kill -0 "$(cat "${PID_FILE}")" 2>/dev/null; then
    echo "A scrub is already running (pid $(cat "${PID_FILE}")). Skipping."
    return 0
  fi

  # Resolve the device list: DEVICES (space-separated, multiple disks) takes
  # precedence; fall back to the single DEVICE key for backwards compat.
  local devlist="${DEVICES:-${DEVICE:-/dev/nmd1p1}}"
  DEVICES_LIST="${devlist}"
  local total; total=$(echo ${devlist} | wc -w)
  local opts; opts="$(build_args)"
  local ts; ts="$(date '+%Y%m%d-%H%M%S')"
  local start_ts; start_ts="$(date '+%Y-%m-%d %H:%M:%S')"
  local run_log="${RUNS_DIR}/run-${ts}.log"

  echo "$$" > "${PID_FILE}"
  START_TS="${start_ts}"
  FINISH_TS="null"
  write_status 1 "running" 0 "${run_log}" "" "0/${total}" "${total}"

  local overall_rc=0
  local errored=0
  local idx=0
  {
    echo "[${start_ts}] starting sequential scrub of: ${devlist} ${opts}"
    for device in ${devlist}; do
      idx=$((idx + 1))
      write_status 1 "running" 0 "${run_log}" "${device}" "${idx}/${total}" "${total}"
      # Target the RAW rdev: scrub-rs reads the btrfs superblock at the
      # partition offset, so we must pass --offset +<rdevOffset> (sectors)
      # from /proc/nmdstat. If the offset is wrong, scrub-rs rejects the
      # device early with a clear "bad magic / no readable superblock" error
      # (it validates the superblock magic + header csum before trusting
      # anything) — it never silently scrubs garbage.
      local dev_opts="${opts}"
      local off; off="$(offset_for "${device}")"
      if [ -n "${off}" ]; then
        dev_opts="${dev_opts} --offset +${off}"
        echo "[$(date '+%Y-%m-%d %H:%M:%S')] ($idx/${total}) ${device}: partition offset +${off} sectors"
      else
        echo "[$(date '+%Y-%m-%d %H:%M:%S')] ($idx/${total}) ${device}: WARNING no rdevOffset found in nmdstat; scrub-rs will likely reject this device"
      fi
      # Auto-detect the live mountpoint for this device so recovery writes
      # can freeze the filesystem. We only pass --freeze-mount when the disk
      # is actually MOUNTED (findmnt returns a target); an unmounted disk
      # gets no freeze flag at all. NO_FREEZE disables it entirely.
      if [ "${NO_FREEZE:-0}" != "1" ]; then
        local fm; fm="$(freeze_mount_for "${device}")"
        if [ -n "${fm}" ]; then
          dev_opts="${dev_opts} --freeze-mount ${fm}"
          echo "[$(date '+%Y-%m-%d %H:%M:%S')] ($idx/${total}) ${device}: freeze mount auto-detected at ${fm}"
        fi
      fi
      echo "[$(date '+%Y-%m-%d %H:%M:%S')] ($idx/${total}) scrubbing ${device}"
      # Capture output so we can tell a *successful* run that found issues
      # apart from a run that errored out (no "scrub complete:" line).
      # scrub-rs exit-code contract (mode-independent — same disk => same
      # code regardless of flags):
      #   0 clean | 1 runtime/setup error | 2 usage error
      #   3 issues found (plain scrub, no array)
      #   4 issues found, ALL recoverable (--repair, or dry-run assessment)
      #   5 issues found, SOME unrecoverable
      #   6 METADATA FATAL — a metadata node had NO good copy; unmount +
      #     run `btrfs check --repair` offline (highest-priority non-clean)
      local dev_out; dev_out="$("${SCRUB}" "${device}" ${dev_opts} 2>&1)"
      local rc=$?
      echo "${dev_out}"
      if echo "${dev_out}" | grep -q "scrub complete:"; then
        # Tool ran to completion. Map the exit code to a human label.
        case "${rc}" in
          0) echo "[$(date '+%Y-%m-%d %H:%M:%S')] ($idx/${total}) ${device}: OK (clean)" ;;
          3) echo "[$(date '+%Y-%m-%d %H:%M:%S')] ($idx/${total}) ${device}: ISSUES FOUND (rc=3)" ;;
          4) echo "[$(date '+%Y-%m-%d %H:%M:%S')] ($idx/${total}) ${device}: ISSUES FOUND — all recoverable (rc=4)" ;;
          5) echo "[$(date '+%Y-%m-%d %H:%M:%S')] ($idx/${total}) ${device}: ISSUES FOUND — some UNRECOVERABLE (rc=5)" ;;
          6) echo "[$(date '+%Y-%m-%d %H:%M:%S')] ($idx/${total}) ${device}: METADATA FATAL — unmount + btrfs check --repair (rc=6)" ;;
          *) echo "[$(date '+%Y-%m-%d %H:%M:%S')] ($idx/${total}) ${device}: ISSUES FOUND (rc=${rc})" ;;
        esac
      else
        # No completion marker => the tool aborted (bad args, unopenable
        # device, panic). Treat as a real error.
        echo "[$(date '+%Y-%m-%d %H:%M:%S')] ($idx/${total}) ${device}: ERROR(rc=${rc})"
        errored=1
      fi
      [ "${rc}" -gt "${overall_rc}" ] && overall_rc=${rc}
    done
    local end_ts; end_ts="$(date '+%Y-%m-%d %H:%M:%S')"
    FINISH_TS="${end_ts}"
    if [ "${errored}" -ne 0 ]; then
      # At least one disk aborted (tool error, not corruption found).
      echo "[${end_ts}] finished: ERROR(rc=${overall_rc})"
      write_status 0 "ERROR(rc=${overall_rc})" "${overall_rc}" "${run_log}" "" "${idx}/${total}" "${total}"
    elif [ "${overall_rc}" -eq 6 ]; then
      # METADATA FATAL: a metadata node had no good copy. Highest priority.
      echo "[${end_ts}] finished: METADATA_FATAL(rc=6)"
      write_status 0 "METADATA_FATAL(rc=6)" 6 "${run_log}" "" "${idx}/${total}" "${total}"
    elif [ "${overall_rc}" -eq 5 ]; then
      # All disks completed; at least one reported unrecoverable corruption.
      echo "[${end_ts}] finished: ISSUES_FOUND_UNRECOVERABLE(rc=5)"
      write_status 0 "ISSUES_FOUND_UNRECOVERABLE(rc=5)" 5 "${run_log}" "" "${idx}/${total}" "${total}"
    elif [ "${overall_rc}" -eq 4 ]; then
      # All disks completed; issues found but all recoverable.
      echo "[${end_ts}] finished: ISSUES_FOUND_RECOVERABLE(rc=4)"
      write_status 0 "ISSUES_FOUND_RECOVERABLE(rc=4)" 4 "${run_log}" "" "${idx}/${total}" "${total}"
    elif [ "${overall_rc}" -ne 0 ]; then
      # Non-zero but not 4/5/6 (e.g. usage error 2, plain-scrub 3) — surface
      # as issues found.
      echo "[${end_ts}] finished: ISSUES_FOUND(rc=${overall_rc})"
      write_status 0 "ISSUES_FOUND(rc=${overall_rc})" "${overall_rc}" "${run_log}" "" "${idx}/${total}" "${total}"
    else
      echo "[${end_ts}] finished: OK"
      write_status 0 "OK" 0 "${run_log}" "" "${idx}/${total}" "${total}"
    fi
  } > "${run_log}"

  # The full per-device detail lives in the run log (viewable on demand via
  # the Settings page "View Logs").  The persistent /var/log/$PLUGIN.log (the
  # schedule/syslog stream) gets ONLY the compact summary stats, not the full
  # scrub dump — see write_summary().
  write_summary "${run_log}" "${overall_rc}" "${end_ts}"

  rm -f "${PID_FILE}"

  # Prune old run logs, keep the most recent KEEP_RUNS.
  local n=0
  for f in $(ls -1t "${RUNS_DIR}"/run-*.log 2>/dev/null); do
    n=$((n + 1))
    [ "${n}" -gt "${KEEP_RUNS}" ] && rm -f "${f}"
  done
  return "${overall_rc}"
}

status() {
  if [ -f "${STATUS_FILE}" ]; then
    cat "${STATUS_FILE}"
  else
    echo '{"last_run": null, "status": "never run", "running": false}'
  fi
}

lastlog() {
  if [ -f "${STATUS_FILE}" ]; then
    local p; p="$(grep -oP '"log"\s*:\s*"\K[^"]+' "${STATUS_FILE}")"
    [ -n "${p}" ] && [ -f "${p}" ] && { echo "${p}"; return; }
  fi
  echo "${LOG}"
}

# List the array's data-disk raw rdevs (one per btrfs data disk) by reading
# /proc/nmdstat (or /proc/mdstat; PROC_NMDSTAT overrides for testing). We
# target the RAW rdev (e.g. /dev/sdX, /dev/nvmeXnY, /dev/loopX — whatever the
# kernel reports in rdevName.N), NOT the array partition (/dev/nmdNp1),
# because scrub-rs needs the raw device to (a) read the btrfs superblock at
# its partition offset and (b) write recovered blocks back through the array
# layer. The matching partition offset is auto-supplied per device by run()
# via offset_for(). Parity slots (0 and 29) are excluded — only btrfs data
# disks apply.
stat_file() {
  for p in "${PROC_NMDSTAT:-}" /proc/nmdstat /proc/mdstat; do
    [ -n "$p" ] && [ -f "$p" ] && { echo "$p"; return; }
  done
}

devices() {
  local stat; stat="$(stat_file)"
  [ -z "$stat" ] && return 0
  # rdevName.N -> raw rdev path; rdevOffset.N -> partition offset (sectors).
  # Emit the raw rdev for each data slot (skip parity 0 and 29).
  local slot name
  while IFS='=' read -r key val; do
    case "$key" in
      rdevName.*)
        slot="${key#rdevName.}"
        name="$(echo "$val" | xargs)"
        [ -z "$name" ] && continue
        [ "$slot" = "0" ] && continue
        [ "$slot" = "29" ] && continue
        # rdevName may be a bare name ("loop2") or a full path.
        case "$name" in
          /*) echo "$name" ;;
          *)  echo "/dev/$name" ;;
        esac
        ;;
    esac
  done < "$stat"
}

# Print the partition offset (in 512-byte sectors) for a given raw rdev,
# looked up from rdevOffset.N in the stat file. Echoes nothing if the device
# isn't a known data disk. scrub-rs accepts --offset +N as sector multiples.
offset_for() {
  local want="$1"
  local stat; stat="$(stat_file)"
  [ -z "$stat" ] && return 0
  local slot name off
  while IFS='=' read -r key val; do
    case "$key" in
      rdevName.*)
        slot="${key#rdevName.}"
        # Parity slots (0=P, 29=Q) are never scrub targets.
        [ "$slot" = "0" ] && continue
        [ "$slot" = "29" ] && continue
        name="$(echo "$val" | xargs)"
        [ -z "$name" ] && continue
        case "$name" in
          /*) name="$name" ;;
          *)  name="/dev/$name" ;;
        esac
        [ "$name" != "$want" ] && continue
        off="$(awk -F= -v s="$slot" '$1=="rdevOffset."s {print $2}' "$stat" | xargs)"
        [ -n "$off" ] && echo "$off"
        return 0
        ;;
    esac
  done < "$stat"
}

case "${1:-status}" in
  run)     run ;;
  status)  status ;;
  lastlog) lastlog ;;
  devices) devices ;;
  *) echo "usage: $0 run|status|lastlog|devices" ;;
esac
