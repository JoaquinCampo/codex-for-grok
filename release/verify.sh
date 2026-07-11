#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: $0 <release-dist-directory>" >&2
  exit 2
fi

dist=$(cd "$1" && pwd)
(cd "$dist" && shasum -a 256 -c SHA256SUMS)

shopt -s nullglob
archives=("$dist"/*.tar.gz)
((${#archives[@]})) || { echo "no archives found" >&2; exit 1; }
for archive in "${archives[@]}"; do
  listing=$(tar -tzf "$archive")
  [[ $listing != *"../"* && $listing != /* ]] || {
    echo "unsafe archive paths: $archive" >&2
    exit 1
  }
  count=$(printf '%s\n' "$listing" | grep -c '/codex-for-grok$')
  [[ $count -eq 1 ]] || { echo "archive lacks one bridge executable: $archive" >&2; exit 1; }
done

echo "checksums and archive layouts verified"
