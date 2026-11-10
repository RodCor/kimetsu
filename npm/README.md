# npm distribution

Kimetsu is also published to npm as the `kimetsu-ai` package (the `kimetsu`
package name was taken; the project's npm scope is `@kimetsu-ai`), so JS/TS
users can `npm install -g kimetsu-ai` without a Rust toolchain. npm ships the
**same prebuilt native binary** as the GitHub Release — it is not a
reimplementation.

The **server** (`kimetsu-remote`, beta) is published as a *separate* package —
`npm install -g kimetsu-remote` — so the `kimetsu-ai` CLI never pulls the server
binary. See `npm/kimetsu-remote/`.

## Layout

```
npm/
  kimetsu/          main CLI package — committed source (launcher, no binaries)
    bin/cli.js      resolves the platform package and execs its binary
    lib/embeddings.js  on-demand embeddings download (KIMETSU_NPM_FLAVOR=embeddings)
    package.json    optionalDependencies -> the 4 @kimetsu-ai/* platform packages
    README.md
  kimetsu-remote/   server package (beta) — separate from the CLI
    bin/cli.js      resolves @kimetsu-ai/remote-<platform> and execs kimetsu-remote
    package.json    optionalDependencies -> 3 @kimetsu-ai/remote-* packages
    README.md
  kimetsu-sdk/      @kimetsu-ai/sdk — typed TS client for Kimetsu Remote's MCP
    src/            TypeScript sources (compiled to dist/ at publish time)
    test/           node:test suite, run against the compiled dist/
    README.md
  README.md         this file
```

## `@kimetsu-ai/sdk` is a different kind of package

The two above ship a native binary. The SDK ships code: a typed client for
Kimetsu Remote's MCP surface, mirroring the Python SDK method-for-method, with
zero runtime dependencies (Node 18's `fetch`).

It exists because the TypeScript ecosystem Kimetsu integrates with — Pi
extensions, OpenClaw plugins, Cursor, VS Code — was shelling out to the binary
and parsing its text output, which is how the Pi extension and its published npm
copy drifted apart without anyone noticing. There was no shared typed surface
for them to share. Now there is.

Unlike the launcher packages, it has a build step (`tsc`). Its version tracks
the Cargo workspace, the same number as `kimetsu-ai` and `kimetsu-remote`.
0.1.0 was the last release that versioned the MCP surface on its own.

## How publishing works (esbuild / turbo style)

Platform packages are **not committed** — binaries never live in git. They are
assembled and published entirely in CI (`.github/workflows/release.yml`, the
`stage-npm` job) from the **lean** release archives the `build` matrix already
produces:

```
@kimetsu-ai/linux-x64     os:[linux]  cpu:[x64]    x86_64-unknown-linux-gnu
@kimetsu-ai/darwin-x64    os:[darwin] cpu:[x64]    x86_64-apple-darwin
@kimetsu-ai/darwin-arm64  os:[darwin] cpu:[arm64]  aarch64-apple-darwin
@kimetsu-ai/win32-x64     os:[win32]  cpu:[x64]    x86_64-pc-windows-msvc
```

npm installs only the platform package whose `os`/`cpu` match the host; the
launcher `require.resolve`s its binary and execs it. No postinstall script — it
works under `npm install --ignore-scripts`.

The embeddings build is larger and only supported on three targets, so it is
fetched on demand rather than shipped as a package. Users opt in once with
`kimetsu npm-flavor embeddings` (a launcher-only command that fetches the binary
and records the preference in `<cache>/kimetsu/npm/flavor`, so it persists with
no env var); `KIMETSU_NPM_FLAVOR=embeddings`/`=lean` remains a per-run override.

## Versioning

`Cargo.toml` `[workspace.package] version` is the single source of truth.
`npm/kimetsu/package.json`, `npm/kimetsu-remote/package.json`,
`npm/kimetsu-sdk/package.json`, and the launchers' `optionalDependencies`
carry that same number. `scripts/check-version-sync.py` fails CI when they
drift. CI still stamps `${GITHUB_REF_NAME#v}` into the launcher packages and
every `@kimetsu-ai/*` platform package at publish time, and the release
version-guard refuses a tag that disagrees with the workspace. Platform
packages are not committed; `scripts/assemble-npm.sh` writes them from the
lean release archives.
