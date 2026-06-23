---
title: 'Local Models'
sidebar_position: 4
---

# Fully-local Kimetsu: zero external network calls

Kimetsu can run entirely on-device — no cloud embedder, no cloud reranker, no
cloud distiller. This guide shows how to achieve that with Ollama as the
cheap-model backend.

## What "fully local" means

| Component | Cloud default | Local alternative |
|---|---|---|
| Embedder | `jina-v2-base-code` (fastembed, local binary) | same — already local |
| Reranker | `ms-marco-tinybert-l-2-v2` (fastembed, local) | same — already local |
| Cheap model (distiller / harvester) | Anthropic / OpenAI API | **Ollama** — runs on your machine |

When all three are local, no bytes leave your machine during normal operation
(brain context injection, session-end distillation, consolidation distillation).

## Requirements

- **Ollama** installed and running: [https://ollama.com](https://ollama.com).
  Start the server with `ollama serve`.
- A small instruct model pulled locally. Recommended options:
  - `ollama pull qwen2.5:3b` (~2 GB, fast, excellent instruction following)
  - `ollama pull llama3.2:3b` (~2 GB, strong general reasoning)
  - `ollama pull qwen2.5:7b` for higher quality at ~4 GB

The embedder and reranker are already local (fastembed bundles them); no
additional setup is needed for those.

## Configuration

### Option A — new install (wizard)

```
kimetsu plugin install claude   # or codex
```

When asked "Which model should run the harvester?", enter `ollama`.
The wizard will ask for an optional base URL (leave blank for the default
`http://localhost:11434/v1`) and set the model to `qwen2.5:3b`.

### Option B — edit `project.toml` manually

Add a `[cheap_model]` section to your `.kimetsu/project.toml`:

```toml
[cheap_model]
enabled = true
provider = "ollama"
# Recommended small instruct models: qwen2.5:3b, llama3.2:3b
model = "qwen2.5:3b"
# Optional: override the endpoint via env var (default: http://localhost:11434/v1)
base_url_env = "OLLAMA_BASE_URL"
# Optional: only needed for authenticated / remote Ollama deployments
api_key_env = "OLLAMA_API_KEY"
```

No API key is required for a standard local Ollama install — leave the env
var unset or empty.

### Back-compat: existing `[learning.distiller]` configs

If you already have `[learning.distiller]` configured in your project, it
continues to work unchanged — `[cheap_model]` and `[learning.distiller]` are
equivalent; the former takes precedence when both are present.

## Verifying the setup

```
kimetsu doctor
```

When the cheap model is configured with `provider = "ollama"`, doctor probes
the endpoint and reports `reachable` or `unreachable` (informational — does not
fail doctor if Ollama is not currently running).

## How it works

Ollama exposes an OpenAI-compatible REST API at `http://localhost:11434/v1`.
Kimetsu's cheap-model client reuses the existing OpenAI provider path — the
`ollama` provider value simply sets the base URL to `http://localhost:11434/v1`
(or the `OLLAMA_BASE_URL` env-var override) and makes the API key optional.

At session end (the `SessionEnd` / Codex `Stop` hook), the configured cheap
model reads a bounded transcript view (≤12 000 characters), extracts 0–3
durable lessons, and records them via the confidence-gated brain API.  With
Ollama this call is entirely local.

## Quality and latency expectations

Kimetsu's distillation prompt is simple (structured JSON extraction), so a 3B
instruct model is sufficient for most users. Expect:

- **Latency**: 3–10 seconds per session end on a modern machine (CPU or GPU).
- **Quality**: comparable to cloud Haiku for straightforward lesson extraction;
  may miss subtle anti-patterns on complex sessions.

Use `kimetsu brain bench` or `kimetsu brain eval` to measure retrieval quality
on your own corpus.

## TODO (future work)

- `kimetsu ask` latency/quality benchmark with local vs. cloud cheap models
  (deferred to Flagship 3 — `kimetsu ask` does not exist yet).
- Automatic Ollama model-presence check in `kimetsu doctor` (currently doctor
  only probes TCP reachability, not whether the specific model is pulled).
