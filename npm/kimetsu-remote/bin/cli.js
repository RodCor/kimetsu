#!/usr/bin/env node
"use strict";

// Launcher for the `kimetsu-remote` npm package — the server-hosted Kimetsu
// brain (HTTP MCP). It is a SEPARATE package from `kimetsu-ai` (the CLI); the
// remote server is intentionally not bundled with the CLI.
//
// The native binary ships through per-platform optionalDependencies
// (@kimetsu-ai/remote-<platform>-<arch>) built with `--features embeddings,tls`.
// npm installs only the one matching the host; this launcher execs its binary,
// forwarding all args, stdio, and the exit code.

const { spawnSync } = require("child_process");

// key = `${process.platform}-${process.arch}`. Only the targets with an ONNX
// Runtime prebuilt get an embeddings+tls server binary (mirrors the embeddings
// flavor in release.yml). Elsewhere: `cargo install kimetsu-remote`.
const PLATFORMS = {
  "linux-x64": { pkg: "@kimetsu-ai/remote-linux-x64", bin: "kimetsu-remote" },
  "darwin-arm64": { pkg: "@kimetsu-ai/remote-darwin-arm64", bin: "kimetsu-remote" },
  "win32-x64": { pkg: "@kimetsu-ai/remote-win32-x64", bin: "kimetsu-remote.exe" },
};

const REPO_URL = "https://github.com/RodCor/kimetsu";

function fail(message) {
  process.stderr.write(`kimetsu-remote: ${message}\n`);
  process.exit(1);
}

const key = `${process.platform}-${process.arch}`;
const entry = PLATFORMS[key];
if (!entry) {
  fail(
    `no prebuilt kimetsu-remote binary for ${key} (${process.platform}/${process.arch}).\n` +
      `Prebuilt npm binaries cover: ${Object.keys(PLATFORMS).join(", ")}.\n` +
      `Install another way:\n` +
      `  - cargo install kimetsu-remote --features embeddings\n` +
      `  - grab a kimetsu-remote archive from ${REPO_URL}/releases`
  );
}

let binPath;
try {
  binPath = require.resolve(`${entry.pkg}/bin/${entry.bin}`);
} catch (_err) {
  fail(
    `the platform package ${entry.pkg} is not installed.\n` +
      `npm may have skipped optional dependencies (e.g. --no-optional or\n` +
      `--ignore-scripts). Reinstall with optional deps enabled:\n` +
      `  npm install -g kimetsu-remote\n` +
      `Or: cargo install kimetsu-remote --features embeddings, or an archive\n` +
      `from ${REPO_URL}/releases`
  );
}

const result = spawnSync(binPath, process.argv.slice(2), { stdio: "inherit" });
if (result.error) {
  fail(`failed to launch the server binary: ${result.error.message}`);
}
process.exit(result.status === null ? 1 : result.status);
