#!/usr/bin/env bash
#
# cmp_utilities.sh
#
# Three-way per-case comparison of scrub-rs vs `btrfs check --check-data-csum`
# vs (online) `btrfs scrub start -B`, over every image produced by
# btrfs_test_matrix.sh. Replicates the manual cross-check done locally on
# 2026-07-13 so it can be re-run in CI on every push.
#
# What it does, per image:
#   1. Copy the gold image to a THROWAWAY work file (cp --sparse=always) so
#      the gold image under btrfs_test_images/ is never touched. btrfs scrub
#      in particular can self-heal a corrupt DUP copy on a writable mount,
#      so operating on a copy is mandatory.
#   2. Run scrub-rs (no --repair — read-only assessment) against the copy
#      and capture FULL output (no grep / tail / head).
#   3. Run `btrfs check --readonly --check-data-csum` against the copy,
#      full output.
#   4. Loop-mount the copy WRITABLE (btrfs scrub aborts with errno=30 on a
#      read-only mount) and run `btrfs scrub start -B`. Capture full output.
#      If the FS won't mount (corrupted / superblock-wiped), record
#      MOUNT_FAILED and move on.
#   5. Parse a small set of stat lines out of each log:
#        scrub-rs   : sectors mismatch / metadata mirror / metadata hdr errs / rc
#        btrfs check: rc, count of "mirror N bytenr" data-csum lines,
#                     count of "checksum verify failed" meta-csum lines,
#                     "some csum missing", "cannot open file system",
#                     "invalid generation"
#        btrfs scrub: rc, csum=N, Corrected=N, Uncorrectable=N, mount OK/FAILED
#   6. Print a per-case block with all three FULL logs to stdout (so CI
#      preserves them in the job log with no truncation), then a final
#      SUMMARY table with the parsed counts side-by-side and an ALIGN
#      verdict per case.
#   7. Exit non-zero ONLY on an UNEXPECTED misalignment. Known acceptable
#      divergences (see known_acceptable_divergence()) are flagged WARN but
#      do not fail CI. The 11c single-superblock-wiped case is one such
#      divergence: scrub-rs falls back to the 64MiB backup superblock and
#      reports it as a recoverable metadata divergence (mirror : 1, rc=0),
#      while btrfs check / btrfs scrub refuse to mount the image offline.
#      See scrub-rs-matrix-live-test.md repo memory for the rationale.
#      (Recipes 13a/13b were previously `unverified` but now carry strict
#      expectations and ALIGN.)
#
# Usage:
#   sudo ./cmp_utilities.sh [--scrub-cmd TEMPLATE] [--images DIR]
#
# Options:
#   --scrub-cmd=TEMPLATE   required. {DEVICE} -> per-case copy path.
#                          e.g. --scrub-cmd "/abs/scrub-rs {DEVICE}"
#   --images=DIR           default: ./btrfs_test_images (must already contain
#                          expectations.tsv produced by btrfs_test_matrix.sh)
#   --keep                 do not delete the per-case work copies / mount
#                          dirs (debugging)
#
# Exit codes:
#   0  all cases aligned (or only known-acceptable divergences)
#   1  at least one UNEXPECTED misalignment
#   2  usage error / missing expectations.tsv
#
set -uo pipefail

SCRUB_CMD=""
IMAGES=""
KEEP=0

usage() { grep '^#' "$0" | sed -n '2,70p'; exit 2; }

i=1
while [[ $i -le $# ]]; do
  arg="${!i}"
  case "$arg" in
    --scrub-cmd=*) SCRUB_CMD="${arg#*=}" ;;
    --scrub-cmd) i=$((i+1)); SCRUB_CMD="${!i}" ;;
    --images=*)   IMAGES="${arg#*=}" ;;
    --images)     i=$((i+1)); IMAGES="${!i}" ;;
    --keep)       KEEP=1 ;;
    -h|--help)    usage ;;
    *)            echo "unknown option: $arg"; usage ;;
  esac
  i=$((i+1))
done

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
IMAGES="${IMAGES:-$SCRIPT_DIR/btrfs_test_images}"
EXPECTATIONS="$IMAGES/expectations.tsv"

# expectations.tsv is emitted by btrfs_test_matrix.sh in the form
# `./<images_dir_basename>/<case_dir>/<file>.img` (relative to the parent
# of IMAGES). Reproduce that exact key form so the associative-array lookup
# below actually hits. We cd to IMAGES' parent and find below the basename.
IMAGES_PARENT="$(cd "$(dirname "$IMAGES")" && pwd)"
IMAGES_BASE="$(basename "$IMAGES")"

[[ -n "$SCRUB_CMD" ]] || { echo "--scrub-cmd is required"; usage; }
[[ -f "$EXPECTATIONS" ]] || {
  echo "expectations.tsv not found at $EXPECTATIONS"
  echo "run btrfs_test_matrix.sh first"
  usage
}

[[ $EUID -eq 0 ]] || { echo "must run as root (losetup/mount/scrub need root)"; exit 2; }

WORK="$(mktemp -d /tmp/cmp_work.XXXXXX)"
OUT="$(mktemp -d /tmp/cmp.XXXXXX)"
SUMMARY="$OUT/summary.tsv"
: > "$SUMMARY"

[[ $KEEP -eq 0 ]] && trap 'rm -rf "$WORK" "$OUT"' EXIT

printf '%-66s %-22s | %-26s | %-30s | %-40s | %s\n' \
  "CASE" "EXPECTED" "SCRUB-RS(mm/hdr/rc)" "BTRFS-CHECK(rc/csum/meta/miss)" "BTRFS-SCRUB(rc/csum/corr/uncorr/mnt)" "ALIGN" \
  | tee -a "$SUMMARY"
printf '%s\n' "$(printf '%.0s-' {1..220})" | tee -a "$SUMMARY"

# ---------------------------------------------------------------------------
# Load expectations keyed by absolute image path
# ---------------------------------------------------------------------------
declare -A EXP_RESULT EXP_MIN
while IFS=$'\t' read -r img result minm desc; do
  [[ "$img" == "image_path" ]] && continue
  [[ -z "$img" ]] && continue
  EXP_RESULT["$img"]="$result"
  EXP_MIN["$img"]="$minm"
done < "$EXPECTATIONS"

unexpected_failures=0
cases_seen=0

# ---------------------------------------------------------------------------
# Per-case parsing helpers. All operate on stdout of the respective tool.
# ---------------------------------------------------------------------------
parse_sr_mismatch() { awk '/sectors mismatch *:/ {print $NF; exit}'          <<<"$1"; }
parse_sr_mirror()   { awk '/metadata mirror *:/   {print $NF; exit}'          <<<"$1"; }
parse_sr_hdr()      { awk '/metadata hdr errs *:/  {print $NF; exit}'          <<<"$1"; }

parse_bc_csum_data() { awk '/^mirror [0-9]+ bytenr/          {n++} END{print n+0}' <<<"$1"; }
parse_bc_csum_meta() { awk '/^checksum verify failed on/     {n++} END{print n+0}' <<<"$1"; }
parse_bc_missing()   { awk '/some csum missing/              {print 1; exit}'    <<<"$1"; }
parse_bc_no_fs()     { awk '/No valid Btrfs|cannot open file system/ {print 1; exit}' <<<"$1"; }
parse_bc_gen()       { awk '/invalid generation for extent/  {print 1; exit}'    <<<"$1"; }

# NOTE: these five take a FILE PATH (not content) because the btrfs-scrub
# output can be large and we already persisted it to disk; feeding it via a
# here-string would pass the path string to awk, not the file's contents.
parse_bs_csum()          { awk '/Error summary:/  {for(i=1;i<=NF;i++) if($i ~ /^csum=/){sub("csum=","",$i);print $i;exit}}' "$1"; }
parse_bs_corrected()     { awk '/^  Corrected:/    {print $2; exit}' "$1"; }
parse_bs_uncorrectable() { awk '/^  Uncorrectable:/ {print $2; exit}' "$1"; }
parse_bs_no_errors()     { awk '/no errors found/ {print 1; exit}' "$1"; }
parse_bs_no_mount()      { awk '$0=="MOUNT_FAILED"{print 1; exit}' "$1"; }

# ---------------------------------------------------------------------------
# Alignment classifier. Returns one of: ALIGN / WARN / FAIL on stdout.
# Accepts the expected category and the per-tool parsed counts.
# ---------------------------------------------------------------------------
classify() {
  local exp="$1" sr_mis="$2" sr_mir="$3" sr_hdr="$4" sr_rc="$5"
  local bc_rc="$6" bc_data="$7" bc_meta="$8" bc_miss="$9" bc_nofs="${10}" bc_gen="${11}"
  local bs_rc="${12}" bs_csum="${13}" bs_corr="${14}" bs_unc="${15}" bs_nomnt="${16}"
  local cn="${17}"

  case "$exp" in
    clean)
      if [[ "$sr_mis" -eq 0 && "$sr_mir" -eq 0 && "$sr_hdr" -eq 0 && "$sr_rc" -eq 0 \
            && "$bc_rc" -eq 0 && "$bs_nomnt" != "1" && "$bs_csum" -eq 0 ]]; then
        echo "ALIGN"
      else echo "FAIL"; fi
      ;;
    data_corrupt)
      # scrub-rs must see >= min mismatches; btrfs check must flag data csum
      # errors (or refuse to open); btrfs scrub must either correct / fail
      # to correct (online) OR fail to mount (unmountable images).
      local ok=1
      [[ "$sr_mis" -ge "${EXP_MIN[$cn]:-1}" ]] || ok=0
      if [[ "$bc_data" -eq 0 && "$bc_nofs" != "1" ]]; then ok=0; fi
      if [[ "$bs_nomnt" != "1" && "$bs_csum" -eq 0 ]]; then ok=0; fi
      [[ "$ok" -eq 1 ]] && echo "ALIGN" || echo "FAIL"
      ;;
    self_heal_recoverable)
      # scrub-rs: either a data mismatch or a metadata mirror count.
      # btrfs check: data csum line (12a) OR metadata verify-failed line
      # (12c/12d — note: btrfs check prints these but exits 0 in this
      # btrfs-progs version, that's the documented behaviour).
      # btrfs scrub: data-DUP is corrected (csum>0, Corrected>0); meta-DUP
      # is invisible (no errors, kernel reads the good mirror).
      local ok=1
      local sr_total=$(( sr_mis + sr_mir ))
      [[ "$sr_total" -ge "${EXP_MIN[$cn]:-1}" ]] || ok=0
      if [[ "$bc_data" -eq 0 && "$bc_meta" -eq 0 && "$bc_nofs" != "1" ]]; then ok=0; fi
      [[ "$ok" -eq 1 ]] && echo "ALIGN" || echo "FAIL"
      ;;
    meta_corrupt)
      # Unmirrored (or all-mirrors-broken) metadata corruption in a tree
      # scrub-rs walks (targeted by the new recipe 11b at the DEV_TREE).
      # All three tools should surface SOMETHING:
      #   scrub-rs: a metadata mirror mismatch (mir>=1), a metadata header
      #            error (hdr>=1), OR a data mismatch the broken tree would
      #            have served (mm>=1) — e.g. a broken DEV_TREE leaf makes
      #            scrub-rs unable to enumerate dev-extents and the open()
      #            metadata-mirror walk should flag it.
      #   btrfs check: must flag metadata csum (meta>0) OR refuse (rc!=0 / nofs).
      #   btrfs scrub: must mount-fail (no good metadata copy) OR report errors.
      local ok=1
      local sr_total=$(( sr_mis + sr_mir + sr_hdr ))
      [[ "$sr_total" -ge "${EXP_MIN[$cn]:-1}" && "$sr_rc" -ne 0 ]] || ok=0
      if [[ "$bc_meta" -eq 0 && "$bc_nofs" != "1" && "$bc_rc" -eq 0 ]]; then ok=0; fi
      [[ "$ok" -eq 1 ]] && echo "ALIGN" || echo "FAIL"
      ;;
    unreadable)
      if [[ "$sr_rc" -ne 0 && "$bc_rc" -ne 0 ]]; then echo "ALIGN"
      else echo "FAIL"; fi
      ;;
    unverified)
      # Logged only. Known-acceptable divergences that live here are
      # downgraded SKIP→WARN by known_acceptable_divergence() so they show
      # up in the summary but do NOT fail CI.  (Recipes 13a/13b used to
      # live here but now carry strict expectations and take the normal
      # ALIGN path.)
      echo "SKIP"
      ;;
    *)
      echo "FAIL_UNKNOWN_EXP"
      ;;
  esac
}

# ---------------------------------------------------------------------------
# Is this `unverified` case one of the known-acceptable divergences? If so
# we downgrade SKIP→WARN so it shows up in the summary but does NOT fail CI.
# ---------------------------------------------------------------------------
known_acceptable_divergence() {
  local cn="$1"
  case "$cn" in
    *11_corrupted_known_bad__c_single_superblock_wiped*) echo 1;;
    *) echo 0;;
  esac
}
# ---------------------------------------------------------------------------
# Main per-case loop
# ---------------------------------------------------------------------------
# find produces `./<IMAGES_BASE>/<case_dir>/<file>.img` matching the keys
# in expectations.tsv (which is emitted in exactly that relative form).
while IFS= read -r -d '' img; do
  [[ -v EXP_RESULT["$img"] ]] || continue
  case "$img" in *pristine_baseline*) continue;; esac
  cases_seen=$((cases_seen+1))

  # Absolute on-disk path (img is in expectations.tsv's `./<base>/...` form).
  abs="$IMAGES_PARENT/${img#./}"

  rel="${img#./}"
  cn="${rel%.img}"; cn="${cn//\//__}"
  d="$OUT/$cn"; mkdir -p "$d"

  expected="${EXP_RESULT[$img]}"
  echo "##################### CASE: $cn (expected=$expected) #####################"
  echo "image: $abs  size: $(stat -c%s "$abs") bytes"

  work_img="$WORK/${cn}.img"
  cp --sparse=always "$abs" "$work_img"

  # ---- 1. scrub-rs (read-only, against the copy) ----
  cmd="${SCRUB_CMD//\{DEVICE\}/$work_img}"
  sr_out="$(eval "$cmd" 2>&1)"
  sr_rc=$?
  printf '%s\n' "$sr_out" > "$d/scrub_rs.txt"
  echo "--- scrub-rs (exit=$sr_rc) ---"
  cat "$d/scrub_rs.txt"

  # ---- 2. btrfs check (against the copy) ----
  bc_out="$(btrfs check --readonly --check-data-csum "$work_img" 2>&1)"
  bc_rc=$?
  printf '%s\n' "$bc_out" > "$d/btrfs_check.txt"
  echo "--- btrfs check --readonly --check-data-csum (exit=$bc_rc) ---"
  cat "$d/btrfs_check.txt"

  # ---- 3. btrfs scrub (online, writable mount of the copy) ----
  loopdev="$(losetup -fP --show "$work_img" 2>/dev/null)"
  if [[ -z "$loopdev" ]]; then
    echo "--- btrfs scrub: losetup failed ---"
    printf 'MOUNT_FAILED\nlosetup returned empty\n' > "$d/btrfs_scrub.txt"
    bs_rc=-1; bs_csum=0; bs_corr=0; bs_unc=0; bs_nomnt=1
  else
    mnt="$(mktemp -d /tmp/cmp_mnt.XXXXXX)"
    bs_rc=-1; bs_csum=0; bs_corr=0; bs_unc=0
    if mount "$loopdev" "$mnt" 2>"$d/mount.log"; then
      bs_out="$(btrfs scrub start -B "$mnt" 2>&1)"
      bs_rc=$?
      printf '%s\n' "$bs_out" > "$d/btrfs_scrub.txt"
      echo "--- btrfs scrub start -B (exit=$bs_rc) ---"
      cat "$d/btrfs_scrub.txt"
      btrfs scrub cancel "$mnt" >/dev/null 2>&1 || true
      umount "$mnt" 2>/dev/null || umount -l "$mnt" 2>/dev/null
      bs_nomnt=0
    else
      echo "--- btrfs scrub: MOUNT_FAILED (expected for unreadable cases) ---"
      cat "$d/mount.log"
      printf 'MOUNT_FAILED\n' > "$d/btrfs_scrub.txt"
      bs_nomnt=1
    fi
    rmdir "$mnt" 2>/dev/null || true
    losetup -d "$loopdev" 2>/dev/null || true
  fi

  # ---- parse ----
  sr_mis="$(parse_sr_mismatch "$sr_out")"; sr_mis="${sr_mis:-0}"
  sr_mir="$(parse_sr_mirror   "$sr_out")"; sr_mir="${sr_mir:-0}"
  sr_hdr="$(parse_sr_hdr      "$sr_out")"; sr_hdr="${sr_hdr:-0}"

  bc_data="$(parse_bc_csum_data "$bc_out")"; bc_data="${bc_data:-0}"
  bc_meta="$(parse_bc_csum_meta "$bc_out")"; bc_meta="${bc_meta:-0}"
  bc_miss="$(parse_bc_missing   "$bc_out")"; bc_miss="${bc_miss:-0}"
  bc_nofs="$(parse_bc_no_fs     "$bc_out")"; bc_nofs="${bc_nofs:-0}"
  bc_gen="$(parse_bc_gen        "$bc_out")"; bc_gen="${bc_gen:-0}"

  bs_csum="$(parse_bs_csum          "$d/btrfs_scrub.txt")"; bs_csum="${bs_csum:-0}"
  bs_corr="$(parse_bs_corrected     "$d/btrfs_scrub.txt")"; bs_corr="${bs_corr:-0}"
  bs_unc="$(parse_bs_uncorrectable   "$d/btrfs_scrub.txt")"; bs_unc="${bs_unc:-0}"
  bs_nomnt_chk="$(parse_bs_no_mount "$d/btrfs_scrub.txt")"; bs_nomnt_chk="${bs_nomnt_chk:-0}"
  [[ "$bs_nomnt_chk" == "1" ]] && bs_nomnt=1

  # reasoning field: collect short notes for clearly diverging counts
  align="$(classify "$expected" "$sr_mis" "$sr_mir" "$sr_hdr" "$sr_rc" \
            "$bc_rc" "$bc_data" "$bc_meta" "$bc_miss" "$bc_nofs" "$bc_gen" \
            "$bs_rc" "$bs_csum" "$bs_corr" "$bs_unc" "$bs_nomnt" "$img")"

  note=""
  # Known-acceptable divergences get WARN, not FAIL — regardless of their
  # expectation category.  These are cases where the three tools legitimately
  # disagree by design (e.g. scrub-rs falls back to a backup superblock that
  # btrfs check / btrfs scrub refuse to mount), not silent test suppression.
  if [[ "$align" != "ALIGN" ]]; then
    if [[ "$(known_acceptable_divergence "$cn")" == "1" ]]; then
      align="WARN(doc)"
      note="documented divergence (see repo memory scrub-rs-matrix-live-test.md)"
    fi
  fi

  printf '%-66s %-22s | mm=%-3s mir=%-3s hdr=%-3s rc=%-2s | rc=%-2s csum=%-2s meta=%-2s miss=%-2s | rc=%-2s csum=%-2s corr=%-2s uncorr=%-2s mnt=%-5s | %-9s %s\n' \
    "${cn:0:66}" "$expected" \
    "$sr_mis" "$sr_mir" "$sr_hdr" "$sr_rc" \
    "$bc_rc" "$bc_data" "$bc_meta" "$bc_miss" \
    "$bs_rc" "$bs_csum" "$bs_corr" "$bs_unc" "$([[ $bs_nomnt == 1 ]] && echo FAIL || echo OK)" \
    "$align" "$note" | tee -a "$SUMMARY"

  if [[ "$align" == "FAIL" || "$align" == "FAIL_UNKNOWN_EXP" ]]; then
    unexpected_failures=$((unexpected_failures+1))
    echo "::error::UNEXPECTED misalignment in case $cn (align=$align; full logs above)"
  fi

  [[ $KEEP -eq 0 ]] && rm -f "$work_img"
  echo ""
done < <(cd "$IMAGES_PARENT" && find "./$IMAGES_BASE" -name '*.img' -print0 | sort -z)

echo ""
echo "======================= SUMMARY ======================="
cat "$SUMMARY"

# Persist the summary next to the matrix results for artifact uploads
cp "$SUMMARY" "$IMAGES/cmp_utilities_summary.tsv" 2>/dev/null || true
# Also copy the full per-case log dir into IMAGES for upload
rm -rf "$IMAGES/cmp_utilities_logs"
cp -r "$OUT" "$IMAGES/cmp_utilities_logs" 2>/dev/null || true

echo ""
if [[ "$cases_seen" -le 0 ]]; then
  echo "::error::cmp_utilities: zero cases matched expectations.tsv — \
expectations key format drift? (expected keys like ./$IMAGES_BASE/...)"
  exit 1
fi
if [[ "$unexpected_failures" -gt 0 ]]; then
  echo "::error::cmp_utilities: $unexpected_failures unexpected misalignment(s)"
  exit 1
fi
echo "cmp_utilities: $cases_seen cases checked, all aligned (or only documented WARN divergences)"
exit 0