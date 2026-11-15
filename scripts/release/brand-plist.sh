#!/usr/bin/env bash
# Renames Zed to Icarus in an app bundle's Info.plist strings: the permission prompts and the
# document type name, which come from Zed's app/crates/zed/resources/info/*.plist.
#
#   scripts/release/brand-plist.sh <Info.plist>
set -euo pipefail

if [[ $# -ne 1 || ! -f $1 ]]; then
  echo "usage: $0 <Info.plist>" >&2
  exit 2
fi
plist=$1

plutil -convert xml1 "$plist"
branded=$(mktemp)
trap 'rm -f "$branded"' EXIT
# Whole words only, so identifiers such as dev.zed.Zed-Dev or zed-cli stay as they are.
sed -E '/<string>/s/(^|[^[:alnum:].-])Zed([^[:alnum:].-]|$)/\1Icarus\2/g' "$plist" >"$branded"
plutil -lint "$branded" >/dev/null
cat "$branded" >"$plist"

if grep -E '<string>([^<]*[^[:alnum:].-])?Zed([^[:alnum:].-][^<]*)?</string>' "$plist"; then
  echo "$plist still names Zed in the strings above" >&2
  exit 1
fi
