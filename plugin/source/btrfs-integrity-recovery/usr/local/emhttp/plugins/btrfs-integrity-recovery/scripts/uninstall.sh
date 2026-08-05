#!/bin/bash
# scripts/uninstall.sh — run by the .plg remove step.
set -u
PLUGIN="btrfs-integrity-recovery"
PLUGIN_DIR="/usr/local/emhttp/plugins/${PLUGIN}"
CONFIG_DIR="/boot/config/plugins/${PLUGIN}"
RC="/etc/rc.d/rc.${PLUGIN}"

# 1. Stop the schedule (clears the cron.d entry) before touching anything.
"${RC}" stop 2>/dev/null
rm -f /etc/cron.d/${PLUGIN} 2>/dev/null

# 2. Remove the Slackware package(s). The bundle name is versioned
#    (btrfs-integrity-recovery-<version>-x86_64-1.txz), so iterate the
#    package DB and remove every installed version of the plugin package.
for pkg in /var/log/packages/${PLUGIN}-*; do
  [ -f "$pkg" ] && removepkg "$(basename "$pkg")" &>/dev/null
done

# 3. Belt-and-suspenders: drop any leftover package DB entries (covers both
#    the legacy /var/log/packages and the pkgtools location) in case
#    removepkg above did not find the package.
rm -f /var/log/packages/${PLUGIN}-* 2>/dev/null
rm -f /var/lib/pkgtools/packages/${PLUGIN}-* 2>/dev/null

# 4. Remove the plugin tree and the persisted config / runtime state.
rm -rf "${PLUGIN_DIR}"
rm -rf "${CONFIG_DIR}"
rm -f /var/log/${PLUGIN}.log

echo ""
echo " btrfs-integrity-recovery has been uninstalled."
echo ""
