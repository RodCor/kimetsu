#!/usr/bin/env bash
# Run from the verified release-source checkout, with flat release assets in dist/.
set -euo pipefail
VERSION="${1:?stable release version required}"
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || exit 1

# npm-key  target-triple  os  cpu  binname  ext
PLATFORMS=(
  "linux-x64    x86_64-unknown-linux-gnu  linux   x64    kimetsu      tar.gz"
  "darwin-x64   x86_64-apple-darwin       darwin  x64    kimetsu      tar.gz"
  "darwin-arm64 aarch64-apple-darwin      darwin  arm64  kimetsu      tar.gz"
  "win32-x64    x86_64-pc-windows-msvc     win32   x64    kimetsu.exe  zip"
)

mkdir -p stage
for row in "${PLATFORMS[@]}"; do
  read -r key target osv cpuv binname ext <<<"$row"
  stem="kimetsu-${VERSION}-${target}-lean"
  archive="dist/${stem}.${ext}"
  if [ ! -f "$archive" ]; then
    echo "::error::missing lean artifact for ${target}: ${archive}"
    exit 1
  fi

  tmp="$(mktemp -d)"
  if [ "$ext" = "zip" ]; then
    # PowerShell `Compress-Archive` writes ZIP entries with backslash
    # separators; Info-ZIP `unzip` flattens those into a literal
    # filename so the binary can't be found by path. `7z` (preinstalled
    # on ubuntu-latest) normalizes backslashes into real directories.
    7z x -y -o"$tmp" "$archive" >/dev/null
  else
    tar -xf "$archive" -C "$tmp"
  fi

  pkgdir="stage/${key}"
  mkdir -p "$pkgdir/bin"
  # Prefer the expected layout (`<stem>/<binname>`), then fall back to
  # a recursive search so the step is robust to archive quirks.
  if [ -f "$tmp/${stem}/${binname}" ]; then
    src="$tmp/${stem}/${binname}"
  elif [ -f "$tmp/${binname}" ]; then
    src="$tmp/${binname}"
  else
    # Exact match, then suffix match — the latter catches a
    # backslash-flattened basename like `<stem>\kimetsu.exe` if some
    # extractor still mangles the Windows archive.
    src="$(find "$tmp" -type f -name "${binname}" | head -n1)"
    [ -z "$src" ] && src="$(find "$tmp" -type f -name "*${binname}" | head -n1)"
  fi
  if [ -z "${src:-}" ] || [ ! -f "$src" ]; then
    echo "::error::binary ${binname} not found inside ${archive}"
    echo "::error::extracted tree:"; find "$tmp" -maxdepth 3 -print
    exit 1
  fi
  cp "$src" "$pkgdir/bin/${binname}"
  [ "$ext" = "zip" ] || chmod +x "$pkgdir/bin/${binname}"

  node -e '
    const fs = require("fs");
    const [dir, name, version, osv, cpuv] = process.argv.slice(1);
    fs.writeFileSync(dir + "/package.json", JSON.stringify({
      name,
      version,
      description: name + " — prebuilt kimetsu binary",
      repository: { type: "git", url: "git+https://github.com/RodCor/kimetsu.git" },
      license: "MIT OR Apache-2.0",
      os: [osv],
      cpu: [cpuv],
      files: ["bin"],
    }, null, 2) + "\n");
  ' "$pkgdir" "@kimetsu-ai/${key}" "$VERSION" "$osv" "$cpuv"

done

node -e '
  const fs = require("fs");
  const version = process.argv[1];
  const p = "npm/kimetsu/package.json";
  const j = JSON.parse(fs.readFileSync(p, "utf8"));
  j.version = version;
  for (const k of Object.keys(j.optionalDependencies || {})) {
    j.optionalDependencies[k] = version;
  }
  fs.writeFileSync(p, JSON.stringify(j, null, 2) + "\n");
' "$VERSION"

# The server ships only where an embeddings build exists (ONNX prebuilts).
# npm-key  target-triple  os  cpu  binname  ext
PLATFORMS=(
  "linux-x64    x86_64-unknown-linux-gnu  linux   x64    kimetsu-remote      tar.gz"
  "darwin-arm64 aarch64-apple-darwin      darwin  arm64  kimetsu-remote      tar.gz"
  "win32-x64    x86_64-pc-windows-msvc     win32   x64    kimetsu-remote.exe  zip"
)

mkdir -p stage-remote
for row in "${PLATFORMS[@]}"; do
  read -r key target osv cpuv binname ext <<<"$row"
  stem="kimetsu-remote-${VERSION}-${target}"
  # The remote archive is uploaded alongside the embeddings kimetsu archive.
  archive="dist/${stem}.${ext}"
  if [ ! -f "$archive" ]; then
    echo "::error::missing kimetsu-remote artifact for ${target}: ${archive}"
    exit 1
  fi

  tmp="$(mktemp -d)"
  if [ "$ext" = "zip" ]; then
    7z x -y -o"$tmp" "$archive" >/dev/null
  else
    tar -xf "$archive" -C "$tmp"
  fi

  pkgdir="stage-remote/${key}"
  mkdir -p "$pkgdir/bin"
  if [ -f "$tmp/${stem}/${binname}" ]; then
    src="$tmp/${stem}/${binname}"
  elif [ -f "$tmp/${binname}" ]; then
    src="$tmp/${binname}"
  else
    src="$(find "$tmp" -type f -name "${binname}" | head -n1)"
    [ -z "$src" ] && src="$(find "$tmp" -type f -name "*${binname}" | head -n1)"
  fi
  if [ -z "${src:-}" ] || [ ! -f "$src" ]; then
    echo "::error::binary ${binname} not found inside ${archive}"
    find "$tmp" -maxdepth 3 -print
    exit 1
  fi
  cp "$src" "$pkgdir/bin/${binname}"
  [ "$ext" = "zip" ] || chmod +x "$pkgdir/bin/${binname}"

  node -e '
    const fs = require("fs");
    const [dir, name, version, osv, cpuv] = process.argv.slice(1);
    fs.writeFileSync(dir + "/package.json", JSON.stringify({
      name,
      version,
      description: name + " — prebuilt kimetsu-remote server binary",
      repository: { type: "git", url: "git+https://github.com/RodCor/kimetsu.git" },
      license: "MIT OR Apache-2.0",
      os: [osv],
      cpu: [cpuv],
      files: ["bin"],
    }, null, 2) + "\n");
  ' "$pkgdir" "@kimetsu-ai/remote-${key}" "$VERSION" "$osv" "$cpuv"

done

node -e '
  const fs = require("fs");
  const version = process.argv[1];
  const p = "npm/kimetsu-remote/package.json";
  const j = JSON.parse(fs.readFileSync(p, "utf8"));
  j.version = version;
  for (const k of Object.keys(j.optionalDependencies || {})) {
    j.optionalDependencies[k] = version;
  }
  fs.writeFileSync(p, JSON.stringify(j, null, 2) + "\n");
' "$VERSION"
