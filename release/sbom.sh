#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: $0 <release-dist-directory>" >&2
  exit 2
fi

dist=$1
command -v syft >/dev/null || { echo "syft is required" >&2; exit 1; }
shopt -s nullglob
archives=("$dist"/*.tar.gz)
((${#archives[@]})) || { echo "no release archives in $dist" >&2; exit 1; }
for archive in "${archives[@]}"; do
  base=$(basename "$archive" .tar.gz)
  syft "$archive" -o "spdx-json=$dist/$base.spdx.json"
done
