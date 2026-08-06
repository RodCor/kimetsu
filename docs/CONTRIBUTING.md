# Contributing to kimetsu

Thanks for helping improve kimetsu. This repo keeps a clean, secure
`main` and `develop` through automated checks at two points: locally on
every commit, and in CI on every pull request.

## Branches

- **`main`**: released, stable. Protected; changes land via PR.
- **`develop`**: integration branch for in-flight work. Branch your
  feature work off `develop` and open PRs back into it.
- Release tags (`vX.Y.Z`) are cut from `main` and drive the publish
  pipeline (`.github/workflows/release.yml`).

```
feature/your-thing  ->  develop  ->  main  ->  tag vX.Y.Z
```

## One-time setup

Enable the pre-commit hook after cloning:

```bash
./scripts/install-hooks.sh        # sets core.hooksPath -> .githooks
```

The hook enforces `cargo fmt --all --check` (blocking) and runs
`cargo clippy --workspace` in advisory mode (prints findings, does not
block) on staged Rust changes. Escape hatches when you need them:

- `SKIP_CLIPPY=1 git commit …`: formatting only (faster).
- `git commit --no-verify …`: skip the hook (discouraged; CI will still
  catch issues).

## The checks

Both the local hook and CI enforce the same quality bar so a green
local commit means a green PR.

| Check | Local (pre-commit) | CI (PR + push to main/develop) | Blocking? |
|-------|:------------------:|:------------------------------:|:---------:|
| `cargo fmt --all --check` | ✅ | ✅ | yes |
| `cargo clippy --workspace` | ✅ (advisory) | ✅ | advisory* |
| `cargo test --workspace` | n/a | ✅ (ubuntu + macOS) | yes |
| `cargo-audit` (RUSTSEC advisories) | n/a | ✅ | yes |
| `cargo-deny` (licenses + bans) | n/a | ✅ | advisory* |

\* clippy and cargo-deny run on every PR but do not block merges yet
(`continue-on-error` in `ci.yml`). main carries a small pre-existing
clippy backlog; once it's cleared in a dedicated cleanup PR, flip these to
hard gates (clippy: add `-D warnings`; deny: remove `continue-on-error`).

Run the full suite locally before opening a PR:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets    # advisory; aim to keep it quiet
cargo test --workspace
cargo deny check            # optional: cargo install cargo-deny
```

The semantic flavor is a separate build and CI gates on it, so run it too
before opening a PR — a `#[cfg(feature = "embeddings")]` block cannot be
compiled by any lean build, and defects there survive a fully green lean run:

```bash
cargo clippy --workspace --all-targets --features embeddings -- -D warnings
cargo test --workspace --features embeddings -j 4 -- --test-threads=1
```

`-j 4` is not decoration. On a 16-core / 32 GB machine the default job count
exhausts the page file linking the test binaries, and the failure arrives as
`os error 1455` ("the paging file is too small") or as
`STATUS_STACK_BUFFER_OVERRUN` inside `rustc` — which reads like a compiler bug
and is not one.

## Pull requests

- Target `develop` for features/fixes; target `main` only for releases
  or hotfixes.
- Keep PRs focused; describe what changed and why.
- All required CI checks must pass before merge.
- Don't commit secrets. The brain redacts known token shapes, but treat
  `.env`, credentials, and API keys as never-commit.

## Security

- Dependency vulnerabilities are caught by the `cargo-audit` CI job
  (RUSTSEC database). If it flags an advisory, bump or replace the
  affected crate before merging.
- Report security issues privately rather than in a public issue.
