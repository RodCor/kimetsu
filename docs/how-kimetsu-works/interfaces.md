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
| `kimetsu_bridge_*` / `kimetsu_skills_search` / `kimetsu_skill` | Skill registry, install, invoke |
| `kimetsu_brain_cite` / `cite_memory` | Record that a memory materially helped |
| `expand_capsule` | Expand a lazily injected capsule headline to full detail |

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
- **`Stop` -> `kimetsu brain stop-hook`** prints a one-line post-turn banner:
  how many lessons were captured, or a nudge to record one after a
  non-trivial session.
- **`SessionEnd` -> `kimetsu brain session-end-hook`** runs the optional
  credentialed distiller (Codex uses `Stop` with `--distill-on-stop`).

These are plain CLI subcommands, so the same pattern works under any harness
that can run a command on a prompt, stop, or session-end event.

### Proactive recall (mid-work)

`UserPromptSubmit` only fires between turns. Two tool-level hooks surface a
memory while the agent works, the way a memory comes to you rather than you
fetching it. Both match only Bash commands and never block:

- **`PreToolUse` -> `kimetsu brain pretool-hook`**: if the command strongly
  matches a stored `failure_pattern` or `convention`, warn first.
- **`PostToolUse` -> `kimetsu brain posttool-hook`**: when the output looks
  like a failure, surface a matching fix.

Discipline keeps this near-zero-cost: lexical-FTS-only retrieval, a high
score floor (0.45; 0.35 for a repeated failing command), one capsule max,
per-session dedup, and a refractory window between injections. When nothing
clears the bar, the hook prints nothing. Per-session state lives outside the
repo under `~/.kimetsu/cache/` and is GC'd after 7 days.

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
