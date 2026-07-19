#!/bin/bash
# scripts/install.sh — run by the .plg install step.
#
# The bundle (installed just before this script runs via upgradepkg) has
# already placed every file, including the prebuilt binaries at
#   ${PLUGIN_DIR}/bin/scrub-rs  and  ${PLUGIN_DIR}/bin/craft-corrupt
# so this script only fixes permissions, writes a default config and
# applies the schedule. No binary download happens here.

set -u
PLUGIN="btrfs-integrity-recovery"
PLUGIN_DIR="/usr/local/emhttp/plugins/${PLUGIN}"
CONFIG_DIR="/boot/config/plugins/${PLUGIN}"
CONFIG_FILE="${CONFIG_DIR}/config.cfg"
RC="/etc/rc.d/rc.${PLUGIN}"

# Make the shipped binaries and rc script executable / owned by root.
chown root:root "${PLUGIN_DIR}/bin/scrub-rs" "${PLUGIN_DIR}/bin/craft-corrupt" 2>/dev/null
chmod 755        "${PLUGIN_DIR}/bin/scrub-rs" "${PLUGIN_DIR}/bin/craft-corrupt" 2>/dev/null
chown root:root "${RC}"
chmod 755        "${RC}"

# Default config (INI-style so both bash and PHP can read it unquoted).
mkdir -p "${CONFIG_DIR}"
if [ ! -f "${CONFIG_FILE}" ]; then
  cat > "${CONFIG_FILE}" <<'EOF'
DEVICES=/dev/loop2
DEVICE=/dev/loop2
SCHEDULE=disabled
RECOVER=1
WRITE=0
NO_FREEZE=0
BATCH_MAX=
BATCH_IDLE=
EXTRA_OPTIONS=
EOF
  chmod 644 "${CONFIG_FILE}"
fi

# Apply schedule / hooks.
"${RC}" restart

echo ""
echo "-----------------------------------------------------------"
echo " btrfs-integrity-recovery has been installed."
echo " Open Settings -> btrfs-integrity-recovery to run a scrub."
echo "-----------------------------------------------------------"
echo ""
