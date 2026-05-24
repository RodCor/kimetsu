# Changelog

All notable changes to kimetsu land here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions
follow [SemVer](https://semver.org/spec/v2.0.0.html) with the
caveat that pre-1.0 minor bumps may include breaking changes
(documented in the release notes).

## v0.4.9 — SecretString for provider tokens + automated crates.io publish

SECURITY
  Both `ClaudeCodeProvider` and `AnthropicProvider` previously held
  `api_key: String` and derived `#[derive(Debug)]`. Any `{:?}`
  print of either struct — panic backtrace, `dbg!` left in a debug
  session, `tracing::debug!(?provider)` from a future telemetry
  pass — would have written the raw OAuth token / API key to
  stderr or the log sink.

  v0.4.9 introduces `kimetsu_core::secret::SecretString` whose
  `Debug` / `Display` / `serde::Serialize` impls all emit
  `"[REDACTED; len=N]"`. Cleartext is only reachable via
  `expose_secret()` — every caller is now greppable in code
  review.

  Provider fields converted:
    ClaudeCodeProvider.api_key: String -> SecretString
    AnthropicProvider.api_key: String -> SecretString

  Cleartext leak points (greppable, intentional):
    crates/kimetsu-agent/src/claude_code.rs
      .env("CLAUDE_CODE_OAUTH_TOKEN", self.api_key.expose_secret())  (2 sites)
      redact_token(..., self.api_key.expose_secret())                (3 sites)
    crates/kimetsu-agent/src/anthropic.rs
      .header("x-api-key", self.api_key.expose_secret())             (1 site)

  Regression guards:
    kimetsu_core::secret::tests
      debug_format_never_includes_inner_value
      display_emits_redaction_marker
      serialize_emits_redaction_marker
      expose_secret_returns_cleartext
      parent_struct_derive_debug_does_not_leak
    kimetsu_agent::claude_code::tests::debug_format_does_not_leak_api_key
    kimetsu_agent::anthropic::tests::debug_format_does_not_leak_api_key

  Pre-existing v0.4.5 `kimetsu_brain::redact` already covers the
  ingest-side leak surface (token strings in memory text); this
  patch closes the in-memory-struct surface.

DISTRIBUTION
  Per-crate Cargo.toml: every `path = "../kimetsu-X"` now also
  declares `version = "0.4.9"`. Required for `cargo publish`
  to resolve cross-crate deps via the registry instead of the
  local path.

  .github/workflows/release.yml gains a `publish-crates` job
  that runs after the binary matrix + GH Release succeed. The
  job uses `${{ secrets.CARGO_REGISTRY_TOKEN }}` and publishes
  the six crates in dependency order with a 30-second sleep
  between each so the crates.io index can propagate:
    kimetsu-core -> kimetsu-brain -> kimetsu-agent
                 -> kimetsu-harbor-rs -> kimetsu-chat
                 -> kimetsu-cli

  ONE-TIME SETUP (operator action):
    gh secret set CARGO_REGISTRY_TOKEN < <(cat path/to/token)
  Or via the GitHub UI: Settings -> Secrets -> Actions.
  The job hard-errors with an actionable message if the secret
  is missing, so a misconfigured pipeline fails fast.

  After this tag ships, end-users can:
    cargo install kimetsu-cli
    cargo install kimetsu-cli --features embeddings

  v0.4.7 + v0.4.8 GH Releases exist but were never published to
  crates.io (the workflow didn't have the publish job). v0.4.9 is
  the first crates.io-published version.

NEXT
  Smoke-validate with `kimetsu doctor` against a fresh
  `cargo install kimetsu-cli` to confirm the registry flow works
  end-to-end. If anything breaks per-platform, cut v0.4.9.1.

## v0.4.8 — release-pipeline patch

The v0.4.7 release workflow failed across every platform with:

```
error: the package 'kimetsu-cli' does not contain this feature: embeddings
help: package with the missing feature: kimetsu-brain
```

Cargo doesn't auto-forward features across workspace dep chains —
the `embeddings` feature lived on `kimetsu-brain` but the release
matrix called `cargo build -p kimetsu-cli --features embeddings`,
which can't propagate down to a dep.

v0.4.8 adds a passthrough `embeddings` feature on every crate
that depends on `kimetsu-brain` — `kimetsu-cli`,
`kimetsu-chat`, `kimetsu-agent`, `kimetsu-harbor-rs`. Each one
declares:

```toml
[features]
default = []
embeddings = ["kimetsu-brain/embeddings"]
```

`kimetsu-cli` in particular fans out to all four downstreams so
`cargo install kimetsu-cli --features embeddings` builds the
whole tree on the embeddings code path.

No behavior change beyond unblocking the release pipeline. The
v0.4.7 tag stays in git history but its corresponding GitHub
Release was never published (the pipeline failed before upload).

## v0.4.7 — distribution path

- **Per-crate `Cargo.toml` metadata** filled in for crates.io
  publish: `description`, `repository`, `homepage`,
  `documentation`, `readme`, `rust-version`, `keywords`,
  `categories`. Workspace `license` flipped from `UNLICENSED`
  (which crates.io rejects) to dual `MIT OR Apache-2.0` —
  matches the Rust ecosystem norm.
- **`LICENSE-MIT` + `LICENSE-APACHE`** files added at repo root.
- **`.github/workflows/release.yml`** ships tag-driven release
  pipeline: pushes a `v0.4.x` tag and CI builds release binaries
  for Linux/macOS/Windows × {lean, embeddings} flavors, runs
  `kimetsu doctor --skip-mcp` against each, attaches the archives
  to the GitHub Release, and pulls release notes from this
  CHANGELOG.
- **`kimetsu doctor` runs as a release gate** before any artifact
  uploads, so a broken build can never become a published release.

## v0.4.6 — `kimetsu doctor` (automated wire-health)

- New `kimetsu doctor [--json] [--workspace PATH] [--skip-mcp]`
  CLI subcommand. Runs 8 hermetic checks (workspace, brain,
  safety, retrieval, mcp, plugin) and reports Pass / Warn /
  Fail / Skip per check with an actionable next-step on warns.
- Live-validated against this repo: 6 pass / 1 warn / 0 fail /
  1 skip, proving v0.4.1 (user brain), v0.4.4 (ambient), and
  v0.4.5 (redact) are all wired correctly end-to-end.
- `kimetsu-cli` test count: 2 → 6 (+4 doctor tests).

## v0.4.5 — secret redaction at ingest

- New `kimetsu_brain::redact` module: `redact_secrets(text) ->
  RedactionResult` with non-overlapping greedy coverage across
  **13 secret kinds** (anthropic_oauth, openai_api_key,
  github_pat, slack_token, aws_access_key, jwt,
  private_key_pem, google_api_key, generic_bearer,
  generic_api_key, generic_token, generic_password).
- Wired at every memory write boundary: `project::add_memory`,
  `user_brain::add_user_memory`, `propose_benchmark_memory`.
  Redaction is idempotent — double-call is safe.
- On a hit, prints a one-liner to stderr (`kimetsu-brain:
  redacted 1 secret: anthropic_oauth`). Write proceeds with the
  redacted text; the surrounding context is preserved.
- 12 unit tests + 1 end-to-end test proving `sk-ant-...` never
  reaches brain.db.

## v0.4.4 — ambient pre-turn context

- New `kimetsu_brain::ambient` module: collects git branch,
  `git status --short` top entries, top-5 mtime-ordered recent
  files (via the `ignore` crate, `.kimetsu/` filtered).
- `render_as_query_suffix(&ctx)` appends a short suffix like
  `\n[workspace: branch=X | recent: a.rs, b.rs | dirty: M ...]`
  to the explicit `query` before retrieval — so terse queries
  ("fix it", "continue") still surface useful capsules.
- Wired into `kimetsu brain context [--no-ambient]` CLI and into
  the MCP `kimetsu_brain_context` + `kimetsu_benchmark_context`
  tools (per-call `include_ambient` parameter, default true;
  global kill-switch `KIMETSU_BRAIN_AMBIENT=off`).

## v0.4.3 — fastembed-rs backend + `kimetsu brain reindex`

- `fastembed = "5"` added as an OPTIONAL dependency behind the
  `embeddings` Cargo feature. Default build stays dep-light;
  opt in with `cargo install kimetsu-cli --features embeddings`.
- Three builtin models selectable via `KIMETSU_BRAIN_EMBEDDER`:
  `bge-small-en-v1.5` (default, 384 dim), `bge-m3` (1024 dim,
  multilingual), `jina-v2-base-code` (768 dim, code-tuned).
- `open_default_embedder()` returns a cached embedder via
  process-static `OnceLock` — model loads once per process.
- New `kimetsu brain reindex [--scope project|user|all]
  [--dry-run] [--force] [--limit N]` CLI subcommand backfills
  NULL embeddings AND rows whose `embedding_model` doesn't
  match the active embedder.

## v0.4.2 — embeddings + hybrid retrieval scaffolding

- New `kimetsu_brain::embeddings` module: `Embedder` trait,
  `NoopEmbedder` (production default through v0.4.2),
  `StubEmbedder` (deterministic test pseudo-embedder), cosine +
  little-endian f32 BLOB codec helpers.
- `memories.embedding BLOB NULLABLE` + `embedding_model TEXT
  NULLABLE` schema columns. Migrated idempotently via
  `add_column_if_missing`.
- `retrieve_context_with_embedder` blends cosine with FTS as
  `final = (1 - α) * lex + α * normalized_cos` with `α = 0.5`.
  Cross-model rows skip the cosine term safely.

## v0.4.1 — user-scope brain at `~/.kimetsu/brain.db`

- New `kimetsu_brain::user_brain` module. `MemoryScope::GlobalUser`
  writes now route to `~/.kimetsu/brain.db` (or
  `$KIMETSU_USER_BRAIN_DIR/brain.db` for tests / power users).
- `BrainSession` opens both DBs and merges retrievals across them
  via the new `retrieve_context_multi` path. Repo memories stay
  per-project; user memories follow the user between repos.
- Kill-switch: `KIMETSU_USER_BRAIN=0` falls back to v0.3.5
  behavior.

## v0.3 — see [`docs/V0.3.4-SHIP.md`](docs/V0.3.4-SHIP.md) + the kimetsu-chat + bridge plugin commits

The v0.3 line introduced the chat client, the bridge plugin
mode (MCP sidecar for Claude Code and Codex), and Anthropic
prompt-cache visibility + the persistent claude subprocess
that makes cache_read actually land. See the V0.3.4 ship doc
for the deep dive on cache + persistent subprocess; the
v0.3.5 perf pass (later commit) flipped the persistent path
to default-on for chat.

## v0.2 — Terminal-Bench validation

The v0.2 line ran the MP gauntlet from MP-4 through MP-18:
broker design + retrieval scoring, the 20-tool surface,
auto-orient pre-shell, parallel `tool_calls` envelope,
record_deviation + iterative verify. See `docs/archive/MP-*-RESULTS.md`
and `docs/archive/V0.2-SHIP.md`.

## v0.1 — initial scaffold

Brain + agent + pipeline foundations. See `docs/archive/MVP.md`.
