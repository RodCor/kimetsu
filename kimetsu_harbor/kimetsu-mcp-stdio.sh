#!/usr/bin/env bash
set -euo pipefail

script_dir="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
repo_root="$(CDPATH= cd -- "$script_dir/.." && pwd -P)"

kimetsu_bin="${KIMETSU_BIN:-}"
if [[ -z "$kimetsu_bin" ]]; then
  if [[ -x "$repo_root/target/x86_64-unknown-linux-musl/release/kimetsu" ]]; then
    kimetsu_bin="$repo_root/target/x86_64-unknown-linux-musl/release/kimetsu"
  else
    kimetsu_bin="$repo_root/target/release/kimetsu"
  fi
fi

exec "$kimetsu_bin" mcp serve --workspace "$repo_root"
