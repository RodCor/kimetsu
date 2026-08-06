#!/usr/bin/env bash
# Copy the canonical Pi integration assets into a kimetsu-pi checkout.
#
# The Pi extension lives in two places: this repo embeds it with include_str!
# so `kimetsu plugin install pi` can write it, and the kimetsu-pi npm package
# publishes it for `pi install kimetsu-pi`. This repo is the source of truth;
# CI (the `integration asset drift` job) fails the build when the two diverge.
#
#   usage: scripts/sync-pi-package.sh [path-to-kimetsu-pi]   (default: ../kimetsu-pi)
#
# Then commit the result in the kimetsu-pi checkout.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
target="${1:-$(dirname "$repo_root")/kimetsu-pi}"

if [[ ! -d "$target" ]]; then
  echo "error: no kimetsu-pi checkout at $target" >&2
  echo "usage: $0 [path-to-kimetsu-pi]" >&2
  exit 1
fi

copy() {
  local from="$1" to="$2"
  mkdir -p "$(dirname "$to")"
  if cmp -s "$from" "$to"; then
    echo "  unchanged  ${to#"$target"/}"
  else
    cp "$from" "$to"
    echo "  synced     ${to#"$target"/}"
  fi
}

echo "syncing $repo_root -> $target"
copy "$repo_root/crates/kimetsu-chat/assets/pi-extension.ts" "$target/extensions/kimetsu.ts"
echo "done. Commit the changes in $target."
