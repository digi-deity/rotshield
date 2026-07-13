#!/usr/bin/env bash
#
# run_matrix.sh
#
# Drives a scrub tool over every image produced by btrfs_test_matrix.sh and
# compares its findings against expectations.tsv -- the machine-readable
# ground truth btrfs_test_matrix.sh records at build time (NOT re-derived
# by grepping btrfs check's text output here, which was the old approach
# and is fragile across btrfs-progs versions/locales).
#
# Harmonized with btrfs_live_scrub_test.sh: same --scrub-cmd templating
# style ({DEVICE}/{OUTFILE} placeholders), so both scripts can point at the
# same tool invocation.
#
# Usage:
#   ./run_matrix.sh --scrub-cmd "TEMPLATE" [options] [images_dir]
#
# Options:
#   --scrub-cmd=TEMPLATE   required. {DEVICE} -> image path, {OUTFILE} ->
#                          path this script picks for the tool's report.
#                          e.g.: --scrub-cmd "./target/release/scrub-rs {DEVICE} --report {OUTFILE}"
#   --results=PATH         default: <images_dir>/matrix_results.tsv
#
# Output parsing: scrub tool output is parsed via three small functions
# below (parse_data_mismatches / parse_meta_mismatches / parse_self_heal)
# using the tokens this project's scrub-rs binary is known to print
# ("sectors mismatch", "metadata hdr errs", "self heal"). If your tool's
# output differs, edit those three functions -- everything else in this
# script (the expectation-comparison logic, the unverified/self-heal
# handling) is independent of the exact wording.
#
set -uo pipefail

SCRUB_CMD=""
IMG_ROOT=""
RESULTS=""

usage() { grep '^#' "$0" | sed -n '2,30p'; exit 1; }

i=1
while [[ $i -le $# ]]; do
  arg="${!i}"
  case "$arg" in
    --scrub-cmd=*) SCRUB_CMD="${arg#*=}" ;;
    --scrub-cmd) i=$((i+1)); SCRUB_CMD="${!i}" ;;
    --results=*) RESULTS="${arg#*=}" ;;
    --results) i=$((i+1)); RESULTS="${!i}" ;;
    -h|--help) usage ;;
    --*) echo "unknown option: $arg"; usage ;;
    *) IMG_ROOT="$arg" ;;
  esac
  i=$((i+1))
done

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
IMG_ROOT="${IMG_ROOT:-$SCRIPT_DIR/btrfs_test_images}"
EXPECTATIONS="$IMG_ROOT/expectations.tsv"
RESULTS="${RESULTS:-$IMG_ROOT/matrix_results.tsv}"

[[ -n "$SCRUB_CMD" ]] || { echo "--scrub-cmd is required"; usage; }
[[ -f "$EXPECTATIONS" ]] || { echo "expectations manifest not found: $EXPECTATIONS (run btrfs_test_matrix.sh first)"; exit 1; }

# ---------------------------------------------------------------------------
# Output parsing -- adjust these three to match your tool's actual output.
# ---------------------------------------------------------------------------
parse_data_mismatches() { awk '/sectors mismatch/{print $NF; exit}' <<<"$1"; }
# scrub-rs reports recoverable DUP-mirror divergence as "metadata mirror : N"
# and unrecoverable (no good copy) as "metadata hdr errs : N".  Both are
# metadata mismatches for the purpose of the >= expect_min check; the
# self-heal parser below isolates the recoverable signal.
parse_meta_mismatches() { awk '/metadata (hdr errs|mirror)/{s+=$NF} END{print s+0}' <<<"$1"; }
parse_self_heal()       { awk '/metadata mirror/{print $NF; exit}' <<<"$1"; }

# ---------------------------------------------------------------------------
# Load expectations into associative arrays keyed by absolute image path
# ---------------------------------------------------------------------------
declare -A EXP_RESULT EXP_MIN EXP_DESC
while IFS=$'\t' read -r img result minm desc; do
  [[ "$img" == "image_path" ]] && continue   # header
  [[ -z "$img" ]] && continue
  EXP_RESULT["$img"]="$result"
  EXP_MIN["$img"]="$minm"
  EXP_DESC["$img"]="$desc"
done < "$EXPECTATIONS"

: > "$RESULTS"
printf '%-70s %-8s %-6s %-6s %-6s %-20s %-8s %s\n' \
  "IMAGE" "EXPECTED" "DMISM" "MMISM" "SHEAL" "RC" "RESULT" "NOTE" | tee -a "$RESULTS"
printf '%s\n' "$(printf '%.0s-' {1..140})" | tee -a "$RESULTS"

total=0; pass=0; fail=0; skipped=0

while IFS= read -r -d '' img; do
  [[ -v EXP_RESULT["$img"] ]] || continue   # not a tracked image (e.g. a stray copy)

  expected="${EXP_RESULT[$img]}"
  expect_min="${EXP_MIN[$img]}"

  if [[ "$expected" == "unverified" ]]; then
    skipped=$((skipped+1))
    total=$((total+1))
    printf '%-70s %-8s %-6s %-6s %-6s %-20s %-8s %s\n' \
      "$img" "$expected" "-" "-" "-" "-" "SKIP" "${EXP_DESC[$img]}" | tee -a "$RESULTS"
    continue
  fi

  outfile="${img%.img}.scrub_report.txt"
  cmd="${SCRUB_CMD//\{DEVICE\}/$img}"
  cmd="${cmd//\{OUTFILE\}/$outfile}"
  out="$(eval "$cmd" 2>&1)"
  rc=$?

  # Always persist the raw tool output next to the image so CI can surface
  # it for non-PASS cases (the {OUTFILE} placeholder is optional in the
  # scrub command, so we write it unconditionally here).
  printf '%s\n' "$out" > "$outfile"

  dmism="$(parse_data_mismatches "$out")"; dmism="${dmism:-0}"
  mmism="$(parse_meta_mismatches "$out")"; mmism="${mmism:-0}"
  sheal="$(parse_self_heal "$out")"; sheal="${sheal:-0}"
  total_mism=$(( dmism + mmism ))

  result="?"; note=""
  case "$expected" in
    clean)
      if [[ "$total_mism" -eq 0 && $rc -eq 0 ]]; then result="PASS"
      else result="FAIL"; note="expected clean, tool reported $total_mism mismatch(es) rc=$rc"
      fi
      ;;
    data_corrupt|meta_corrupt)
      if [[ "$total_mism" -ge "$expect_min" && $rc -ne 0 ]]; then result="PASS"
      else result="FAIL"; note="expected >= $expect_min mismatch(es) and rc!=0, got $total_mism mismatch(es) rc=$rc"
      fi
      ;;
    self_heal_recoverable)
      if [[ "$total_mism" -ge "$expect_min" ]]; then
        if [[ "$sheal" -ge 1 ]]; then result="PASS"
        else result="PASS"; note="mismatch detected but tool didn't report it as self-heal-recoverable specifically -- check if that's intentional"
        fi
      else
        result="FAIL"; note="expected >= $expect_min mismatch(es) (recoverable via other copy), got $total_mism"
      fi
      ;;
    unreadable)
      if [[ $rc -ne 0 ]]; then result="PASS"
      else result="FAIL"; note="expected the tool to refuse/fail outright (rc!=0), got rc=0"
      fi
      ;;
    *)
      result="FAIL"; note="unknown expected_result '$expected' in expectations.tsv"
      ;;
  esac

  printf '%-70s %-8s %-6s %-6s %-6s %-20s %-8s %s\n' \
    "$img" "$expected" "$dmism" "$mmism" "$sheal" "$rc" "$result" "$note" | tee -a "$RESULTS"

  total=$((total+1))
  if [[ "$result" == "PASS" ]]; then pass=$((pass+1)); else fail=$((fail+1)); fi
done < <(find "$IMG_ROOT" -name '*.img' -print0 | sort -z)

printf '\nTOTAL=%d PASS=%d FAIL=%d SKIPPED(unverified)=%d\n' "$total" "$pass" "$fail" "$skipped" | tee -a "$RESULTS"
echo "Detailed results: $RESULTS"
[[ $fail -eq 0 ]]
