## The MCP surface

Run `kimetsu mcp serve` and the host harness gets ~28
`kimetsu_*` tools. The ones you'll actually reach for:

| Tool | What it does |
|------|--------------|
| `kimetsu_brain_context` | Retrieve a context bundle for a query/stage (returns `skipped: true` when nothing relevant, at zero overhead) |
| `kimetsu_brain_record` | Capture a lesson after a non-obvious solve; runs semantic dedup (§6) |
| `kimetsu_brain_status` | Brain health + memory counts at a glance |
| `kimetsu_brain_insights` | Effectiveness analytics over a recent-runs window (hit-rate, citation rate, acceptance, token economy; §6a) |
| `kimetsu_brain_memory_add` | Persist a new memory directly |
| `kimetsu_brain_memory_list` | List memories in scope, sorted by relevance |
| `kimetsu_brain_memory_top` | Top memories by usefulness ratio |
| `kimetsu_brain_memory_proposals` | Pending proposals awaiting review (paginated: `limit`/`offset`) |
| `kimetsu_brain_memory_accept` / `_reject` | Promote / reject a proposal |
| `kimetsu_brain_memory_invalidate` | Retire a memory |
| `kimetsu_brain_memory_search` | Full-text search over memory text (paginated; filter by kind/scope) |
| `kimetsu_brain_memory_blame` | Per-run citation attribution |
| `kimetsu_brain_memory_conflicts` / `kimetsu_brain_conflict_resolve` | List / settle open ingest conflicts |
| `kimetsu_brain_prune` | List (or, with `apply`, invalidate) net-negative memories |
| `kimetsu_brain_model_list` / `kimetsu_brain_model_set` | Inspect / switch the embedding model (set re-embeds the corpus) |
| `kimetsu_brain_reindex` | Backfill stale/missing embeddings |
| `kimetsu_brain_config_show` | Read the parsed `project.toml` |
| `kimetsu_brain_ingest_repo` | Index repo files + manifests |
| `kimetsu_benchmark_context` | Retrieve a task-aware playbook (biases toward `semantic_operator` + `anti_pattern` roles) |
| `kimetsu_benchmark_record_outcome` | Record run outcome → proposal |
| `kimetsu_bridge_status` / `_export` / `_import` / `_sync` | Cross-harness skill registry + install/sync |
| `kimetsu_skills_search` / `kimetsu_skill` | Find / invoke a portable skill |
| `kimetsu_brain_cite` | (write-gated) Record that a retrieved memory materially helped, closing the ground-truth loop for self-tuning |
| `cite_memory` | (in-run) Mark a memory as cited |
| `expand_capsule` | (in-run) Expand a lazily-injected capsule headline to full detail by resolving its `memory:` / `file:` handle (§3a) |

Tool input schemas + descriptions are advertised via the standard
MCP `tools/list`. Every kimetsu_* tool returns `{"ok": true, "usage":
{...}, ...}` so the host agent gets actionable "how to use this
output" guidance, not just raw data.

### Host hooks

The MCP tools work whether or not the model decides to call them. To
make the loop reliable, Kimetsu's plugin installers write host-native
hook config:

- **Claude Code**: `.claude/settings.json` (hooks) + `.mcp.json` (MCP server)
- **Codex**: `.codex/hooks.json` + `.codex/config.toml`
- **OpenClaw**: `openclaw.json` (MCP server + a hooks plugin) + a `kimetsu-context` skill
- **Pi** (no MCP): a TypeScript extension under `~/.pi/agent/extensions/` that
  shells to `kimetsu brain *-hook`, plus a `kimetsu-brain` skill

The core hook pattern is the same across MCP hosts:

- **`UserPromptSubmit` → `kimetsu brain context-hook`** fires before
  each turn. It reads the prompt from stdin, retrieves a context
  bundle, and injects it, so the model sees relevant memories without
  having to remember to ask. Zero-overhead: when the brain has nothing,
  the hook emits nothing. On `embeddings` builds the hook first asks a
  **warm embedder daemon** (`kimetsu brain embed-daemon`, one per user,
  started/pre-warmed by `kimetsu brain warm` on `SessionStart`) for
  *semantic* retrieval over a local socket; if the daemon is unreachable
  within a tight budget (300ms) it falls back to lexical FTS for that turn
  and spawns the daemon for next time, so the prompt is never blocked. The
  daemon holds the ONNX model in memory once (no per-prompt cold load) and
  serves hybrid semantic retrieval with an absolute cosine floor, finished
  by a cross-encoder rerank of a 6-capsule pool (`ms-marco-tinybert-l-2-v2`
  by default, paired with the `jina-v2-base-code` embedder, both chosen by
  benchmark; see "Retrieval models & benchmarking" below). Toggles:
  `[embedder] daemon` / `warm_on_start` / `reranker`, or
  `KIMETSU_EMBED_DAEMON=0`.
- **`Stop` → `kimetsu brain stop-hook`** fires when the host supports a
  stop event. It walks the transcript, counts `kimetsu_brain_record`
  calls, and prints a one-line post-turn banner, either confirming how
  many lessons were captured or nudging the model to record one after a
  non-trivial, un-captured session.
- **`SessionEnd` → `kimetsu brain session-end-hook`** runs the optional
  credentialed distiller when the host exposes SessionEnd. Claude Code uses
  this event; Codex uses its supported `Stop` event with `--distill-on-stop`
  for the same deterministic harvest path.

These are plain CLI subcommands, so the same pattern works under any
harness that can run a command on a prompt, stop, session-end, or tool
event. The Codex installer wires prompt-time context, stop summaries,
Stop-time distilling, proactive tool hooks, and a
`kimetsu-memory-harvester` custom agent; the Claude Code installer wires
the same flow through `.claude/settings.json` and its subagent file.

### Proactive recall (mid-work)

`UserPromptSubmit` only fires between turns. v0.8 adds two **tool-level**
hooks so the brain can surface a memory *while* the agent works, the
way a memory "comes to you" rather than you going to fetch it. Both use
a `matcher: "Bash"` so they only fire on shell commands (most tool calls
spawn nothing), and both emit `hookSpecificOutput.additionalContext`
without ever blocking:

- **`PreToolUse` → `kimetsu brain pretool-hook`** runs *before* a Bash
  command; if the command strongly matches a stored `failure_pattern` /
  `convention`, it warns first, heading off a known mistake.
- **`PostToolUse` → `kimetsu brain posttool-hook`** runs *after* a Bash
  command; when the output looks like a failure, it surfaces a matching
  `failure_pattern` / `command` fix.

Discipline keeps this near-zero-cost and non-spammy: retrieval is
**lexical-FTS-only** (no embedding-model load, so the per-call latency
stays low), gated by a **high score floor** (0.45; 0.35 once a
**repeated** failing command is detected), capped at **one capsule**,
**deduped per session** (a memory surfaces at most once), and
**throttled** by a refractory window between injections. When nothing
clears the bar the hook prints nothing, at zero tokens. Per-session state
(surfaced ids, last injection, recent commands) lives OUT of the repo, under
`~/.kimetsu/cache/<project-hash>/proactive/<session_id>.json`, and is GC'd
after 7 days, keeping `.kimetsu/` itself lean (just `brain.db` + `project.toml`).

Proactive hooks install by default with `kimetsu plugin install`; pass
`--no-proactive` (or `proactive:false` to `kimetsu_plugin_install`) to
skip `PreToolUse` / `PostToolUse` and keep only prompt-time context plus any
supported stop hook.

---

## Kimetsu Remote (beta)

Everything above assumes a **local** brain, one `.kimetsu/brain.db` next to your
checkout, reached over stdio MCP. Kimetsu Remote runs the brain on a **server**
and exposes it over **HTTP MCP**, so the identity is the **repository**, not a
local directory: any checkout of the same repo (on any machine, or a teammate's)
hits the same brain, with no local files required.

> **Beta**, under active testing; the `kimetsu-remote` server is a **separate
> package** (`npm i -g kimetsu-remote` or `cargo install kimetsu-remote
> --features embeddings`), not installed with the `kimetsu` CLI.

**The server.** `kimetsu-remote serve --data <dir> --token <secret>` hosts one
brain per repo under `<dir>/<repo-id>/`, keyed by a sanitized id the client sends
in the URL (`POST /mcp/<repo-id>`). It reuses the same transport-agnostic tool
dispatch as the stdio server, filtered to the **pure-DB, agent-facing subset**
(context, record, search, insights, curation): the tools that need no checkout.
Per-repo SQLite + WAL gives concurrent reads; writes serialize through each
repo's lock; cross-repo is fully parallel.

**Auth + hardening.** Bearer tokens (global or per-repo, constant-time compared);
optional per-token rate limiting (`--rate-limit <req/min>` → `429`); a structured
per-request log and an aggregate Prometheus `GET /metrics` (no repo labels, since it's
unauthenticated); plain HTTP by default (terminate TLS at a reverse proxy) or
in-process HTTPS with `--features tls` + `--tls-cert`/`--tls-key`.

**Client wiring.** `kimetsu plugin install <claude-code|openclaw> --remote <url>`
writes a remote MCP entry (`url` + `Authorization` header) instead of the local
stdio command, deriving the repo id from your git remote and referencing
`${KIMETSU_REMOTE_TOKEN}` so no secret hits disk.

**Optional extras.**
- **Shared org brain** (`--org-brain <dir>`): `global_user`-scoped memories are
  stored in one shared brain and merged into *every* repo's retrieval
  (cross-project team memory); `project`-scoped memories stay per-repo.
- **Server-side ingest** (`--repos-file` + `--checkout-dir`): the operator
  pre-registers repo-id → git URL; the server clones/refreshes a managed checkout
  and `kimetsu_brain_ingest_repo` indexes its files into the brain, so `context`
  retrieval includes **file capsules** remotely too. Clients can't trigger
  arbitrary clones; private repos use the server's own git auth.

### Retrieval models on the server

The remote server runs a **cross-encoder reranker** stage on every
`kimetsu_brain_context` call, the same stage the local daemon uses, but
operator-configured rather than per-repo.

**`--reranker <model>`** (default `jina-reranker-v1-tiny-en`, operator-level):
over-fetches a candidate pool of 6 capsules, runs the cross-encoder, drops noise
capsules below sigmoid score 0.30, and truncates to the caller's `max_capsules`.
`"off"` disables reranking. Any curated id or HuggingFace ONNX path is accepted
(same model registry as the local daemon).  The default was chosen by the 100-memory
benchmark: jina-tiny MRR 0.931 vs 0.914 for TinyBERT on the local bench; remote
has no hook-latency budget so the lightest reranker wins.

The **embedder** comes from per-repo config or `KIMETSU_BRAIN_EMBEDDER` (set before
seeding; reindex required after changes). The reranker is an operator flag and
cannot be overridden by a cloned repo's `project.toml` (untrusted on a server).

**Remote benchmark results** (100-case dataset, WITH jina-tiny reranker, production
floors active):

| embedder          | recall@2 | recall@4 |  MRR  | seq mean | rps  | peak RSS |
|-------------------|----------|----------|-------|----------|------|----------|
| jina-v2-base-code | 0.924    | 0.939    | 0.906 | 416ms    |  5.0 | 1198 MB  |
| bge-small-en-v1.5 | 0.929    | 0.939    | 0.909 | 700ms    |  3.8 |  697 MB  |

vs. pre-rerank baselines: jina-v2 was MRR 0.904, bge-small was MRR 0.901.

```bash
# Re-judge as your brain grows (one embedder per invocation):
kimetsu brain bench --remote --embedders jina-v2-base-code --dataset bench/dataset-100.json --out bench/results-100
kimetsu brain bench --remote --embedders bge-small-en-v1.5 --dataset bench/dataset-100.json --out bench/results-100
```

> **One embedder per invocation**: multi-embedder `--remote` runs seed later combos with the
> first embedder's vectors (process-global singleton). Kill stray `kimetsu-remote` processes
> between runs.

---


## Retrieval models & benchmarking (local)

The local retrieval stack is **embedder + cross-encoder reranker**, both
running warm inside the embed daemon. Defaults were chosen with
`kimetsu brain bench`, a benchmark seeded from REAL exported memories
(`bench/dataset-100.json`: 100 memories in confusable topic clusters, 210
cases: keyword, paraphrase, oblique, confusable, in-domain no-answer, open
multi-answer) that records expected-vs-obtained per case, latency, and RAM
per embedder × reranker combo (floors off, for raw ranking quality):

| embedder          | reranker     | recall@2 | recall@4 |  MRR  | mean ms | peak RSS |
|-------------------|--------------|----------|----------|-------|---------|----------|
| jina-v2-base-code | jina-turbo   | 0.954    | 0.975    | 0.933 | 552     | 2.0 GB   |
| jina-v2-base-code | jina-tiny    | 0.949    | 0.975    | 0.931 | 414     | 2.0 GB   |
| jina-v2-base-code | minilm-l-4   | 0.949    | 0.959    | 0.927 | 372     | 2.3 GB   |
| **jina-v2-base-code** | **tinybert-l-2** | 0.914 | 0.949 | 0.914 | **132** | 1.5 GB |
| jina-v2-base-code | off          | 0.929    | 0.939    | 0.915 | 106     | 1.5 GB   |
| bge-small-en-v1.5 | off          | 0.931    | 0.966    | 0.911 | 446     | 359 MB   |

*(Re-confirmed on v2.0 with `kimetsu brain bench --dataset bench/dataset-100.json`:
the jina-v2 quality numbers are unchanged, since retrieval ranking is deterministic on
a fixed corpus. Latencies are machine-dependent.)*

The default (**jina-v2-base-code + ms-marco-tinybert-l-2-v2**) is the
fastest reranked combo, within ~2% MRR of the grid best, and its rerank
stage fits the hook's 300ms budget. The jina-v2 embedder beats bge-small
across every reranker on this corpus (it recovers oblique, dev-phrased
queries bge never pools). Any reranker reliably beats none; the top three
rerankers are within noise of each other. Trade-off: ~1.5 GB resident
daemon (the lean-RAM option is `bge-small-en-v1.5`, ~360-525 MB, at
~1-3% lower MRR).

**Swapping models** (all local, takes effect after a daemon restart):

```bash
kimetsu config set embedder.model bge-small-en-v1.5      # or bge-m3, jina-v2-base-code
kimetsu config set embedder.reranker jina-reranker-v1-tiny-en   # or off, minilm, any HF org/repo ONNX
kimetsu brain reindex          # REQUIRED after an embedder change (vector dims differ)
kimetsu brain daemon stop      # next prompt/warm spawns a daemon with the new models
```

`KIMETSU_BRAIN_EMBEDDER` overrides the config per-process. Non-curated
rerankers load as user-defined ONNX from any HuggingFace repo.

**Re-judging as your brain grows**: the benchmark's value compounds:

```bash
kimetsu brain export bench/memories-export.json   # refresh the dataset source
kimetsu brain bench                               # full grid -> bench/results/summary.md
kimetsu brain eval                                # fixture-based quick check (recall@k, MRR)
```

Watch-item: the semantic floor (`broker.min_semantic_score`, 0.35) was
calibrated on bge-family cosine distributions; if you see over- or
under-filtering after an embedder change, re-tune it against
`kimetsu brain eval`.
