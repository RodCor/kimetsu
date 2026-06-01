# kimetsu

A persistent memory **brain** sidecar for Claude Code and Codex. It accumulates
generalizable knowledge across sessions and retrieves it on demand.

This npm package installs the prebuilt native `kimetsu` binary for your platform —
**no Rust toolchain required**. It's the same binary published on
[GitHub Releases](https://github.com/RodCor/kimetsu/releases) and via
`cargo install kimetsu-cli`.

## Install

```bash
npm install -g kimetsu
kimetsu --version
kimetsu doctor      # checks paths, brain.db, embedder, MCP, bridge
```

npm downloads only the platform package that matches your OS/CPU
(`@kimetsu/linux-x64`, `@kimetsu/darwin-x64`, `@kimetsu/darwin-arm64`,
`@kimetsu/win32-x64`) via `optionalDependencies`. There is **no postinstall
download** — it works under `npm install --ignore-scripts`.

### Semantic (embeddings) build

The default install is the **lean** build: fast lexical (FTS) retrieval, no model
download. To opt into the semantic build (fastembed + ONNX; first run downloads
BGE-small), set `KIMETSU_NPM_FLAVOR=embeddings`:

```bash
KIMETSU_NPM_FLAVOR=embeddings npm install -g kimetsu
```

With that env var set, the launcher fetches and caches the embeddings binary from
the matching GitHub Release on first run. Embeddings prebuilts exist for
**Linux x64, macOS Apple Silicon, and Windows x64** (the targets ONNX Runtime
ships prebuilts for); elsewhere the launcher falls back to the lean build.

## Supported platforms

| OS            | Arch  | Lean | Embeddings |
|---------------|-------|------|------------|
| Linux         | x64   | ✅   | ✅         |
| macOS (Intel) | x64   | ✅   | ❌         |
| macOS (Apple) | arm64 | ✅   | ✅         |
| Windows       | x64   | ✅   | ✅         |

On unsupported platforms, install with `cargo install kimetsu-cli` or grab an
archive from the [releases page](https://github.com/RodCor/kimetsu/releases).

## Links

- Source & full docs: <https://github.com/RodCor/kimetsu>
- License: MIT OR Apache-2.0
