<?php
/* status.php — aggregated status endpoint for the btrfs-integrity-recovery page.
 *
 * Polled by the Settings page (every 5 s) to feed the per-disk progress
 * table.  Returns blank-line-separated `key=value` blocks, one per source:
 *
 *   - FINAL blocks parsed from the newest run log (one per completed
 *     device): each scrub-rs run ends by printing a `status:` marker
 *     followed by its exact final counters, so a finished disk's column
 *     shows the real end-of-run numbers even though the live status server
 *     died with the process.  The log is streamed live by scrub.sh, so
 *     completed devices appear while later devices are still scrubbing.
 *   - a LIVE block (only while the running scrub's localhost status server
 *     answers): `scrub.sh status` curls 127.0.0.1:<port> for the in-flight
 *     counters of the device being scrubbed right now.
 *   - a META block, always last: running=0|1, run_log=<file>,
 *     run_outcome=<finished: label>, active_device=<device>.
 *
 * All blocks share one format (state= / device= / counters), so the page
 * parses every block the same way and distinguishes live (state != done)
 * from final (state == done) purely by the state value.  The block order is
 * final blocks, then the live block, then meta — the live block wins for
 * the active disk.  Read-only; no user input reaches this script.
 */
$plugin     = "btrfs-integrity-recovery";
$plugin_dir = "/usr/local/emhttp/plugins/$plugin";
$config_dir = "/boot/config/plugins/$plugin";
$runs_dir   = "$config_dir/runs";

header("Content-Type: text/plain; charset=utf-8");
header("Cache-Control: no-store");

$blocks = [];   // each element = array of key=value lines (one block)

// 1. Final per-device blocks from the newest run log.  Parsed with a
//    streaming fgets loop: only the in-progress block is held in memory, so
//    a huge log (mass corruption -> millions of MISMATCH lines) costs no
//    more than one block of RAM regardless of log size.  The same pass also
//    picks up the run's `finished:` marker (scrub.sh's run outcome label)
//    and the most recent `scrubbing <device>` line (the active device,
//    useful when the live server is disabled).
$run_log   = "";
$outcome   = "";
$active    = "";
$candidates = glob("$runs_dir/run-*.log") ?: [];
if ($candidates) {
    usort($candidates, fn($a, $b) => filemtime($b) <=> filemtime($a));
    $run_log = $candidates[0];
}
if ($run_log !== "" && is_file($run_log)) {
    $fp = fopen($run_log, "r");
    if ($fp) {
        $cur = null;   // in-progress status block (device => kv map)
        while (($line = fgets($fp)) !== false) {
            $line = rtrim($line);
            if ($line === "status:") {
                if ($cur !== null) { $blocks[] = $cur; }
                $cur = [];
                continue;
            }
            if ($cur !== null) {
                if (preg_match('/^([a-z_][a-z0-9_]*)=(.*)$/', $line, $m)) {
                    $cur[$m[1]] = $m[2];
                    continue;
                }
                $blocks[] = $cur;   // non-key=value line ends the block
                $cur = null;
            }
            // scrub.sh's per-run outcome marker: "[ts] finished: OK"
            if (preg_match('/finished:\s*(.+)$/', $line, $m)) {
                $outcome = $m[1];
            }
            // scrub.sh's per-device start marker: "[ts] (1/2) scrubbing /dev/sdb"
            if (preg_match('/scrubbing\s+(\S+)/', $line, $m)) {
                $active = $m[1];
            }
        }
        if ($cur !== null) { $blocks[] = $cur; }
        fclose($fp);
    }
}

// 2. Live block from the running scrub's status server (empty while idle).
$live = trim(shell_exec("$plugin_dir/scripts/scrub.sh status 2>/dev/null") ?: '');
if ($live !== '') {
    $blocks[] = array_filter(array_map('trim', explode("\n", $live)));
}

// 3. Meta block.  `running` comes from the run lock (works for manual AND
//    scheduled runs); `active_device` is only meaningful while running.
$running = trim(shell_exec("$plugin_dir/scripts/scrub.sh running 2>/dev/null")) === '1';
$meta = ["running=" . ($running ? "1" : "0")];
if ($run_log !== "") {
    $meta[] = "run_log=" . basename($run_log);
}
if ($outcome !== "") {
    $meta[] = "run_outcome=" . $outcome;
}
if ($running && $active !== "") {
    $meta[] = "active_device=" . $active;
}

// Blank-line-separated blocks; the meta block is always last.
$out = [];
foreach ($blocks as $b) {
    $out[] = implode("\n", $b);
}
$out[] = implode("\n", $meta);
echo implode("\n\n", $out), "\n";
