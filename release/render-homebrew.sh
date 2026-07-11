#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: $0 <X.Y.Z>" >&2
  exit 2
fi
version=$1
[[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+([+-][0-9A-Za-z.-]+)?$ ]] || {
  echo "invalid version: $version" >&2
  exit 2
}
root=$(cd "$(dirname "$0")/.." && pwd)
dist="$root/release/dist"
sums="$dist/SHA256SUMS"
template="$root/release/codex-for-grok.rb.in"
out="$dist/codex-for-grok.rb"
[[ -f $sums ]] || { echo "missing $sums" >&2; exit 1; }

checksum() {
  local target=$1
  local file="codex-for-grok-$version-$target.tar.gz"
  awk -v file="$file" '$2 == file || $2 == "*" file {print $1}' "$sums"
}

arm_macos=$(checksum aarch64-apple-darwin)
intel_macos=$(checksum x86_64-apple-darwin)
arm_linux=$(checksum aarch64-unknown-linux-gnu)
intel_linux=$(checksum x86_64-unknown-linux-gnu)
for value in "$arm_macos" "$intel_macos" "$arm_linux" "$intel_linux"; do
  [[ $value =~ ^[0-9a-fA-F]{64}$ ]] || { echo "missing or invalid archive checksum" >&2; exit 1; }
done

sed -e "s/@VERSION@/$version/g" \
  -e "s/@SHA_AARCH64_APPLE_DARWIN@/$arm_macos/g" \
  -e "s/@SHA_X86_64_APPLE_DARWIN@/$intel_macos/g" \
  -e "s/@SHA_AARCH64_UNKNOWN_LINUX_GNU@/$arm_linux/g" \
  -e "s/@SHA_X86_64_UNKNOWN_LINUX_GNU@/$intel_linux/g" \
  "$template" > "$out"
echo "$out"
