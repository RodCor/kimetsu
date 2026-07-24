The MCP tool surface, the host hooks that make the loop reliable, and proactive mid-work recall.

## The MCP surface

Run `kimetsu mcp serve` and the host gets ~28 `kimetsu_*` tools. The ones you
will actually reach for:

| Tool | What it does |
|------|--------------|
| `kimetsu_brain_context` | Retrieve a context bundle (returns `skipped: true` when nothing relevant, at zero overhead) |
| `kimetsu_brain_record` | Capture a lesson; runs semantic dedup |
| `kimetsu_brain_status` | Brain health + memory counts |
| `kimetsu_brain_insights` | Effectiveness analytics over recent runs |
| `kimetsu_brain_memory_add` / `_list` / `_top` / `_search` | Direct memory CRUD and search |
| `kimetsu_brain_memory_proposals` / `_accept` / `_reject` | Review pending proposals |
| `kimetsu_brain_memory_invalidate` | Retire a memory |
| `kimetsu_brain_memory_blame` | Per-run citation attribution |
| `kimetsu_brain_memory_conflicts` / `kimetsu_brain_conflict_resolve` | List / settle ingest conflicts |
| `kimetsu_brain_prune` | List (or invalidate) net-negative memories |
| `kimetsu_brain_model_list` / `_set` / `kimetsu_brain_reindex` | Inspect / switch / re-embed the embedding model |
| `kimetsu_brain_ingest_repo` | Index repo files + manifests |
| `kimetsu_benchmark_context` / `_record_outcome` | Task-aware playbook + outcome recording |
| `kimetsu_bridge_*` / `kimetsu_skills_search` | Skill registry discovery, import, export, sync |
| `kimetsu_brain_cite` | Record that a memory materially helped |
| `kimetsu_brain_answer` | Grounded, cited answer composed from memory (local model) |

Every tool returns `{"ok": true, "usage": {...}}` so the host gets guidance on
how to use the output, not just raw data.

### Host hooks

The MCP tools work whether or not the model decides to call them. The plugin
installers make the loop reliable by writing host-native hook config
(`.claude/settings.json` + `.mcp.json` for Claude Code, `.codex/` for Codex,
`openclaw.json` for OpenClaw, a TypeScript extension for Pi):

- **`UserPromptSubmit` -> `kimetsu brain context-hook`** fires before each
  turn, retrieves a bundle, and injects it. On embeddings builds it asks the
  warm embedder daemon (`kimetsu brain embed-daemon`, pre-warmed on
  SessionStart) for semantic retrieval within a 300ms budget, falling back to
  lexical FTS if the daemon is unreachable, so the prompt is never blocked.
  The daemon holds the ONNX models in memory and finishes with a
  cross-encoder rerank (see [Retrieval models](retrieval-models)).
  With `--warm-on-first-prompt` the hook also prepends the warm-start block
  (digest + resume) to the session's first turn — see below.
- **`Stop` -> `kimetsu brain stop-hook`** prints a one-line post-turn banner:
  how many lessons were captured, or a nudge to record one after a
  non-trivial session.
- **`SessionEnd` -> `kimetsu brain session-end-hook`** runs the optional
  credentialed distiller (Codex uses `Stop` with `--distill-on-stop`).

These are plain CLI subcommands, so the same pattern works under any harness
that can run a command on a prompt, stop, or session-end event.

### Warm start, on every host

The warm-start block — the ~400-token repo digest plus your episodic resume —
is what makes the agent's first turn already know the repo. Every host gets it,
by whichever route that host actually has:

| Host | Route |
|------|-------|
| Claude Code | `SessionStart` -> `kimetsu brain session-start-hook` |
| Codex, Pi, OpenClaw | per-turn hook with `--warm-on-first-prompt`: prepended to the session's first prompt, once |
| Cursor | no hooks at all — the first `kimetsu_brain_context` call of a session returns a `warm_start` field alongside the capsules |

Only one route fires per host, so the block is never delivered twice. It is
gated by `[broker] warm_start` (default on) everywhere. A cached digest is
served immediately even when the corpus has moved under it, and the rebuild
runs detached, so a stale digest never puts a build in front of your first turn.

Pi and OpenClaw are driven by a generated TypeScript extension rather than a
hook file: it feeds the hook payload to the CLI on stdin and reads the injected
context back off stdout (Pi returns it from `before_agent_start`, OpenClaw as
`prependContext` from `agent_turn_prepare`). A missing, hung, or failing
`kimetsu` binary is always a silent no-op — the sidecar must never break the
host.

### Proactive recall (mid-work)

`UserPromptSubmit` only fires between turns. Two tool-level hooks surface a
memory while the agent works, the way a memory comes to you rather than you
fetching it. Both match only Bash commands and never block:

- **`PreToolUse` -> `kimetsu brain pretool-hook`**: if the command strongly
  matches a stored `failure_pattern` or `convention`, warn first.
- **`PostToolUse` -> `kimetsu brain posttool-hook`**: when a command actually
  failed, surface a matching fix. "Actually failed" is decided by the strongest
  evidence available — the exit code when the harness reports one, else a
  toolchain summary line (cargo, pytest, jest/vitest, go test, tsc, npm, make),
  else a substring scan that an explicit `0 failed` / `test result: ok` can
  veto. A passing test suite is not a failure just because its summary line
  contains the word "failed".

Discipline keeps this near-zero-cost: lexical-FTS-only retrieval, one capsule
max, per-session dedup, and a refractory window between injections. When
nothing clears the bar, the hook prints nothing. Per-session state lives
outside the repo under `~/.kimetsu/cache/` and is GC'd after 7 days.

**Whether to speak is a learned decision.** The score threshold used to be a
constant (0.45, or 0.35 when the agent was visibly looping) that applied to
every brain and never moved. It is now a small logistic policy over the score,
loop mode, capsule kind, how much has already been injected this session, how
long since the last injection, how often this command has already failed, and
how strong the evidence of failure was.

Its prior is pinned to *exactly* the old thresholds, with every other weight at
zero — so an untrained brain behaves precisely as it did before, and there is no
cold-start regression to trade against the eventual gain. Each decision is
recorded with its features, and labelled by whether the agent went on to cite
the memory it was handed. `kimetsu brain policy` prints the weights and how the
fit compares to the legacy rule on your own history; `--train` refits;
`--reset` returns to the constant. Model-free, so it runs on the Free tier.

Proactive hooks install by default; pass `--no-proactive` to skip them.

---

## Kimetsu Remote (beta)

Everything above assumes a local brain over stdio MCP. Kimetsu Remote runs
the brain on a server over HTTP MCP: the identity becomes the repository, so
any checkout on any machine (or a teammate's) hits the same brain. One brain
per repo, bearer auth with per-user tokens and attribution, an optional
shared org brain, server-side repo ingest, TLS, Prometheus metrics, and a
server-side reranker. Setup, hardening, and benchmarks:
[Kimetsu Remote](../remote).
