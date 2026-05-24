# Changelog

All notable changes to kimetsu land here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions
follow [SemVer](https://semver.org/spec/v2.0.0.html) with the
caveat that pre-1.0 minor bumps may include breaking changes
(documented in the release notes).

## v0.5.2 — conflict detection at ingest: contradictions surface, don't silently compete

Third and final beat of the v0.5 arc. v0.5.0 attributed which memories
helped; v0.5.1 made stale boosters age out; v0.5.2 stops contradictory
memories from accumulating in the first place. "Use anyhow" and "use
thiserror" no longer both live in the brain quietly competing for
retrieval slots — the second write surfaces the conflict at ingest
time so the operator can decide which to keep.

WHAT v0.5.2 ADDS
  * New module `kimetsu_brain::conflict`. Top-level surface:
    `find_potential_conflicts(conn, scope, text, embedder, top_k,
    threshold)` returns `ConflictHit` rows whose cosine >= threshold
    AND whose normalized text differs from the incoming text.
    Defaults: `DEFAULT_CONFLICT_THRESHOLD = 0.8`, `DEFAULT_TOP_K = 3`.
  * Embedder gating. `embedder.is_noop()` short-circuits to zero
    hits, so lean builds keep exact pre-v0.5.2 behavior. Cross-model
    rows (embedding_model != active embedder id) are silently
    skipped — cosine across models is meaningless. A subsequent
    `kimetsu brain reindex` rehydrates them and the next ingest
    catches the conflict.
  * New schema: `memory_conflicts` table linking
    `(new_memory_id, existing_memory_id)` with `similarity`,
    `scope`, `kind`, `detected_at`, optional `resolved_at` +
    `resolution`. UNIQUE on the pair so re-scans stay idempotent.
    Created via `CREATE TABLE IF NOT EXISTS`; pre-v0.5.2 brain.db
    files pick it up on first open.
  * Wiring: both `project::add_memory` and `user_brain::add_user_memory`
    call `conflict::detect_and_record` after the post-insert
    embedding write. On a hit, one line to stderr — never blocks
    the write (surfacing > blocking; a blocked write loses user
    intent, a logged write loses nothing).

USER SURFACE: conflicts
  * `kimetsu brain memory conflicts [--limit N] [--json]` CLI:
    lists open conflicts merged from project + user brains,
    sorted by detected_at DESC. Each row shows similarity, scope,
    kind, and a one-line preview of both texts so the contradiction
    is visible at a glance.
  * `kimetsu brain memory conflicts --resolve <id> <kept_new|
    kept_existing|kept_both>`: settles a single conflict. With
    `kept_new` the existing memory is invalidated; with
    `kept_existing` the new memory is invalidated; with
    `kept_both` neither is touched (legit case where both apply
    in different contexts). Idempotent — a second resolve on the
    same id returns false without rewriting `invalidated_at`.
  * `kimetsu_brain_memory_conflicts` MCP tool (read-only): same
    backend, JSON-shaped for Claude Code / Codex. Resolution is
    deliberately CLI-only to keep the audit trail centralized.
    Brings the kimetsu_* MCP catalog to 18 tools.

NEW BRAIN API
  * `conflict::find_potential_conflicts(...)` — pure detection.
  * `conflict::record_conflict(...)` — idempotent insert keyed on
    the memory pair.
  * `conflict::detect_and_record(...)` — convenience wrapper used
    by the ingest path, returns the count of newly-recorded hits.
  * `conflict::list_unresolved_conflicts(conn, limit)` — joined
    with both memories' text for rich display.
  * `conflict::resolve_conflict(conn, conflict_id, resolution)` —
    settles a row, invalidates the losing side, returns true if
    something changed.
  * `project::list_conflicts(start, limit) -> Vec<ScopedConflict>`
    merges project + user brain rows with a `source` label.
  * `project::resolve_conflict(start, id, resolution)` — project
    DB first, user brain fallback. Acquires the project lock.

TESTS (12 new in brain — 10 conflict module + 1 project integration + 1 wrapper)
  conflict::tests (10)
    noop_embedder_returns_no_conflicts
    cross_model_rows_are_skipped
    exact_match_is_not_flagged_as_conflict
    similar_but_different_text_is_flagged
    record_conflict_is_idempotent
    list_unresolved_excludes_resolved_rows
    resolve_conflict_invalidates_loser_side
    resolve_conflict_is_idempotent
    detect_and_record_noop_writes_nothing
    resolve_conflict_rejects_invalid_resolution_strings
  project::tests (1)
    add_memory_under_noop_embedder_writes_no_conflicts
      End-to-end regression: NoopEmbedder path produces zero
      conflicts, exercises list_conflicts + resolve_conflict
      wrappers (unknown id returns false, invalid resolution
      string errors out).

VERIFIED
  cargo test --workspace      239 / 239 passing
    (was 227 at v0.5.0, 239 now: +12 conflict tests)
  cargo build --workspace     clean at 0.5.2

UPGRADE NOTES
  * Existing brain.db files: `memory_conflicts` table created
    idempotently on first open with the v0.5.2 binary. No
    backfill — conflicts are only detected at fresh ingest.
  * Lean (default) builds: conflict detection is a silent no-op.
    Build with `--features embeddings` to enable. (Same gate as
    semantic retrieval.)
  * Threshold tuning: 0.8 cosine is BGE-small-en-v1.5's empirical
    "same concept" floor. If you see false positives in
    `kimetsu brain memory conflicts`, the surfaced pairs are
    similar-but-correct (e.g. two legit preferences for different
    contexts) — resolve as `kept_both` to silence them. If you see
    false negatives (a real contradiction sneaking through),
    raise the threshold via a future config knob (deferred until
    real data justifies the surface).
  * The MCP tool is read-only by design. Operators resolve from
    the CLI; the host harness can list and reason about open
    conflicts but cannot apply resolutions. This keeps the audit
    trail centralized and prevents an agent from silently
    "resolving" a real contradiction it should have surfaced.

THE v0.5 ARC IS COMPLETE
  v0.5.0 — citations: the brain knows *which* memories helped.
  v0.5.1 — decay:     stale "useful" boosters age toward neutral.
  v0.5.2 — conflicts: contradictory writes surface, don't compete.
  Together: the brain learns from outcomes, ages out stale signal,
  and stops accumulating noise. Pitch sharpens from "memory that
  follows you" to "memory that follows you AND improves on its own."

## v0.5.1 — usefulness decay: recency-weighted memory ranking

Second beat of the v0.5 arc. v0.5.0 gave us *which* memories
helped; v0.5.1 makes "helped" age out. A memory that proved
useful 6 months ago shouldn't outrank one that proved useful
yesterday — yet under the v0.5.0 multiplier they tied, because
the boost was permanent. Long-running repos accumulated stale
boosters that crowded out fresh signal.

WHAT v0.5.1 ADDS
  * New column `memories.last_useful_at TEXT NULL`. Bumped by
    the projector ONLY on `(memory cited) AND (run.finished)` —
    cited + run.failed doesn't count (the memory misled the
    model), silent passengers never bump regardless of outcome.
    Distinct from `last_used_at` which still bumps on every
    retrieval. NULL on pre-v0.5.1 rows and on rows never
    cited successfully; the broker falls back to `created_at`
    for those.
  * New broker config `[broker.weights] decay_half_life_days`,
    default 30.0. `#[serde(default)]` so pre-v0.5.1 project.toml
    files keep loading cleanly. Set to 0 to disable decay.
  * New helper `kimetsu_brain::context::usefulness_decay(
    last_useful_at, created_at, half_life_days) -> f32` returning
    `exp(-ln(2) * age_days / half_life)` clamped to `[0, 1]`.
    Fail-open: unparseable timestamps and non-positive half-lives
    return 1.0 so retrieval never silently drops rows.

THE DECAY SHAPE
  decay attenuates the *deviation* from neutral, not the
  multiplier itself:
    effective = 1.0 + (raw_multiplier - 1.0) * decay
  At decay=1.0 a memory with the max +1.5 boost stays at +1.5.
  At decay=0.0 (very old) it slides back to 1.0 (neutral) —
  same as a brand-new memory with zero history. Critically NOT
  zero: losing confidence in old signal shouldn't penalize a
  memory *below* a fresh one. Symmetric for the penalty side
  too: old penalties also fade toward neutral.

CALL CHAIN PLUMBING
  * `retrieve_context_with_embedder` reads
    `weights.decay_half_life_days` and threads it through
    `memory_candidates` → `{latest, fts}_memory_candidates` →
    `memory_row_to_candidate`.
  * Both retrieval SQL queries now also SELECT `last_useful_at`.

TESTS (7 new in brain, all in context.rs)
  context::tests::
    usefulness_decay_disabled_when_half_life_is_zero_or_negative
      Operator opt-out hatch: half_life <= 0 returns 1.0.
    usefulness_decay_returns_one_on_unparseable_timestamps
      Fail-open guard for corrupted rows.
    usefulness_decay_full_at_zero_age
      Future-timestamp (negative age clamped to 0) returns 1.0.
    usefulness_decay_follows_half_life_curve
      Asserts decay ≈ 0.5 at one half-life, ≈ 0.25 at two
      half-lives, computed against a real OffsetDateTime::now_utc.
    usefulness_decay_falls_back_to_created_at_when_last_useful_is_none
      Brand-new never-cited memories decay from their birthday,
      not from a hard 1.0 floor.
    aged_cited_memory_ranks_below_recently_cited_memory
      End-to-end: two FTS-tied memories, one cited yesterday and
      one cited a year ago — recent must rank first under the
      default 30-day half-life.
    aged_cited_memory_does_not_decay_when_half_life_is_zero
      Companion regression: with decay off, the same two
      memories tie on score. Proves the v0.5.1 flip is caused
      by decay, not by some unrelated timestamp side effect.

VERIFIED
  cargo test -p kimetsu-brain    86 / 86 passing  (was 79)
  cargo build --workspace        clean at 0.5.1

UPGRADE NOTES
  * Existing brain.db files: `last_useful_at` column added
    idempotently on first open with the v0.5.1 binary. All
    pre-v0.5.1 memories start at NULL → they decay from their
    `created_at` until the next successful citation refreshes
    them. No data loss; ranking will shift toward recently
    confirmed memories.
  * Existing project.toml files: no edit required.
    `decay_half_life_days = 30.0` applies automatically. To
    opt out, add `decay_half_life_days = 0.0` under
    `[broker.weights]`.
  * Tune the half-life per repo: lower (e.g. 14) for fast-
    moving codebases where knowledge ages quickly; higher
    (e.g. 90) for slow-evolving ones where old playbooks
    still apply.

NEXT (in flight)
  * v0.5.2 — conflict detection at ingest. (Shipped above.)

## v0.5.0 — the brain learns from outcomes: citations + blame

v0.5's north star: make the brain *get smarter over time* from
real run data. v0.5.0 ships the foundation — per-memory
attribution — that v0.5.1 (decay) and v0.5.2 (conflict detection)
build on. See `docs/V0.5-PLAN.md` for the full arc.

PROBLEM
  Until v0.4.x the brain's usefulness signal was per-run, all-or-
  nothing: every memory in a run's `context.injected` event got
  +1 on `run.finished` or -1 on `run.failed`. A run that
  succeeded thanks to 1 of 10 retrieved memories rewarded all
  10 equally. Noise compounded over time — retrieved-and-ignored
  memories accumulated the same usefulness score as
  retrieved-and-pivotal ones.

WHAT v0.5.0 ADDS
  * New tool `cite_memory(memory_id, rationale?)`. The model
    calls it during a turn when it consciously leveraged a
    retrieved capsule. Best-effort metadata — forgetting to cite
    doesn't fail the turn. Multiple citations per turn are fine.
  * New `memory.cited` event kind. The agent loop accumulates
    `cite_memory` calls into `recorded_citations` (annotated with
    the turn index), and the transport surface (chat REPL, harbor
    binary) emits one `memory.cited` event per citation to the
    trace at run wrap-up.
  * New schema: `memory_citations` table linking
    `(run_id, memory_id, turn)` with `cited_at` + optional
    `rationale`. Idempotent migration via `CREATE TABLE IF NOT
    EXISTS`; pre-v0.5.0 brain.db files pick up the table on first
    open with the new binary.
  * Projector handler `apply_memory_cited` mirrors each event
    into the new table.
  * Usefulness scoring split — cited memories get the strong
    ±1.0 delta, silent passengers (retrieved-but-not-cited)
    get the weak ±0.1 delta. Encourages models to actually use
    `cite_memory` and keeps the strong signal aimed at memories
    that actually contributed.

USER SURFACE: blame
  * `kimetsu brain memory blame <run-id> [--json]` CLI:
    prints cited memories with rationale + turn, then silent
    passengers with their text previews. JSON output for
    hooks/CI.
  * `kimetsu_brain_memory_blame` MCP tool: same backend
    (`project::blame_run`), JSON-shaped for Claude Code / Codex
    to consume. Listed in the 16+1 = 17 kimetsu_* tools advertised
    by `tools/list`.

NEW BRAIN API
  * `project::blame_run(start, run_id) -> BlameReport` walks
    `memory_citations` + `context.injected` events + the terminal
    run event, looks up each memory's text from project + user
    brains, returns `BlameReport { run_id, outcome,
    failure_category, cited, silent_passengers }`.

TESTS (3 new in brain, 1 net new since v0.4.11)
  brain::project::tests::
    run_finished_increments_usefulness_for_injected_memories
      Updated: now emits `memory.cited` so the test demonstrates
      the strong +1.0 signal path.
    run_failed_decrements_usefulness_unless_gate
      Updated: same — adds a citation so the failure penalty
      hits at strong -1.0.
    run_finished_gives_weak_signal_to_silent_passenger_memories  (NEW)
      Asserts: retrieved + uncited memory ends up at +0.1, not
      +1.0, on run.finished.
    blame_run_separates_cited_from_silent_passengers  (NEW)
      End-to-end: writes a run with 2 retrieved memories
      (1 cited, 1 silent), calls blame_run, asserts the cited
      one appears under `cited` with rationale + turn, the
      silent one under `silent_passengers`.

VERIFIED
  cargo test --workspace        227 / 227 passing
  cargo metadata --no-deps      clean at 0.5.0

UPGRADE NOTES
  * Pre-v0.5.0 chat / harbor runs continue to work — they just
    won't emit `memory.cited` events, so all their retrieved
    memories will be treated as silent passengers (±0.1 each).
    If you want the old "everything in context gets ±1" behavior
    back, the rule lives in
    `kimetsu_brain::projector::apply_memory_usefulness_for_run`.
  * Existing brain.db files don't need migration beyond opening
    them with the v0.5.0 binary — the `memory_citations` table
    is created on first open.
  * `kimetsu brain memory blame` on a pre-v0.5.0 run will
    typically show 0 cited + all retrieved as silent passengers
    (since no `memory.cited` events fired).

NEXT (shipped)
  * v0.5.1 — usefulness decay. (Shipped above.)
  * v0.5.2 — conflict detection at ingest. (Shipped above.)

## v0.4.11 — drop x86_64-apple-darwin from the release matrix

The v0.4.10 release pipeline got stuck because two GitHub Actions
matrix jobs queued indefinitely:

```
build x86_64-apple-darwin (lean)        — queued, never started
build x86_64-apple-darwin (embeddings)  — queued, never started
```

As of late 2026, `macos-13` (Intel) runners are deprecated on the
GitHub Actions free tier and queue indefinitely without an SLA.
Apple Silicon (`macos-14` and newer, arm64) is the dominant
architecture and runs fine. Sitting in the queue for hours
blocked the `release` job → blocked the `publish-crates` job →
nothing actually shipped.

Fix in v0.4.11:

* `.github/workflows/release.yml` matrix drops the two
  `x86_64-apple-darwin` entries. The release matrix now ships
  6 archives (down from 8):
    * x86_64-unknown-linux-gnu   (lean + embeddings)
    * aarch64-apple-darwin       (lean + embeddings)  ← Apple Silicon
    * x86_64-pc-windows-msvc     (lean + embeddings)
* Users on Intel Macs can still `cargo install kimetsu-cli`
  (with or without `--features embeddings`) — the source build
  is target-portable. They just don't get a pre-built binary.
* If GitHub re-provisions `macos-13` capacity in the future, we
  add it back; if x86_64 mac demand spikes, we can also cross-
  compile from `macos-14` (arm64 host) — a v0.5 follow-up.

No code changes. v0.4.9's SecretString + v0.4.10's harbor-rs
publish exclusion both carry forward.

OPERATOR ACTION
  Cancel the stuck v0.4.10 workflow run on GitHub Actions
  (it'll never complete with those queued macOS jobs):
    gh run cancel <run-id>
    # or click "Cancel workflow" in the Actions tab UI

  Then v0.4.11's tag push fires a fresh, clean run.

## v0.4.10 — kimetsu-harbor-rs stays out of crates.io

The v0.4.9 publish pipeline included `kimetsu-harbor-rs` in the
registry rollout. Reviewing pre-flight, that was wrong:

  * `kimetsu-cli` (the binary `cargo install kimetsu-cli` produces)
    does not depend on `kimetsu-harbor-rs`. End users never reach
    it through the registry path.
  * Harbor is a Terminal-Bench operator tool, still iterating
    internally. Publishing implies API stability we don't want
    to commit to yet.
  * The `kimetsu-harbor-agent` binary that benchmark operators
    actually use ships in every GH Release archive built by the
    matrix job (lean flavor). That stays.

Fixes in v0.4.10:

  * `crates/kimetsu-harbor-rs/Cargo.toml` adds `publish = false`.
    A manual `cargo publish -p kimetsu-harbor-rs` now refuses
    outright with a clear error.
  * `.github/workflows/release.yml` drops the
    `publish kimetsu-harbor-rs` step. The publish-crates job now
    walks 5 crates (core → brain → agent → chat → cli), not 6.
  * Summary block updated: "Published 5 crates" + an explicit
    note that harbor-rs is intentionally not published.

No code changes. No new tests. v0.4.9's SecretString + automated
publish work all carries forward.

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
