#!/usr/bin/env node
"use strict";

// Launcher for the `kimetsu` npm package.
//
// Kimetsu is a native Rust binary. npm ships it through per-platform
// optionalDependencies (@kimetsu-ai/<platform>-<arch>) — npm installs only the
// one whose os/cpu match the host, and this launcher execs its binary,
// forwarding all args, stdio, and the exit code.
//
// The platform/arch -> target-triple mapping mirrors `release_target()`,
// `binary_name()`, and `embeddings_assets_supported()` in
// crates/kimetsu-cli/src/update.rs, so npm behaves identically to the
// built-in `kimetsu update` self-updater.

const { spawnSync } = require("child_process");

// key = `${process.platform}-${process.arch}`
const PLATFORMS = {
  "linux-x64": {
    pkg: "@kimetsu-ai/linux-x64",
    target: "x86_64-unknown-linux-gnu",
    bin: "kimetsu",
    embeddings: true,
  },
  "darwin-x64": {
    pkg: "@kimetsu-ai/darwin-x64",
    target: "x86_64-apple-darwin",
    bin: "kimetsu",
    embeddings: false, // ort ships no prebuilt ONNX Runtime for x86_64 macOS
  },
  "darwin-arm64": {
    pkg: "@kimetsu-ai/darwin-arm64",
    target: "aarch64-apple-darwin",
    bin: "kimetsu",
    embeddings: true,
  },
  "win32-x64": {
    pkg: "@kimetsu-ai/win32-x64",
    target: "x86_64-pc-windows-msvc",
    bin: "kimetsu.exe",
    embeddings: true,
  },
};

const VERSION = require("../package.json").version;
const REPO_URL = "https://github.com/RodCor/kimetsu";

function fail(message) {
  process.stderr.write(`kimetsu: ${message}\n`);
  process.exit(1);
}

function unsupportedPlatformMessage(key) {
  return (
    `no prebuilt Kimetsu binary for ${key} (${process.platform}/${process.arch}).\n` +
    `Prebuilt npm binaries cover: ${Object.keys(PLATFORMS).join(", ")}.\n` +
    `Install another way:\n` +
    `  - cargo install kimetsu-cli\n` +
    `  - grab an archive from ${REPO_URL}/releases`
  );
}

function missingOptionalDepMessage(entry) {
  return (
    `the platform package ${entry.pkg} is not installed.\n` +
    `This usually means npm skipped optional dependencies (e.g. --no-optional or\n` +
    `--ignore-scripts on an older npm). Reinstall with optional deps enabled:\n` +
    `  npm install -g kimetsu\n` +
    `Or install another way: cargo install kimetsu-cli, or an archive from\n` +
    `${REPO_URL}/releases`
  );
}

function leanBinaryPath(entry) {
  try {
    return require.resolve(`${entry.pkg}/bin/${entry.bin}`);
  } catch (_err) {
    return null;
  }
}

async function resolveBinary() {
  const key = `${process.platform}-${process.arch}`;
  const entry = PLATFORMS[key];
  if (!entry) {
    fail(unsupportedPlatformMessage(key));
  }

  const wantEmbeddings =
    (process.env.KIMETSU_NPM_FLAVOR || "").toLowerCase() === "embeddings";

  if (wantEmbeddings) {
    if (!entry.embeddings) {
      process.stderr.write(
        `kimetsu: embeddings build is not available for ${entry.target}; ` +
          `using the lean build.\n`
      );
    } else {
      try {
        const { ensureEmbeddingsBinary } = require("../lib/embeddings");
        const embeddingsBin = await ensureEmbeddingsBinary({
          version: VERSION,
          target: entry.target,
          binName: entry.bin,
        });
        if (embeddingsBin) return embeddingsBin;
      } catch (err) {
        process.stderr.write(
          `kimetsu: could not obtain embeddings build (${err.message}); ` +
            `falling back to the lean build.\n`
        );
      }
    }
  }

  const lean = leanBinaryPath(entry);
  if (!lean) {
    fail(missingOptionalDepMessage(entry));
  }
  return lean;
}

async function main() {
  const binary = await resolveBinary();
  const result = spawnSync(binary, process.argv.slice(2), {
    stdio: "inherit",
    windowsHide: true,
  });

  if (result.error) {
    fail(`failed to launch ${binary}: ${result.error.message}`);
  }
  // Propagate a fatal signal as the conventional 128+signal exit code.
  if (result.signal) {
    process.exit(1);
  }
  process.exit(result.status === null ? 1 : result.status);
}

main();
