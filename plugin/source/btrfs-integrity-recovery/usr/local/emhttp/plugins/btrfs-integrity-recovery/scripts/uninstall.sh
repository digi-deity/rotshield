#!/bin/bash
# scripts/uninstall.sh — run by the .plg remove step.
set -u
PLUGIN="btrfs-integrity-recovery"
PLUGIN_DIR="/usr/local/emhttp/plugins/${PLUGIN}"
CONFIG_DIR="/boot/config/plugins/${PLUGIN}"
RC="/etc/rc.d/rc.${PLUGIN}"

"${RC}" stop 2>/dev/null
rm -f /etc/cron.d/${PLUGIN} 2>/dev/null
rm -rf "${CONFIG_DIR}"
rm -rf "${PLUGIN_DIR}"
rm -f /var/log/${PLUGIN}.log

removepkg ${PLUGIN}-*-x86_64-1 &>/dev/null
rm -f /var/lib/pkgtools/packages/${PLUGIN}-*-x86_64-1

echo ""
echo " btrfs-integrity-recovery has been uninstalled."
echo ""
