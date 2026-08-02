#!/bin/bash
# pkg_build.sh — package the plugin source tree into a Slackware .txz bundle.
#
# Mirrors the rathole-unraid layout: everything under this directory is
# copied (preserving relative paths) into a temp tree, tar'd into
#   ../../archive/<plugin>-x86_64-1.txz
# and the resulting bundle is what the .plg installs via upgradepkg.
#
# Unlike rathole we do NOT download a binary at install time — the
# scrub-rs / craft-corrupt binaries are copied into
#   usr/local/emhttp/plugins/btrfs-integrity-recovery/bin/
# by CI (build-plugin.yml) before this script runs, so they are bundled.

DIR="$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")"
tmpdir=/tmp/tmp.$(( $RANDOM * 19318203981230 + 40 ))
plugin=$(basename "${DIR}")
archive="$(dirname "$(dirname "${DIR}")")/archive"

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
# Fixed bundle name so the .plg's bundleURL (releases/latest/download/...) stays stable.
# NOTE: do NOT pass --mode here. The --mode="a=r,u+w,a+X" form strips the
# execute bit from every file (including scripts/install.sh and
# scripts/uninstall.sh), which makes the .plg install/remove hooks fail with
# exit 126 ("/bin/bash returned 126"). The chmod -R +x above already set the
# correct perms, so we preserve them by omitting --mode entirely.
tar cfJCo "${archive}/${plugin}-x86_64-1.txz" "$tmpdir" . --owner=0 --group=0
rm -rf "$tmpdir"

echo "Built ${archive}/${plugin}-x86_64-1.txz"
md5sum "${archive}/${plugin}-x86_64-1.txz"
