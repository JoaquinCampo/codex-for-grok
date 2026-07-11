#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: $0 <vX.Y.Z> <rust-target>" >&2
  exit 2
fi

tag=$1
target=$2
version=${tag#v}
[[ $tag == "v$version" && $version =~ ^[0-9]+\.[0-9]+\.[0-9]+([+-][0-9A-Za-z.-]+)?$ ]] || {
  echo "invalid release tag: $tag" >&2
  exit 2
}

root=$(cd "$(dirname "$0")/.." && pwd)
binary="$root/target/$target/release/grok-codex-bridge"
[[ -x $binary ]] || { echo "missing executable: $binary" >&2; exit 1; }

dist="$root/release/dist"
name="grok-codex-bridge-$version-$target"
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
mkdir -p "$dist" "$stage/$name"
install -m 0755 "$binary" "$stage/$name/grok-codex-bridge"
install -m 0644 "$root/README.md" "$root/LICENSE" "$root/COMPATIBILITY.md" "$stage/$name/"

tar -C "$stage" -czf "$dist/$name.tar.gz" "$name"
echo "$dist/$name.tar.gz"
