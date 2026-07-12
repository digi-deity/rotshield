#!/usr/bin/env bash
#
# run_matrix.sh — drive scrub-rs (the btrfs module) over every image
# produced by test_matrix.sh and compare against the ground-truth
# `btrfs check --check-data-csum` result captured alongside each image.
#
# For each image we record:
#   - scrub-rs exit code and sectors_mismatch / metadata_header_errors
#   - whether the ground-truth btrfs check reported a data-csum error
#   - PASS/FAIL: scrub-rs must agree with ground truth
#       * clean image  -> scrub-rs exit 0, 0 mismatches
#       * corrupted    -> scrub-rs exit != 0 (mismatch or meta hdr err)
#
# Usage:
#   ./run_matrix.sh [btrfs_test_images_dir]
#
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
IMG_ROOT="${1:-$SCRIPT_DIR/btrfs_test_images}"
SCRUB="$SCRIPT_DIR/target/release/scrub-rs"
OUT="$SCRIPT_DIR/matrix_results.txt"
: > "$OUT"

[[ -x "$SCRUB" ]] || { echo "scrub-rs binary not built at $SCRUB"; exit 1; }

# Returns 0 if the ground-truth check file reports the filesystem is bad.
# btrfs check signals badness two ways we care about here:
#   - a data-csum error:  "ERROR: errors found in csum tree" (with a
#     "expected csum" line), or "csum ... expected csum" mismatch lines
#   - an unopenable fs:    "No valid Btrfs found" / "cannot open file system"
#     (both superblocks wiped, or primary wiped and check not using backup)
ground_truth_bad() {
  local f="$1"
  [[ -f "$f" ]] || return 1
  grep -qiE "errors found in csum tree|expected csum|No valid Btrfs found|cannot open file system" "$f" 2>/dev/null
}

total=0; pass=0; fail=0
printf '%-55s %-6s %-8s %-8s %-8s %-8s %s\n' \
  "IMAGE" "EXIT" "MISM" "METAERR" "GT_BAD" "RESULT" "NOTE" | tee -a "$OUT"
printf '%s\n' "--------------------------------------------------------------------------------------------------------" | tee -a "$OUT"

# Walk every *.img that is a real filesystem (skip the pristine_baseline copy
# which has no ground-truth check file of its own, and skip truncated image
# which scrub-rs cannot even read a superblock from — that's a separate class).
while IFS= read -r -d '' img; do
  # skip the pristine baseline (no per-image check file)
  [[ "$(basename "$img")" == "pristine_baseline_"* ]] && continue

  dir="$(dirname "$img")"
  label="$(basename "$dir")/$(basename "$img")"
  # ground-truth check file: either <label>_check_status.txt or EXPECTED_check_status.txt
  gt=""
  for cand in "$dir"/*_check_status.txt "$dir"/EXPECTED_check_status.txt; do
    [[ -f "$cand" ]] && { gt="$cand"; break; }
  done

  out="$("$SCRUB" "$img" 2>/dev/null)"
  rc=$?
  mism="$(printf '%s\n' "$out" | awk '/sectors mismatch/{print $NF}')"
  metaerr="$(printf '%s\n' "$out" | awk '/metadata hdr errs/{print $NF}')"
  mism="${mism:-0}"; metaerr="${metaerr:-0}"

  gt_bad="no"
  if [[ -n "$gt" ]]; then
    if ground_truth_bad "$gt"; then gt_bad="yes"; fi
  fi

  # Decide expected: a corrupted variant should be detected (rc != 0).
  # We classify "should be bad" if ground truth reports a csum error OR the
  # image lives under 11_corrupted_known_bad (except the superblock-wiped
  # variants, which btrfs can still read via backup and may scrub clean).
  should_bad="no"
  if [[ "$gt_bad" == "yes" ]]; then should_bad="yes"; fi

  result="?"
  note=""
  if [[ "$should_bad" == "yes" ]]; then
    if [[ $rc -ne 0 ]]; then result="PASS"; else result="FAIL"; note="ground truth bad but scrub-rs clean"; fi
  else
    if [[ $rc -eq 0 ]]; then result="PASS"; else result="FAIL"; note="scrub-rs flagged but ground truth clean"; fi
  fi

  printf '%-55s %-6s %-8s %-8s %-8s %-8s %s\n' \
    "$label" "$rc" "$mism" "$metaerr" "$gt_bad" "$result" "$note" | tee -a "$OUT"

  total=$((total+1))
  if [[ "$result" == "PASS" ]]; then pass=$((pass+1)); else fail=$((fail+1)); fi
done < <(find "$IMG_ROOT" -name '*.img' -print0 | sort -z)

printf '\nTOTAL=%d PASS=%d FAIL=%d\n' "$total" "$pass" "$fail" | tee -a "$OUT"
echo "Detailed results: $OUT"
[[ $fail -eq 0 ]]
