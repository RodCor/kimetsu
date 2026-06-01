# npm distribution

Kimetsu is also published to npm as the `kimetsu-ai` package (the `kimetsu`
package name was taken; the project's npm scope is `@kimetsu-ai`), so JS/TS
users can `npm install -g kimetsu-ai` without a Rust toolchain. npm ships the
**same prebuilt native binary** as the GitHub Release — it is not a
reimplementation.

## Layout

```
npm/
  kimetsu/          main package — committed source (launcher, no binaries)
    bin/cli.js      resolves the platform package and execs its binary
    lib/embeddings.js  on-demand embeddings download (KIMETSU_NPM_FLAVOR=embeddings)
    package.json    optionalDependencies -> the 4 @kimetsu-ai/* platform packages
    README.md
  README.md         this file
```

## How publishing works (esbuild / turbo style)

Platform packages are **not committed** — binaries never live in git. They are
assembled and published entirely in CI (`.github/workflows/release.yml`, the
`publish-npm` job) from the **lean** release archives the `build` matrix already
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
fetched on demand by the launcher when `KIMETSU_NPM_FLAVOR=embeddings` is set,
rather than shipped as a package.

## Versioning

The committed `kimetsu/package.json` carries a `0.0.0` sentinel. CI stamps the
real version (`${GITHUB_REF_NAME#v}`) into the main package and every
`@kimetsu-ai/*` package + `optionalDependencies` entry at publish time, so npm
always matches the crates.io / GitHub Release for the same tag. The single
source of truth remains `Cargo.toml` `[workspace.package] version`.
