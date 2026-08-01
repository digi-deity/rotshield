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
      # Notify the operator the moment this device's scrub actually starts —
      # NOT deferred until the whole (possibly multi-day) run finishes.
      notify_scrub_started "${device}" "${idx}" "${total}"
      # Stream scrub-rs output DIRECTLY into the run log (via tee) instead
      # of capturing it into a shell variable.  This makes every line —
      # including the early parity canary check — appear in the log the
      # moment scrub-rs prints it, so "View Logs" / refresh shows live
      # progress instead of a single dump at the very end.  We still need
      # the exit code and a completion check, so we run the pipeline in a
      # subshell that records $? to a temp file and tee's stdout+stderr
      # to the log; afterwards we read the log (not a variable) for the
      # "scrub complete:" marker and map the exit code to a human label.
      # scrub-rs exit-code contract (mode-independent — same disk => same
      # code regardless of flags):
      #   0 clean | 1 runtime/setup error | 2 usage error
      #   3 issues found (plain scrub, no array)
      #   4 issues found, ALL recoverable (--repair, or dry-run assessment)
      #   5 issues found, SOME unrecoverable
      #   6 METADATA FATAL — a metadata node had NO good copy; unmount +
      #     run `btrfs check --repair` offline (highest-priority non-clean)
      local rc_file; rc_file="$(mktemp)"
      local device_log; device_log="$(mktemp)"
      (
        "${SCRUB}" "${device}" ${dev_opts} 2>&1
        echo "${?}" > "${rc_file}"
      ) | tee "${device_log}"
      local rc; rc="$(cat "${rc_file}"; rm -f "${rc_file}")"
      local recovery_note; recovery_note="$(recovery_note_for_log "${device_log}")"
      local status_msg=""
      if grep -q "scrub complete:" "${device_log}"; then
        # Tool ran to completion. Map the exit code to a human label.
        case "${rc}" in
          0)
            echo "[$(date '+%Y-%m-%d %H:%M:%S')] ($idx/${total}) ${device}: OK (clean)"
            status_msg="OK (clean)${recovery_note}"
            ;;
          3)
            echo "[$(date '+%Y-%m-%d %H:%M:%S')] ($idx/${total}) ${device}: ISSUES FOUND (rc=3)"
            status_msg="ISSUES FOUND${recovery_note}"
            ;;
          4)
            echo "[$(date '+%Y-%m-%d %H:%M:%S')] ($idx/${total}) ${device}: ISSUES FOUND — all recoverable (rc=4)"
            status_msg="ISSUES FOUND - all recoverable${recovery_note}"
            ;;
          5)
            echo "[$(date '+%Y-%m-%d %H:%M:%S')] ($idx/${total}) ${device}: ISSUES FOUND — some UNRECOVERABLE (rc=5)"
            status_msg="ISSUES FOUND - some UNRECOVERABLE${recovery_note}"
            ;;
          6)
            echo "[$(date '+%Y-%m-%d %H:%M:%S')] ($idx/${total}) ${device}: METADATA FATAL — unmount + btrfs check --repair"
            status_msg="METADATA FATAL${recovery_note}"
            ;;
          *)
            echo "[$(date '+%Y-%m-%d %H:%M:%S')] ($idx/${total}) ${device}: ISSUES FOUND (rc=${rc})"
            status_msg="ISSUES FOUND${recovery_note}"
            ;;
        esac
      else
        # No completion marker => the tool aborted (bad args, unopenable
        # device, panic). Treat as a real error.
        echo "[$(date '+%Y-%m-%d %H:%M:%S')] ($idx/${total}) ${device}: ERROR(rc=${rc})"
        status_msg="ERROR (rc=${rc})"
        errored=1
      fi
      # Notify the operator as soon as this device's result is known, so the
      # ticket reflects the real per-device outcome (not a stale "OK" from a
      # later source() of a temp file).
      notify_scrub_finished "${device}" "${idx}" "${total}" "${rc}" "${status_msg}"
      rm -f "${device_log}"
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

  # Keep the persistent plugin log simple: append the exact run log bytes
  # instead of deriving a secondary summary format.
  cat "${run_log}" >> "${LOG}"

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

stop() {
  if [ ! -f "${PID_FILE}" ]; then
    echo "No scrub is currently running."
    return 0
  fi
  local pid; pid="$(cat "${PID_FILE}" 2>/dev/null)"
  if [ -z "${pid}" ] || ! kill -0 "${pid}" 2>/dev/null; then
    # Stale pid file — nothing actually running. Clean it up.
    rm -f "${PID_FILE}"
    echo "No scrub is currently running (stale pid file removed)."
    return 0
  fi
  # Kill the runner process group (the bash script) and any scrub-rs
  # children it spawned. The negative pid targets the whole group so a
  # long-running scrub-rs invocation is terminated too.
  kill -TERM -"${pid}" 2>/dev/null
  # Fallback: make sure no scrub-rs binary is left behind.
  pkill -TERM -f "${SCRUB}" 2>/dev/null
  # Give it a moment, then escalate to KILL if still alive.
  local i
  for i in 1 2 3 4 5; do
    kill -0 "${pid}" 2>/dev/null || break
    sleep 0.5
  done
  if kill -0 "${pid}" 2>/dev/null; then
    kill -KILL -"${pid}" 2>/dev/null
    pkill -KILL -f "${SCRUB}" 2>/dev/null
  fi
  rm -f "${PID_FILE}"
  # Reflect the interruption in the status file so the UI stops polling a
  # run that is no longer alive. Preserve the existing log path if present.
  FINISH_TS="$(date '+%Y-%m-%d %H:%M:%S')"
  local prev_log=""
  local prev_current=""
  if [ -f "${STATUS_FILE}" ]; then
    prev_log="$(grep -oP '"log"\s*:\s*"\K[^"]+' "${STATUS_FILE}" 2>/dev/null)"
    prev_current="$(grep -oP '"current"\s*:\s*"\K[^"]+' "${STATUS_FILE}" 2>/dev/null)"
  fi
  write_status 0 "STOPPED" 130 "${prev_log}" "" "0/0" "0"
  # A manual stop produces its own notification so the operator knows the
  # scrub was interrupted rather than completed.
  notify_scrub_stopped "${prev_current}"
  echo "Scrub stopped."
}

case "${1:-status}" in
  run)     run ;;
  status)  status ;;
  lastlog) lastlog ;;
  devices) devices ;;
  stop)    stop ;;
  *) echo "usage: $0 run|status|lastlog|devices|stop" ;;
esac
