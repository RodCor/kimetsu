# Kimetsu Remote (beta)

Run the brain on a server and connect over **HTTP MCP**, so a team (or you
across machines) shares one brain per repository, with no local checkout.

> **Beta.** Kimetsu Remote is under active testing and may have rough edges or
> breaking changes before the stable release. The `kimetsu-remote` **server is a
> separate package**: `cargo install kimetsu-cli` / `npm i -g kimetsu-ai` do
> **not** install it. Install it on the server when you want it:
>
> ```bash
> npm install -g kimetsu-remote                       # prebuilt server binary
> cargo install kimetsu-remote --features embeddings  # or from source
> ```
>
> (or grab the standalone `kimetsu-remote` archive from a GitHub Release). The
> `kimetsu plugin install --remote` *client* wiring is part of the normal
> `kimetsu` binary, so no separate install is needed to point a host at a server.

## Server + client setup

```bash
# On the server (build with --features embeddings for semantic retrieval):
kimetsu-remote serve --addr 0.0.0.0:8787 --data /srv/kimetsu-brains \
  --token <secret> --rate-limit 120        # 120 req/min per token (0 = off)
#   one brain per repo under <data>/<repo-id>/; bearer-auth; plain HTTP. Put a
#   TLS proxy (nginx/Caddy) in front, or build `--features tls` and pass
#   --tls-cert/--tls-key for in-process HTTPS. `GET /healthz` and `GET /metrics`
#   (Prometheus text, aggregate-only) are unauthenticated. Prebuilt
#   kimetsu-remote binaries are built with embeddings + TLS support.
#
#   Add --org-brain /srv/kimetsu-org for a shared team brain: memories recorded
#   at `global_user` scope land there and merge into EVERY repo's retrieval
#   (project-scoped memories stay per-repo). Must be outside --data.
#
#   Add --repos-file repos.toml --checkout-dir /srv/checkouts to let the server
#   clone registered repos and ingest their files (remote file-capsule retrieval).

# On each client, wire a host at the remote instead of the local stdio command:
kimetsu plugin install claude-code --remote https://kimetsu.example.com:8787
kimetsu plugin install openclaw    --remote https://kimetsu.example.com:8787
```

The repo id is derived from your git remote (`--repo <id>` to override), so the
endpoint becomes `https://…/mcp/<repo-id>`. By default the host config
references `${KIMETSU_REMOTE_TOKEN}` (set that env var where your agent runs)
rather than writing the token to disk. Pass `--token <t>` to embed a literal.
The remote surfaces the memory/retrieval/curation tools by default.

## Retrieval quality

The server reranks `kimetsu_brain_context` results with a
cross-encoder (`--reranker`, default `jina-reranker-v1-tiny-en`, operator-level:
`"off"` disables, any curated/HF id accepted). Benchmark results on the 100-memory
dataset (production floors active, jina-tiny reranker):

| embedder          | MRR   | seq mean | rps  | peak RSS |
|-------------------|-------|----------|------|----------|
| jina-v2-base-code | 0.906 | 416ms    |  5.0 | 1.2 GB   |
| bge-small-en-v1.5 | 0.909 | 700ms    |  3.8 |  697 MB  |

The embedder is set per-repo via config or `KIMETSU_BRAIN_EMBEDDER`; the reranker
is operator-owned and cannot be overridden by a repo's `project.toml`.
See §7a "Retrieval models on the server" in
[HOW-KIMETSU-WORKS.md](HOW-KIMETSU-WORKS.md) for the full table and
how to re-run the benchmark.

## Server-side ingest (optional)

To make file-capsule retrieval work remotely,
let the server keep a managed clone of each repo. The operator pre-registers
repos in a TOML file (so clients can't make the server clone arbitrary URLs):

```toml
# repos.toml
[repos]
github-com-org-api = { url = "https://github.com/org/api.git", branch = "main" }
github-com-org-web = "https://github.com/org/web.git"
```

```bash
kimetsu-remote serve --data /srv/kimetsu-brains --token <secret> \
  --repos-file /etc/kimetsu/repos.toml --checkout-dir /srv/kimetsu-checkouts
```

Then `kimetsu_brain_ingest_repo` clones/refreshes the registered repo and indexes
its files into that repo's brain, so `context` retrieval includes file capsules.
Private repos use the server's own git auth (credential helper / SSH / a token in
the URL). The repo-id keys must match the ids clients connect with.

For the architecture (auth, rate limiting, org brain, the tool allowlist, the
write-tools gate), see §7a in [HOW-KIMETSU-WORKS.md](HOW-KIMETSU-WORKS.md).
