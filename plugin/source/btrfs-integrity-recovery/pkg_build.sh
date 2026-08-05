#!/bin/bash
# pkg_build.sh — package the plugin source tree into a Slackware .txz bundle.
#
# Mirrors the rathole-unraid layout: everything under this directory is
# copied (preserving relative paths) into a temp tree, tar'd into
#   ../../archive/<plugin>-<version>-x86_64-1.txz
# and the resulting bundle is what the .plg installs via upgradepkg.
#
# The bundle name is VERSIONED with the version from the .plg:
# unRAID's plugin manager never re-downloads a bundle file that already
# exists on the flash drive, so a versioned filename is what makes
# updates actually refresh the shipped files. The .plg derives its FILE
# Name and bundleURL from the same version entity, so we verify below
# that the .plg references exactly the bundle we are about to build.
#
# Unlike rathole we do NOT download a binary at install time — the
# scrub-rs / craft-corrupt binaries are copied into
#   usr/local/emhttp/plugins/btrfs-integrity-recovery/bin/
# by CI (build-plugin.yml) before this script runs, so they are bundled.

DIR="$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")"
PLUGIN_DIR="$(dirname "$(dirname "${DIR}")")"          # .../plugin
PLG="${PLUGIN_DIR}/btrfs-integrity-recovery.plg"
tmpdir=/tmp/tmp.$(( $RANDOM * 19318203981230 + 40 ))
plugin=$(basename "${DIR}")
archive="$(dirname "$(dirname "${DIR}")")/archive"

# The bundle version IS the <!ENTITY version> of the .plg — single source
# of truth for the plg, the bundle filename and the release asset name.
VERSION="$(sed -n 's/.*<!ENTITY version[[:space:]]*"\([^"]*\)".*/\1/p' "${PLG}" | head -1)"
if [ -z "${VERSION}" ]; then
  echo "FATAL: cannot read <!ENTITY version> from ${PLG}" >&2
  exit 1
fi
bundle="${plugin}-${VERSION}-x86_64-1.txz"

# Guard: the .plg must reference exactly this bundle (its bundlefile
# entity embeds &version;), otherwise the released asset would not match
# the .plg's FILE Name / bundleURL and updates would 404 or go stale.
grep -q "btrfs-integrity-recovery-&version;-x86_64-1.txz" "${PLG}" || {
  echo "FATAL: ${PLG} does not define the versioned bundlefile entity" >&2
  exit 1
}
grep -q "&bundlefile;" "${PLG}" || {
  echo "FATAL: ${PLG} does not use &bundlefile; in the bundle FILE element" >&2
  exit 1
}

mkdir -p "$tmpdir" "$archive"

cd "$DIR" || { echo "FATAL: cannot cd to $DIR"; exit 1; }
# Copy every file except the build script and the .gitkeep placeholders.
# The find output is intentionally unquoted so each relative path is passed
# as a separate argument to cp (word-splitting is what we want here); the
# tree is ours and contains no whitespace/special-char names.
# shellcheck disable=SC2046
cp --parents -f $(find . -type f ! \( -iname "pkg_build.sh" -o -iname ".gitkeep" \) ) "$tmpdir/"
cd "$tmpdir" || { echo "FATAL: cannot cd to $tmpdir"; exit 1; }
# Normalise line endings and make scripts executable.
find . -type f \( -iname '*.sh' -o -iname '*.page' -o -iname '*.plg' -o -iname 'rc.*' \) -exec sed -i 's/\r//g' {} +
chmod -R +x ./
# NOTE: do NOT pass --mode here. The --mode="a=r,u+w,a+X" form strips the
# execute bit from every file (including scripts/install.sh and
# scripts/uninstall.sh), which makes the .plg install/remove hooks fail with
# exit 126 ("/bin/bash returned 126"). The chmod -R +x above already set the
# correct perms, so we preserve them by omitting --mode entirely.
tar cfJCo "${archive}/${bundle}" "$tmpdir" . --owner=0 --group=0
rm -rf "$tmpdir"

echo "Built ${archive}/${bundle}"
md5sum "${archive}/${bundle}"
