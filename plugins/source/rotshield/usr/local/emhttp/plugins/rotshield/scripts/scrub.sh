#!/bin/bash
# scripts/scrub.sh — backend runner for the Rotshield plugin.
#
#   scrub.sh run      run scrub-rs against the configured device(s). Multiple
#                     disks (DEVICES, space-separated) are scrubbed SEQUENTIALLY
#                     — one after another, in the order listed in DEVICES (the
#                     Settings page stores them alphabetically). The full output
#                     is appended to /var/log/$PLUGIN.log and mirrored to a
#                     per-run file ($CONFIG_DIR/runs/run-<ts>.log; rotation keeps
#                     KEEP_RUNS). Each device's section ends with its `status:`
#                     block (exact final counters), which status.php reads back.
#                     A run lock serialises manual and scheduled runs — if one
#                     is already active, the second invocation skips.
#   scrub.sh running  print 1 if a scrub is currently running, else 0.
#   scrub.sh devices  print the array's data-disk raw rdevs.
#   scrub.sh fstypes  print "<device> <fstype>" per array data disk (btrfs
#                     vs other), read from lsblk's udev-database table —
#                     zero disk I/O, so the Settings page can never spin up
#                     a spun-down drive. Backs the page's grey-out of
#                     non-btrfs disks.
#   scrub.sh stop     terminate a running scrub (kills the runner + scrub-rs).
#                     Before killing, one snapshot of the live counters is
#                     fetched and recorded in the run log as a `status:`
#                     block with state=cancelled, so the UI keeps the aborted
#                     disk's last known numbers.
#
# Both the "Run Scrub Now" button and the scheduled cron job call `run`, so
# manual and scheduled runs are logged identically and show up the same way
# in the UI.

set -u
PLUGIN="rotshield"
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
#
# The live status server is ON by default: scrub-rs serves the live error
# counters over a root-only Unix socket (default
# /var/run/rotshield/status.sock, mode 0600) so the Settings page can poll
# the running scrub's error counters via `scrub.sh status`. No TCP port is
# consumed and there is no config knob; a stale socket file left by a
# crashed run is unlinked and rebound by scrub-rs, and the path can be
# overridden with `--status-sock` via EXTRA_OPTIONS.
#
# Freeze and batch tuning are intentionally NOT config keys: their safe
# defaults are baked into scrub-rs (freeze on for live repairs, batch max
# 64, batch idle 5.0 s) and can only be overridden by passing the flags
# manually via EXTRA_OPTIONS.
build_args() {
  local args=""
  [ "${WRITE:-0}" = "1" ] && args="${args} --repair"
  # Live status: always on via a root-only Unix socket — no port consumed,
  # nothing to collide on. Path is the default unless EXTRA_OPTIONS
  # overrides it (see status_sock()).
  args="${args} --status-sock $(status_sock)"
  [ -n "${EXTRA_OPTIONS:-}" ] && args="${args} ${EXTRA_OPTIONS}"
  echo "${args}"
}

# Resolve the live mountpoint for a device (used to auto-supply
# --freeze-mount during recovery writes). Returns nothing when nothing is
# mounted (or mount state cannot be determined at all).
#
# unRAID mounts the disk's PARTITION (/dev/sdc1) while the plugin targets
# the whole-disk raw rdev (/dev/sdc, from rdevName.N in /proc/nmdstat), so
# an exact-source findmnt lookup on the whole disk can never match a mount
# of its partition — the C1 bug.  Resolution order (first target wins,
# preferring the partition's target, which is the filesystem actually
# being scrubbed):
#   1. each partition CHILD of the device (lsblk udev table — zero disk
#      I/O, the same call fstypes() makes — or sysfs when lsblk is absent);
#   2. the device itself (whole-device mounts — the loop-device CI arrays —
#      and array partitions /dev/nmd<N>p<M>, which ARE mount sources);
#   3. a /proc/self/mountinfo major:minor match for the device and its
#      children — immune to how the mount was registered (by path, by
#      UUID, or by label) and to findmnt availability.
freeze_mount_for() {
  local dev="$1" child target
  if command -v findmnt >/dev/null 2>&1; then
    # 1. Partition children (the real unRAID layout: /dev/sdc1 is mounted,
    #    we hold /dev/sdc).
    for child in $(device_children "$dev"); do
      target="$(findmnt -n -o TARGET -S "$child" 2>/dev/null | head -1)"
      [ -n "$target" ] && { echo "$target"; return 0; }
    done
    # 2. The device itself (whole-device mounts, array partitions).
    target="$(findmnt -n -o TARGET -S "$dev" 2>/dev/null | head -1)"
    [ -n "$target" ] && { echo "$target"; return 0; }
  fi
  # 3. major:minor containment — the robust backstop (also the only path
  #    when findmnt is missing).
  target="$(mountinfo_target_for "$dev")"
  [ -n "$target" ] && echo "$target"
  return 0
}

# List the partition children of a block device, one absolute path per
# line.  Prefers lsblk's udev-database table (zero disk I/O — the same
# call `fstypes` makes, so a spun-down drive is never spun up); falls back
# to sysfs when lsblk is unavailable.
device_children() {
  local dev="$1" base="${1##*/}" part out
  if command -v lsblk >/dev/null 2>&1; then
    out="$(lsblk -p -rno NAME,PKNAME 2>/dev/null | awk -v d="$dev" '$2==d {print $1}')"
    [ -n "${out}" ] && { printf '%s\n' "${out}"; return 0; }
    # lsblk present but produced nothing (broken/stale udev DB, chroot,
    # device without a udev record): fall through to the sysfs
    # enumeration below rather than silently returning an empty set.
  fi
  # sysfs fallback: partition dirs are named <parent>p<N> (loop, md) or
  # <parent><N> (sdX).  Unmatched globs stay literal and fail the `-d`.
  for part in /sys/class/block/"${base}"/"${base}"p[0-9]* \
              /sys/class/block/"${base}"/"${base}"[0-9]*; do
    [ -d "$part" ] && echo "/dev/${part##*/}"
  done
  return 0
}

# Echo the mount target of the first /proc/self/mountinfo entry whose
# device major:minor matches the given device or one of its partition
# children.  Matching by device number — never by source string — finds
# mounts registered by path, by UUID, or by label alike.  Returns nothing
# (exit 1) when nothing is mounted.
mountinfo_target_for() {
  local dev="$1" real sys_dev sys_child target child
  real="$(readlink -f "$dev" 2>/dev/null)"
  [ -n "$real" ] || real="$dev"
  sys_dev="/sys/class/block/${real##*/}/dev"
  # Children first (their filesystem is the one being scrubbed), then the
  # device itself — mirrors freeze_mount_for's preference.
  for child in $(device_children "$dev"); do
    sys_child="/sys/class/block/${child##*/}/dev"
    [ -f "$sys_child" ] || continue
    target="$(mountinfo_target_for_majmin "$(cat "$sys_child" 2>/dev/null)")"
    [ -n "$target" ] && { echo "$target"; return 0; }
  done
  [ -f "$sys_dev" ] || return 1
  target="$(mountinfo_target_for_majmin "$(cat "$sys_dev" 2>/dev/null)")"
  [ -n "$target" ] && { echo "$target"; return 0; }
  return 1
}

# Echo the mount target of the first mountinfo entry with device
# major:minor `want`, decoding mountinfo's \NNN octal escapes for the
# common characters (space, tab, newline, backslash).
mountinfo_target_for_majmin() {
  local want="$1" target
  [ -n "$want" ] || return 0
  target="$(awk -v w="$want" '$3==w {print $5; exit}' /proc/self/mountinfo 2>/dev/null)"
  [ -n "$target" ] || return 0
  printf '%s' "$target" | sed 's/\\040/ /g; s/\\011/\t/g; s/\\012/\n/g; s/\\134/\\/g'
}

# Echo 1 when the device (or a partition of it) appears as a mount
# SOURCE in /proc/self/mountinfo, else 0.  Backs the C1 repair-refusal
# gate.
#
# Deliberately INDEPENDENT of freeze_mount_for's resolution path: this
# answers "is this disk mounted at all?" by scanning the kernel's mount
# table for the device path, so the gate fires EXACTLY when a freeze
# target could not be determined (broken lsblk, missing sysfs entries,
# exotic device names) — the one situation where refusing to write is the
# only safe action.  (If this shared freeze_mount_for's backstop, a
# resolution miss would also report "not mounted" and the gate would be
# dead code — unfrozen --repair writes would proceed silently.)
device_is_mounted() {
  local dev="$1" base="${1##*/}"
  # Match any mount whose SOURCE (the part after the " - " separator in
  # mountinfo) is the device itself or a partition of it (/dev/sdc ->
  # /dev/sdc1, /dev/sdcp1, /dev/mapper/... aliases of the same base).
  # UUID=/LABEL= sources cannot be mapped back to a path here (would need
  # blkid); findmnt covers those in freeze_mount_for — if findmnt is
  # missing AND only UUID sources exist, we err on the side of refusing.
  if awk -v d="$dev" -v b="$base" '
      $0 ~ / - / {
        rest = $0; sub(/^.* - /, "", rest)
        # After the " - " separator: field 1 = fstype, 2 = SOURCE,
        # 3 = superoptions.  (Sources with spaces are \040-escaped in
        # mountinfo, so splitting on literal spaces is safe.)
        split(rest, a, " ")
        src = (length(a) >= 2) ? a[2] : rest
        if (src == d \
            || src ~ ("^" d "p[0-9]") \
            || src ~ ("^" d "[0-9]") \
            || src ~ ("^/dev/mapper/" b "[0-9]p?")) { print "1"; exit }
      }' /proc/self/mountinfo 2>/dev/null | grep -q 1; then
    echo 1
  else
    echo 0
  fi
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
  local device="$1" idx="$2" total="$3" rc="$4" completed="$5" advisory="${6:-}"
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

  # C5: a repair written under a live mount carries the page-cache
  # staleness advisory — never let a clean exit code silence it.
  [ -n "${advisory}" ] && [ "${severity}" = "normal" ] && severity="warning"

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
  # Auto-supply --freeze-mount when the disk (or a mounted partition of
  # it) is actually MOUNTED; an unmounted disk gets no freeze flag.  An
  # explicit --no-freeze in EXTRA_OPTIONS disables it entirely (the only
  # way to opt out — freeze is on by default).
  #
  # C1 integration check: a REPAIR run must never write recovered blocks
  # into a live, unfrozen filesystem.  If the disk is mounted but no freeze
  # mount could be resolved, refuse this device loudly rather than proceed
  # — the freeze is the only thing standing between the recovery write and
  # a concurrent live in-place rewrite (NODATACOW/PREALLOC).
  if [[ " ${EXTRA_OPTIONS:-} " != *" --no-freeze "* ]]; then
    fm="$(freeze_mount_for "${device}")"
    if [ -n "${fm}" ]; then
      dev_opts="${dev_opts} --freeze-mount ${fm}"
      echo "[$(date '+%Y-%m-%d %H:%M:%S')] ($idx/${total}) ${device}: freeze mount auto-detected at ${fm}"
    elif [ "${WRITE:-0}" = "1" ] && [ "$(device_is_mounted "${device}")" = "1" ]; then
      # Mounted but unresolvable: `freeze_mount_for` returned nothing even
      # though the device (or its partition) is mounted.  Never silently
      # downgrade to unfrozen writes — refuse.
      echo "[$(date '+%Y-%m-%d %H:%M:%S')] ($idx/${total}) ${device}: REFUSED: --repair requested but the disk is MOUNTED and no freeze mount could be resolved (mounted-partition lookup failed). Unmount the disk to repair it offline, or pass --no-freeze via Extra options to explicitly allow unfrozen writes."
      errored=1
      notify_scrub \
        "${PLUGIN}_scrub_refused" \
        "Scrub refused (unsafe repair)" \
        "${device}: mounted but no freeze mount resolvable — --repair would write into a live, unfrozen filesystem" \
        "alert"
      return 1
    fi
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
  local advisory=""
  if grep -q '^repaired_while_mounted=1$' "${dev_log}"; then
    # C5: the repair wrote under a live mount.  BLKFLSBUF on the raw rdev
    # clears the block-device buffer cache, but the mounted filesystem's
    # FILE page cache may still serve the OLD corrupt bytes for ranges that
    # were cached before the repair — advise a reboot/remount so the
    # operator does not read stale (still-corrupt) data after a reported
    # "successful" fix.
    advisory=" (repaired under live mount - reboot/remount recommended)"
  fi
  if grep -q "scrub complete:" "${dev_log}"; then
    status="$(device_status "${rc}" "${dev_log}")${advisory}"
  else
    status="ERROR (rc=${rc})"
    errored=1
  fi
  rm -f "${dev_log}"
  echo "[$(date '+%Y-%m-%d %H:%M:%S')] ($idx/${total}) ${device}: ${status}"
  notify_scrub_finished "${device}" "${idx}" "${total}" "${rc}" "${status}" "${advisory}"
  return "${rc}"
}

run() {
  mkdir -p "${RUNS_DIR}"
  acquire_lock || return 0

  # Resolve the device list: DEVICES (space-separated, multiple disks) takes
  # precedence; fall back to the single DEVICE key for backwards compat.
  # The order here IS the scrub order — config is the single source of truth
  # (the Settings page stores DEVICES alphabetically, but a hand-edited
  # config is honored verbatim).  An empty (deliberately saved) target list
  # means "don't scrub": never invent a target.  (The old /dev/nmd1p1
  # last-resort pointed at a device that is not a discoverable option on
  # every system.)
  local devlist="${DEVICES:-${DEVICE:-}}"
  if [ -z "${devlist}" ]; then
    echo "[$(date '+%Y-%m-%d %H:%M:%S')] no disks targeted (empty DEVICES) — nothing to scrub" >> "${LOG}"
    return 1
  fi
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

# Print "<device> <fstype>" (one line per array data disk) for the Settings
# page, which greys out non-btrfs disks so they can't be selected.  ONE
# lsblk call returns every block device's filesystem straight from the udev
# database — lsblk never opens a disk, so visiting the Settings page cannot
# spin up a spun-down drive (blkid would: it probes the device whenever its
# udev cache misses).  On unRAID the filesystem lives on the data disk's GPT
# partition (/dev/sdc1); the raw rdev (/dev/sdc) itself carries no FSTYPE,
# so each discovered rdev resolves through its direct children: the first
# child row whose PKNAME equals the rdev and whose FSTYPE is non-empty
# wins.  Anything else (xfs, reiserfs, unformatted, no udev record) reports
# "other".  If lsblk is unavailable or produced nothing, fstypes prints
# nothing and the page keeps every disk selectable — scrub-rs re-validates
# the superblock at run time and rejects a non-btrfs device with a clear
# error before scrubbing anything.
fstypes() {
  command -v lsblk >/dev/null 2>&1 || return 0
  local table dev
  # Raw mode (no --list: newer util-linux rejects combining them), no
  # header; rows stay in tree order, parent before its children.
  table="$(lsblk -p -rno FSTYPE,PKNAME 2>/dev/null)"
  [ -n "${table}" ] || return 0
  while read -r dev; do
    local fs
    fs="$(printf '%s\n' "${table}" | awk -v d="${dev}" '$2==d && $1!="" {print $1; exit}')"
    if [ "${fs}" = "btrfs" ]; then
      echo "${dev} btrfs"
    else
      echo "${dev} other"
    fi
  done < <(devices)
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

# Best-effort thaw of every mount that could be frozen by a stopped run.
# Called AFTER the process tree is dead (see stop()): scrub-rs's own
# SIGTERM handler should already have thawed on TERM, but KILL cannot be
# caught, a freeze may predate the handler, and a FITHAW that failed inside
# scrub-rs leaves the filesystem frozen — so the shell's last line of
# defence is an unconditional `fsfreeze -u` pass.
#
# FIFREEZE is a filesystem STATE, not a process-scoped lock, so thawing
# after scrub-rs is gone is both safe and correct.  `fsfreeze -u` on a
# mount that is NOT frozen fails; that is expected and silent.  A missing
# `fsfreeze` binary is noted once (the operator may need to thaw manually).
thaw_frozen_mounts() {
  local active_dev="$1" dev mnt seen=""
  command -v fsfreeze >/dev/null 2>&1 || {
    echo "WARNING: fsfreeze not available — if the stopped run left a filesystem frozen, thaw it manually with 'fsfreeze -u <mount>'."
    return 0
  }
  # 1. The device that was actually being scrubbed when the stop hit (from
  #    the run log's "scrubbing <dev>" marker).
  if [ -n "${active_dev}" ]; then
    mnt="$(freeze_mount_for "${active_dev}")"
    if [ -n "${mnt}" ]; then
      seen="${seen} ${mnt}"
      if fsfreeze -u "${mnt}" >/dev/null 2>&1; then
        echo "[stop] thawed ${mnt} (fsfreeze -u)"
      else
        echo "[stop] note: ${mnt} was not frozen (or already thawed) — nothing to do"
      fi
    fi
  fi
  # 2. Belt-and-braces: every array data disk's mount.  A stopped run could
  #    have been mid-batch on any disk; an unfrozen mount is a harmless
  #    no-op error above.  `devices` reads /proc/nmdstat only — no disk I/O.
  #    Mounts already tried in pass 1 are skipped (one line per mount).
  #
  #    Deliberate over-reach: `fsfreeze -u` on a mount that another process
  #    (e.g. a snapshot tool) froze would thaw ITS freeze.  Bounded to the
  #    manual-stop path, logged line by line, and preferable to leaving the
  #    live filesystem frozen after a KILL that the signal handler could
  #    not catch — a thawed snapshot window beats a stuck filesystem.
  for dev in $(devices 2>/dev/null); do
    mnt="$(freeze_mount_for "${dev}")"
    [ -n "${mnt}" ] || continue
    case " ${seen} " in
      *" ${mnt} "*) continue ;;
    esac
    seen="${seen} ${mnt}"
    if fsfreeze -u "${mnt}" >/dev/null 2>&1; then
      echo "[stop] thawed ${mnt} (fsfreeze -u)"
    fi
  done
  return 0
}

stop() {
  # Optional fallback snapshot file (written by the Settings page with the
  # last live payload it received): used only when the direct status curl
  # comes up empty, so a busy port / dead server can't lose the counters.
  local fallback_file="${1:-}"
  [ -d "${LOCK_DIR}" ] || { echo "No scrub is currently running."; return 0; }
  local pid; pid="$(cat "${LOCK_DIR}/pid" 2>/dev/null)"
  if [ -z "${pid}" ] || ! kill -0 "${pid}" 2>/dev/null; then
    # Stale lock — nothing actually running. Clean it up.
    rm -rf "${LOCK_DIR}"
    echo "No scrub is currently running (stale lock removed)."
    return 0
  fi

  local run_log; run_log="$(newest_run_log)"
  # CHECK: has this run already written its `finished:` line?  run() writes
  # it BEFORE releasing the lock, so a stop in that window must NOT kill the
  # (already done) process or append a fake cancellation after a real
  # completion — that would flip the outcome to CANCELLED and, worse, an
  # empty-counters block would overwrite the device's real final data.
  if [ -n "${run_log}" ] && grep -q "finished:" "${run_log}"; then
    rm -rf "${LOCK_DIR}"
    echo "Run already finished — nothing to cancel."
    return 0
  fi

  # ONE snapshot before the kill: fetch the live counters while the status
  # server is still up, then record them as a cancelled status block in the
  # run log.  This is the only write a stop produces (no per-change state
  # writes anywhere) — it keeps the aborted disk's last known numbers sticky
  # in the UI, even after a page reload.  If the curl payload is missing or
  # fails validation, fall back to the Settings page's last-received live
  # payload (written to ${fallback_file} by the Stop POST).
  local payload; payload="$(status)"
  local active_dev=""
  [ -n "${run_log}" ] && active_dev="$(last_scrubbed_device "${run_log}")"
  # CHECK: only trust a payload that looks like a real scrub-rs status
  # payload AND describes the disk this run is actually scrubbing.  A busy
  # port, a dead server, or a leftover foreign server would otherwise record
  # garbage (or another device's numbers) into the run log.
  payload="$(sanitize_status_payload "${payload}" "${active_dev}")"
  if [ -z "${payload}" ] && [ -n "${fallback_file}" ] && [ -f "${fallback_file}" ]; then
    payload="$(sanitize_status_payload "$(cat "${fallback_file}" 2>/dev/null)" "${active_dev}")"
  fi
  [ -n "${fallback_file}" ] && rm -f "${fallback_file}"

  # Terminate ONLY the runner's process tree (runner + scrub-rs spawned in
  # pipeline subshells). Deliberately NOT the process group: the runner was
  # started as `nohup ... &` from PHP/emhttp, so it shares a process group
  # with webGui processes — a group kill would take unrelated jobs down
  # with it. Walking descendants via pgrep -P is precise: it reaches
  # scrub-rs wherever it sits in the runner's tree without touching any
  # unrelated scrub-rs process.
  #
  # NOTE: if the kill FAILS (killed=0 below) a still-alive scrub-rs can
  # re-freeze on its next batch after our thaw pass; the thaw is still
  # worth doing (the TERM handler usually wins), and the failure is
  # reported so the operator can intervene.
  kill_tree "${pid}" TERM
  # Give it a moment, then escalate to KILL if still alive.
  local waited=0 killed=1
  while [ "${waited}" -lt 5 ] && kill -0 "${pid}" 2>/dev/null; do
    sleep 0.5
    waited=$((waited + 1))
  done
  if kill -0 "${pid}" 2>/dev/null; then
    kill_tree "${pid}" KILL
    sleep 0.2
    if kill -0 "${pid}" 2>/dev/null; then
      killed=0   # could not terminate — reported below, still record the block
    fi
  fi

  # H2: unconditional best-effort thaw AFTER the tree is dead (so scrub-rs's
  # own signal handler / RAII can't race it) and BEFORE recording the
  # cancellation block.  The stopped run may have been frozen at the moment
  # of the kill; leaving the filesystem frozen would stall every writer on
  # it indefinitely.
  thaw_frozen_mounts "${active_dev}"

  # Record the cancellation in the run log (after the tree is dead, so this
  # block is the last thing in the file).  status.php parses it like any
  # other final block; the UI shows the aborted disk as cancelled with the
  # counters captured above.
  if [ -n "${run_log}" ]; then
    {
      # Newline guard: if the killed process left the log ending mid-line,
      # start the append on a fresh line so the `status:` marker stays a
      # standalone line and the block parses.
      tail -c 1 "${run_log}" 2>/dev/null | grep -q . && echo
      echo "[$(date '+%Y-%m-%d %H:%M:%S')] manual stop requested"
      echo "status:"
      if [ -n "${payload}" ]; then
        # Keep the exact last counters; only the state line changes.  The
        # trailing blank line the payload carries is dropped so the block
        # stays a clean key=value run.
        echo "${payload}" | sed 's/^state=.*/state=cancelled/' | sed '/^$/d'
      else
        echo "state=cancelled"
        [ -n "${active_dev}" ] && echo "device=${active_dev}"
      fi
      echo "[$(date '+%Y-%m-%d %H:%M:%S')] finished: CANCELLED (manual stop)"
    } >> "${run_log}"
    # CHECK: the cancelled block must actually be in the log (e.g. a full
    # flash drive would silently drop the append).
    if ! tail -n 8 "${run_log}" | grep -q "^state=cancelled$"; then
      echo "WARNING: could not record the cancellation in the run log."
    fi
  fi
  if [ "${killed}" -ne 1 ]; then
    echo "WARNING: scrub process (pid ${pid}) could not be terminated."
  fi
  # The run() EXIT trap clears the lock on its way out; remove it here too in
  # case the runner was killed without running the trap.
  rm -rf "${LOCK_DIR}"
  # A manual stop produces its own notification so the operator knows the
  # scrub was interrupted rather than completed.
  notify_scrub_stopped ""
  echo "Scrub stopped."
}

# Validate a candidate status payload: it must carry state= and device=,
# and (when the run log tells us which disk is active) describe that disk.
# Echoes the payload unchanged if valid, nothing otherwise — so a busy port,
# a dead server, a foreign server, or a stale snapshot can never record
# garbage into the run log.
sanitize_status_payload() {
  local cand="$1" expect_dev="$2"
  [ -n "${cand}" ] || return 0
  local pdev pstate
  pdev="$(echo "${cand}" | awk -F= '$1=="device"{print $2; exit}')"
  pstate="$(echo "${cand}" | awk -F= '$1=="state"{print $2; exit}')"
  if [ -z "${pdev}" ] || [ -z "${pstate}" ] \
     || { [ -n "${expect_dev}" ] && [ "${pdev}" != "${expect_dev}" ]; }; then
    return 0
  fi
  echo "${cand}"
}

# Newest per-run log (same listing the rotation uses; names are
# tool-generated run-YYYYMMDD-HHMMSS.log, whitespace-free).
newest_run_log() {
  ls -1t "${RUNS_DIR}"/run-*.log 2>/dev/null | head -1
}

# Device of the run's most recently started scrub (last "scrubbing <dev>"
# line in the log) — the disk that was in flight when a stop killed the run.
# Only the runner's OWN marker line matches: scrub-rs also prints a
# "scrubbing (recovery assessment + dry-run):" banner into the same log, and
# its LAST token is "dry-run):" — matching it handed stop() a bogus device,
# which then failed sanitize_status_payload and silently dropped the
# cancelled block's counters (the UI showed a phantom "dry-run):cancelled"
# column instead of the interrupted disk's numbers).  The runner's marker is
# the only "scrubbing" line whose next token is an absolute device path
# (starts with "/"), so the pattern requires that.
last_scrubbed_device() {
  local log="$1"
  awk '/scrubbing[[:space:]]+\//{dev=$NF} END{print dev}' "${log}" 2>/dev/null
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

# Effective live-status socket path.  Always on: the default
# /var/run/rotshield/status.sock (created by scrub-rs itself), unless
# EXTRA_OPTIONS overrides it with `--status-sock <path>` — last one wins,
# matching scrub-rs's own arg parsing.  build_args() and status() both
# resolve through here, so the page always curls the socket the server
# actually bound (an override that only half-applied would silently kill
# the live panel).
status_sock() {
  local sock="/var/run/rotshield/status.sock"
  # shellcheck disable=SC2086
  set -- ${EXTRA_OPTIONS:-}
  while [ $# -gt 0 ]; do
    if [ "$1" = "--status-sock" ] && [ $# -gt 1 ]; then
      sock="$2"
      shift 2
    else
      shift
    fi
  done
  echo "${sock}"
}

# Print the live status payload from the running scrub's Unix-socket status
# server (key=value lines; empty if the server isn't up or curl is missing).
# Backs the Settings page's live-status panel.
status() {
  command -v curl >/dev/null 2>&1 || return 0
  curl -s --max-time 2 --unix-socket "$(status_sock)" "http://localhost/status" 2>/dev/null
}

case "${1:-running}" in
  run)     run ;;
  running) running ;;
  devices) devices ;;
  fstypes) fstypes ;;
  status)  status ;;
  stop)    stop "${2:-}" ;;
  *) echo "usage: $0 run|running|devices|fstypes|status|stop" ;;
esac
