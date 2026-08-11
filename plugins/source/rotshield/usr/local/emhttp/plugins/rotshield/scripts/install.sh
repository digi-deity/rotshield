#!/bin/bash
# scripts/install.sh — run by the .plg install step.
#
# The bundle (installed just before this script runs via upgradepkg) has
# already placed every file, including the prebuilt binary at
#   ${PLUGIN_DIR}/bin/scrub-rs
# so this script only fixes permissions, writes a default config and
# applies the schedule. No binary download happens here. (craft-corrupt,
# the test-only corruption injector, is never part of the bundle.)

set -u
PLUGIN="rotshield"
PLUGIN_DIR="/usr/local/emhttp/plugins/${PLUGIN}"
CONFIG_DIR="/boot/config/plugins/${PLUGIN}"
CONFIG_FILE="${CONFIG_DIR}/config.cfg"
RC="/etc/rc.d/rc.${PLUGIN}"
# $1 = current plugin version, passed by the .plg (install.sh &version;),
# used to prune stale bundles from the flash drive below.
VERSION="${1:-}"

# Make the shipped binary and rc script executable / owned by root.
chown root:root "${PLUGIN_DIR}/bin/scrub-rs" 2>/dev/null
chmod 755        "${PLUGIN_DIR}/bin/scrub-rs" 2>/dev/null
chown root:root "${RC}"
chmod 755        "${RC}"

# Default config (INI-style so both bash and PHP can read it unquoted).
mkdir -p "${CONFIG_DIR}"
if [ ! -f "${CONFIG_FILE}" ]; then
  # Defaults written single-quoted for string values so the file parses as
  # BOTH PHP INI and a bash `source`d file (see the Settings page for why).
  # No target is preselected: /dev/nmd1p1 (the array partition) is not a
  # discoverable option, and the Settings page + run() both refuse to
  # invent a target — the user picks disks on first visit.
  cat > "${CONFIG_FILE}" <<'EOF'
DEVICES=''
DEVICE=''
SCHEDULE='disabled'
CRON=''
RECOVER=1
WRITE=0
EXTRA_OPTIONS=
EOF
  chmod 644 "${CONFIG_FILE}"
fi

# Prune stale bundles: the bundle filename is versioned, so keep only the
# bundle matching the installed version and drop older ones (they would
# otherwise accumulate on the flash drive across updates). Only act when
# $1 looks like a real version (guards against a missing/unexpanded
# argument — then we simply leave the bundles alone).
case "${VERSION}" in
  ''|*[!0-9A-Za-z._-]*)
    ;;
  *)
    mkdir -p "${CONFIG_DIR}/install"
    find "${CONFIG_DIR}/install" -maxdepth 1 -type f \
      -name "${PLUGIN}-*.txz" \
      ! -name "${PLUGIN}-${VERSION}-x86_64-1.txz" \
      -delete 2>/dev/null
    ;;
esac

# Apply schedule / hooks.
"${RC}" restart

echo ""
echo "-----------------------------------------------------------"
echo " rotshield has been installed."
echo " Open Settings -> Rotshield to run a scrub."
echo "-----------------------------------------------------------"
echo ""
