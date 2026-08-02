<?php
/* status.php — live-status endpoint for the btrfs-integrity-recovery page.
 *
 * Polled by the Settings page (every 2 s) to show the running scrub's live
 * error counters.  It simply relays `scrub.sh status`, which curls the
 * running scrub-rs process's localhost status server (127.0.0.1:<port>) and
 * returns the shell-parsable `key=value` payload.  No user input reaches this
 * script, so there is nothing to sanitise; it is read-only.
 */
$plugin     = "btrfs-integrity-recovery";
$plugin_dir = "/usr/local/emhttp/plugins/$plugin";

header("Content-Type: text/plain; charset=utf-8");
header("Cache-Control: no-store");

$payload = trim(shell_exec("$plugin_dir/scripts/scrub.sh status 2>/dev/null") ?: '');
if ($payload !== '') {
    echo $payload;
} else {
    // Idle (no scrub running / status disabled / port busy) — empty body is
    // the contract the page treats as "no live data".
    echo "";
}
