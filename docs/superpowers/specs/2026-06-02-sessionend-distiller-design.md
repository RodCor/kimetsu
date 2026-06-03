# Interactive setup for a credentialed SessionEnd distiller

**Date:** 2026-06-02
**Status:** Approved (pending spec review)
**Builds on:** v0.9.0 auto-harvest (in-agent harvester subagent + `[learning] auto_harvest` + the Stop/PostToolUse cues)

## Context

v0.9.0 made the hooks *generate* memories via an **in-agent** path: a hook emits a
`[kimetsu-harvest]` cue and the already-running Claude Code agent dispatches a
background `kimetsu-memory-harvester` subagent. That path needs no credentials but
is **agent-mediated** (the agent must choose to act) and uses the agent's own model.

This feature adds an opt-in, **credentialed, kimetsu-side SessionEnd distiller**: at
install time an interactive wizard lets the user configure a cheap model (with an API
key, and optionally a LiteLLM base URL). When configured, Kimetsu itself distills the
session transcript at session end and records memories — deterministic, not dependent
on agent compliance. This is the previously-deferred "credentialed SessionEnd
distiller," now opt-in via setup.

Intended outcome: a user can run `kimetsu plugin install claude-code`, answer a short
setup prompt, and from then on get memories auto-harvested at the end of each session
by a cheap configured model (e.g. `claude-haiku-4-5`, or any Anthropic-compatible
endpoint incl. LiteLLM), with zero per-session manual action.

## Decisions (settled in brainstorming)

- **Architecture = Split.** When the distiller is *enabled*, the **Stop** hook
  suppresses its end-of-session `[kimetsu-harvest]` cue (the SessionEnd distiller owns
  end-of-session). The **PostToolUse** resolved-failure cue (mid-session, in-agent)
  **stays**. The Stop record-count banner stays. When the distiller is *disabled*,
  v0.9.0 behavior is unchanged.
- **Secret storage = kimetsu `.env`.** The wizard writes the API key (and optional base
  URL) to a gitignored `.env`, read via the existing `resolve_env_value`.
- **Provider scope = Anthropic only** this iteration (covers Anthropic API *and*
  LiteLLM via a base URL with the same `x-api-key` auth). Codex and OpenAI are
  placeholders/deferred.

## Non-goals

- No OpenAI provider, no Codex distiller (codex is a placeholder option in the wizard
  that reports "not yet supported").
- No hidden/no-echo secret input (the key is read as a normal line; noted to the user).
  A keychain/`apiKeyHelper` path is out of scope.
- No change to the v0.9.0 in-agent harvester subagent (it remains the no-setup default).

## Components & files

### 1. Config: `DistillerSection` (`crates/kimetsu-core/src/config.rs`)
Extend `LearningSection` with a nested distiller config:
```toml
[learning]
auto_harvest = true
[learning.distiller]
enabled  = false                 # wizard sets true
provider = "anthropic"
model    = "claude-haiku-4-5"
api_key_env  = "ANTHROPIC_API_KEY"
base_url_env = "ANTHROPIC_BASE_URL"   # optional; "" = use default endpoint
```
`#[serde(default)]` everywhere so pre-existing `project.toml` files keep loading
(default `enabled = false`). A `DistillerSection::default()` yields a disabled,
Anthropic/`claude-haiku-4-5` config. Back-compat test mirrors the existing
`auto_harvest` default test.

**Secret storage:** the wizard writes the key (and optional base URL) to the workspace's
gitignored `<ws>/.env`:
```
ANTHROPIC_API_KEY=...
ANTHROPIC_BASE_URL=http://localhost:4000   # only if LiteLLM
```
**The distiller config is workspace-scoped this iteration** — it lives in the workspace's
`<ws>/.kimetsu/project.toml` with the key in `<ws>/.env`. This holds even when
`plugin install` runs with `--scope global`: the SessionEnd hook is installed globally
and fires in every project, but only the workspace whose `project.toml` has
`distiller.enabled = true` actually distills — elsewhere the hook is a silent no-op. (A
*global* distiller config that applies to all projects is a follow-up; see Out of scope.)

### 2. Provider base URL (`crates/kimetsu-agent/src/anthropic.rs`)
- Add an optional `base_url: Option<String>` to `AnthropicProvider`, passed in by the
  distiller builder (resolved from the distiller's `base_url_env` via `.env`/process env).
- `complete()` POSTs to `format!("{}/v1/messages", base.trim_end_matches('/'))` when a
  base URL is set, else the existing `MESSAGES_URL`. Auth header unchanged (`x-api-key`
  — LiteLLM accepts it). A constructor `AnthropicProvider::for_distiller(model, api_key,
  base_url, timeout)` builds it directly from resolved values (so the distiller controls
  model/key/base independent of the project's main `[model]` section). Unit test: base
  URL → request goes to `<base>/v1/messages`; no base URL → default.

### 3. Key resolution (`crates/kimetsu-cli` distiller path)
The distiller resolves its key/base via the existing `resolve_env_value(repo_root, name)`:
process env → `<repo_root>/.env`. Since the config + key are workspace-scoped (§1) and the
SessionEnd hook runs with the project as cwd, `<repo_root>/.env` is the workspace `.env` —
the value is found with no new resolver. (A `~/.kimetsu/.env` global fallback is part of
the deferred "global distiller config" follow-up.)

### 4. SessionEnd distiller command (`crates/kimetsu-cli/src/main.rs`)
New `kimetsu brain session-end-hook` subcommand:
- Reads the SessionEnd payload from stdin (`{session_id, transcript_path, reason, cwd}`).
- Loads the workspace config; if `learning.distiller.enabled` is false → silent exit.
- Resolves the key (§3); if absent → silent exit (best-effort).
- Streams the transcript JSONL (reuse the bounded `count_transcript_jsonl` pattern;
  here, collect a *capped* compact view — last N messages / M chars — of user+assistant
  text + tool failures, never the whole file in one String).
- Builds an `AnthropicProvider::for_distiller(...)` and asks for 0–3 generalizable
  lessons as a strict JSON array `[{lesson, tags[], kind, confidence}]` (same selection
  rubric as the harvester subagent's prompt — favor resolved tool-failures / hard-won
  fixes; record nothing if trivial).
- Parses the JSON (tolerant: extract the first JSON array; skip on parse failure) and
  calls `project::propose_or_merge_memory` for each lesson (confidence-gated: ≥0.7
  added, <0.7 proposed; semantic de-dup/merge handles overlap with any in-agent records).
- Prints a one-line `[Kimetsu] distilled N lesson(s) at session end` banner; never errors
  out (a hook must not break shutdown).
- Distillation logic factored into a pure-ish helper (`distill_lessons(transcript, &mut
  dyn ModelProvider) -> Vec<Lesson>`) so it can be tested with a mock provider.

### 5. Bridge wiring (`crates/kimetsu-chat/src/bridge.rs`)
- `write_claude_hooks` also upserts a `SessionEnd` Kimetsu group →
  `kimetsu brain session-end-hook` (Claude) / the `--workspace .` variant. Idempotent via
  the existing `upsert_kimetsu_hook`. It's cheap when the distiller is disabled (loads
  config, returns), so wiring it unconditionally is fine and lets a user enable the
  distiller later by editing config without reinstalling.
- Codex: no SessionEnd distiller this iteration (placeholder).

### 6. Stop-cue suppression (`crates/kimetsu-cli/src/main.rs`)
In `brain_stop_hook`, when `learning.distiller.enabled` is true, skip the end-of-session
`[kimetsu-harvest]` cue (return after the record-count banner). PostToolUse resolution
cue is untouched.

### 7. Interactive wizard (`crates/kimetsu-cli/src/main.rs`)
At the end of a `kimetsu plugin install` for the `claude-code` target:
- Run only when **stdin+stdout are a TTY** and the user didn't pass `--no-setup`
  (and not when `--setup-harvest` forces it on). Non-interactive → skipped silently.
- Flow (hand-rolled prompts over stdin; no new dependency):
  1. `Set up auto-harvest distiller now? [y/N]` → default No.
  2. `Harness:` `claude` (supported) / `codex` (prints "not yet supported", aborts setup).
  3. `Anthropic API key (or LiteLLM key):` (read a line).
  4. `ANTHROPIC_BASE_URL (optional, blank for Anthropic; set for LiteLLM):` (read a line).
  5. `Model [claude-haiku-4-5]:` (blank → default).
- Writes: the key/base to the scope-appropriate `.env` (ensuring `.env` is gitignored),
  and `[learning.distiller]` (enabled=true, provider, model, api_key_env, base_url_env)
  into the workspace/global project config (creating a default config if none exists).
- The wizard's pure logic is a function `run_harvest_setup<R: BufRead, W: Write>(reader,
  writer, paths, scope) -> SetupOutcome` so it's unit-testable by feeding scripted input;
  the CLI wraps it with the real TTY + the `is_terminal()` gate.

## Data flow (enabled distiller)

```
session ends → Claude Code runs SessionEnd hook
  → `kimetsu brain session-end-hook` (stdin: transcript_path)
    → load config; distiller.enabled? resolve key? (else silent exit)
    → stream + cap transcript → distill_lessons(view, AnthropicProvider[base_url])
    → for each lesson: propose_or_merge_memory (confidence-gated)
    → print "[Kimetsu] distilled N lesson(s)"
```

## Error handling

Every new path is best-effort and silent on failure (no brain, disabled, missing key,
unreadable transcript, model/HTTP error, malformed JSON) — a hook must never break the
agent's turn or session shutdown. The wizard aborts cleanly on EOF/empty/`codex` and
leaves the install otherwise complete.

## Testing

- `config.rs`: distiller default + pre-existing-toml back-compat (mirrors `auto_harvest`).
- `anthropic.rs`: base-URL routing (set → `<base>/v1/messages`; unset → default).
- `main.rs`: `distill_lessons` with a `MockProvider` returning a JSON array → expected
  `Lesson`s, and a malformed response → empty; `run_harvest_setup` over scripted input →
  correct `.env` + config writes (using temp dirs); Stop-cue suppression when distiller
  enabled.
- Hook-level smoke: pipe a SessionEnd payload to `kimetsu brain session-end-hook` against
  a temp workspace with a stub provider env (or assert silent no-op when disabled).

## Risks / trade-offs

- **Cost:** one cheap-model call per session end. Gated behind opt-in + `enabled`.
- **Key in `.env` plaintext** (gitignored) — accepted per the storage decision.
- **Per-workspace config:** distiller setup is per-workspace this iteration; a global
  install's SessionEnd hook no-ops in projects that haven't run setup (a global distiller
  config is a follow-up).
- **Double-harvest** with the mid-session PostToolUse cue — accepted (the "Split"); the
  confidence-gated `propose_or_merge_memory` de-dups semantically.

## Out of scope / follow-ups

- **Global distiller config** — one `~/.kimetsu/` config + key applying to all projects
  (with a `~/.kimetsu/.env` resolution fallback), rather than per-workspace.
- OpenAI (`gpt-5.4-mini`) distiller provider; Codex SessionEnd distiller.
- No-echo secret entry / keychain / `apiKeyHelper`.
- A `kimetsu brain distiller test` command to validate the configured endpoint.
