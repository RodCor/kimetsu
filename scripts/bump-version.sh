#!/usr/bin/env bash
#
# Bump the workspace version everywhere a release needs it, in one step, so a
# tag can never ship artifacts that self-report a stale version.
#
# Lesson this encodes (v1.5.0 botch): the tag v1.5.0 was pushed while the
# workspace version was still 1.0.0. Result — every binary self-reported
# 1.0.0, the crates.io publish failed (`kimetsu-core@1.0.0 already exists`),
# and npm published the stale binaries (npm versions can never be reused, so
# the fix had to ship as a fresh patch, v1.5.1). The release.yml `version-guard`
# job now refuses to build on a tag/version mismatch; this script is how you
# avoid tripping it.
#
# Usage:
#   scripts/bump-version.sh <new-version>     # e.g. scripts/bump-version.sh 2.0.0
#
# It updates:
#   1. the [workspace.package] version in the root Cargo.toml,
#   2. every inter-crate path-dependency `version = "…"` pin,
#   3. Cargo.lock (via `cargo update -p` for each workspace crate),
#   4. the npm package versions that track the workspace
#      (kimetsu-ai, kimetsu-remote, their optionalDependencies, and
#      @kimetsu-ai/sdk plus its package-lock).
#
# kimetsu-bus's dev-dependency on kimetsu-sdk is path-only (no version) so
# Cargo can publish the bus before the SDK. This script does not add a
# version to that line.
#
# It does NOT commit or tag — review the diff, commit (INCLUDING Cargo.lock),
# then tag `v<new-version>`.
#
# Portable across GNU and BSD/macOS sed (uses `-i.bak` + rm).
set -euo pipefail

NEW="${1:?usage: bump-version.sh <new-version>  (e.g. 2.0.0)}"
NEW="${NEW#v}" # tolerate a leading "v"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

CUR="$(grep -m1 '^version = ' Cargo.toml | sed -E 's/version = "([^"]+)"/\1/')"
if [ -z "$CUR" ]; then
  echo "ERROR: could not read current [workspace.package] version from Cargo.toml" >&2
  exit 1
fi
if [ "$CUR" = "$NEW" ]; then
  echo "workspace version is already $NEW — nothing to do"
  exit 0
fi
echo "bumping workspace version: $CUR -> $NEW"

# 1) root [workspace.package] version (only the column-0 `version = ` line)
sed -i.bak -E "s/^version = \"$CUR\"/version = \"$NEW\"/" Cargo.toml && rm -f Cargo.toml.bak

# 2) inter-crate path-dependency pins (kimetsu-x = { path = "../kimetsu-x", version = "CUR" })
for f in crates/*/Cargo.toml; do
  sed -i.bak -E "s/(kimetsu-[a-z]+ = \{ path = \"\.\.\/kimetsu-[a-z]+\", version = )\"$CUR\"/\1\"$NEW\"/g" "$f" && rm -f "$f.bak"
done

# 3) regenerate Cargo.lock entries for the workspace crates
cargo update -p kimetsu-core -p kimetsu-brain -p kimetsu-agent -p kimetsu-bus \
             -p kimetsu-sdk -p kimetsu-chat -p kimetsu-cli -p kimetsu-e2e \
             -p kimetsu-remote

# 4) npm packages that publish at the workspace version.
# Replace only the previous version string so descriptions and key order stay
# as committed. Those strings are the package version and, for the launchers,
# every optionalDependency pin.
CUR="$CUR" NEW="$NEW" python3 - <<'PY'
import json, os
from pathlib import Path

cur, new = os.environ["CUR"], os.environ["NEW"]
paths = [
    Path("npm/kimetsu/package.json"),
    Path("npm/kimetsu-remote/package.json"),
    Path("npm/kimetsu-sdk/package.json"),
    Path("npm/kimetsu-sdk/package-lock.json"),
]
for path in paths:
    data = json.loads(path.read_text())
    if data.get("version") != cur:
        raise SystemExit(f"{path} version is {data.get('version')!r}, expected {cur}")
    text = path.read_text().replace(f'"{cur}"', f'"{new}"')
    path.write_text(text)
    bumped = json.loads(path.read_text())
    if bumped["version"] != new:
        raise SystemExit(f"{path} did not land on {new}")
print(f"npm packages set to {new}")
PY

# 5) verify no stale pins survived
if grep -rn "\"$CUR\"" Cargo.toml crates/*/Cargo.toml; then
  echo "ERROR: stale \"$CUR\" version pins remain (see above)" >&2
  exit 1
fi

echo
echo "done: workspace is now $NEW."
echo "next: review the diff, commit (INCLUDING Cargo.lock), then  git tag v$NEW"
