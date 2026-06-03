# Global distiller config in `~/.kimetsu/`

**Date:** 2026-06-02
**Status:** Approved (pending spec review)
**Builds on:** the SessionEnd distiller + setup wizard (same v0.9.0 line)

## Context

The SessionEnd distiller is configured **per-workspace** today: the wizard writes
`[learning.distiller]` + the key into the install workspace, and the hook reads
`discover(cwd)`'s project config. A `--scope global` install puts the `SessionEnd`
**hook** in `~/.claude/settings.json` (it fires in every project), but the
distiller **config** still only exists in the one workspace where setup ran — so
the global hook silently no-ops everywhere else.

This adds a **global distiller**: configure it once (`--scope global`) and have it
distill at the end of every session, recording into the always-available user
brain. `~/.kimetsu/` is already the user-brain home, so it becomes the home for
the global distiller config too — "everything global lives in `~/.kimetsu/`".

## Decisions (settled in brainstorming)

- **Recording scope (global):** globally-distilled lessons record with
  `MemoryScope::GlobalUser` → `~/.kimetsu/brain.db` (the user brain), available in
  every project. Reuses the existing GlobalUser routing.
- **Precedence:** workspace overrides global. If the current project's distiller
  is enabled, use it (project key, Project-scoped recording); otherwise fall back
  to the global distiller (global key, GlobalUser recording).
- **Config home:** `~/.kimetsu/project.toml` `[learning.distiller]` + `~/.kimetsu/.env`,
  resolved via `kimetsu_core::paths::user_kimetsu_dir()` (respects
  `KIMETSU_USER_BRAIN_DIR`; absent when the user brain is disabled).

## Key facts grounding the design

- `add_memory(start, MemoryScope::GlobalUser, kind, text)` routes to the user
  brain via an early return **before** `load_project(start)` (project.rs:436-440),
  so global recording works in any project — even one with no brain of its own.
  The user brain has no proposal queue, so global recording is add-or-dedup (no
  confidence gating); workspace recording keeps `propose_or_merge_memory`.
- `kimetsu_core::paths::{user_kimetsu_dir, user_brain_enabled}` already resolve the
  `~/.kimetsu` dir and the enabled flag.
- `resolve_env_value(repo_root, name)` checks process env then `<repo_root>/.env`,
  so a global key lookup is just `resolve_env_value(user_kimetsu_dir(), name)`
  (process env → `~/.kimetsu/.env`) — no change to that function.

## Components & files

### 1. Wizard write target by scope (`crates/kimetsu-cli/src/harvest_setup.rs`, `main.rs`)
`run_harvest_setup` currently takes `&ProjectPaths`. The global config doesn't fit
that layout (its `project.toml` and `.env` both live directly in `~/.kimetsu/`,
not at `repo_root` + `repo_root/.kimetsu/`). Replace the parameter with an explicit
target:
```rust
pub struct SetupTarget {
    pub project_toml: PathBuf, // where [learning.distiller] is written
    pub env_path: PathBuf,     // where the key/base URL are written
    pub gitignore_dir: PathBuf,// dir whose .gitignore must ignore ".env"
}
```
`apply_distiller_config(target.project_toml, model)` loads-or-defaults that toml,
sets the distiller fields, writes it back; `upsert_env_var(target.env_path, …)`;
`ensure_gitignored(target.gitignore_dir, ".env")`. No `discover`/`init_project`
(no climbing). The CLI gate builds the target from the install scope:
- **workspace:** `project_toml = <ws>/.kimetsu/project.toml`, `env = <ws>/.env`,
  `gitignore_dir = <ws>` (built from `ProjectPaths::at_root(&workspace)`).
- **global:** `dir = user_kimetsu_dir()` (skip with a message if `None`);
  `project_toml = dir/project.toml`, `env = dir/.env`, `gitignore_dir = dir`.

The wizard's success message names the scope ("global — applies to all projects"
vs "this workspace").

### 2. Distiller config resolution (`crates/kimetsu-cli/src/distiller.rs`)
A new resolver returns which distiller to run and how to record:
```rust
struct ResolvedDistiller {
    section: DistillerSection, // model, api_key_env, base_url_env
    key: String,
    base_url: Option<String>,
    scope: MemoryScope,        // Project or GlobalUser
    record_start: PathBuf,     // passed to the recorder
}

fn resolve_distiller(workspace: &Path) -> Option<ResolvedDistiller>
```
Order:
1. **Workspace:** `discover(workspace)` + `load_config`; if `distiller.enabled` and
   `resolve_env_value(repo_root, api_key_env)` is `Some` → `scope = Project`,
   `record_start = repo_root`, base via `resolve_env_value(repo_root, base_url_env)`.
2. **Global:** if `user_brain_enabled()` and `user_kimetsu_dir()` is `Some(dir)`,
   read `dir/project.toml` via `ProjectConfig::from_toml`; if `distiller.enabled`
   and `resolve_env_value(dir, api_key_env)` is `Some` → `scope = GlobalUser`,
   `record_start = workspace` (the cwd; `add_memory` ignores `start` on the
   GlobalUser path, so this never depends on the cwd having a brain), base via
   `resolve_env_value(dir, base_url_env)`.
3. Else `None`.

Edge: a workspace with `distiller.enabled = false` (or no `[learning.distiller]`)
falls through to global. Explicitly opting out per-project while a global distiller
is configured isn't supported (documented).

### 3. Recorder scope (`distill_and_record`)
Add a `scope: MemoryScope` parameter:
- `Project` → `project::propose_or_merge_memory(start, Project, kind, text, conf, …)`
  (confidence-gated, semantic merge — current behavior).
- `GlobalUser` → `project::add_memory(start, GlobalUser, kind, text)` (routes to the
  user brain; exact-text dedup; confidence not used by the user-brain write).

### 4. Hook entry (`run_session_end_hook`)
Replace the inline workspace-only config load with `resolve_distiller(workspace)`.
If `Some(r)`: build `AnthropicProvider::for_distiller(r.section.model, r.key,
r.base_url, timeout)`, build the transcript view, `distill_and_record(&r.record_start,
&view, &mut provider, r.scope)`; banner `"[Kimetsu] distilled N lesson(s) at session
end."` Else silent no-op. Best-effort throughout.

### 5. Stop-cue suppression (`brain_stop_hook`)
`should_emit_stop_harvest_cue` is unchanged; the call site now computes
`distiller_enabled = workspace.enabled || global.enabled`, where `global.enabled`
reads `~/.kimetsu/project.toml`'s distiller via a small `global_distiller_enabled()`
helper (false when the user brain is disabled / no `~/.kimetsu/project.toml`). So
either distiller suppresses the Stop end-of-session cue.

## Data flow (global distiller, no workspace distiller)

```
session ends (any project) → global SessionEnd hook → kimetsu brain session-end-hook
  → resolve_distiller(cwd): workspace disabled → read ~/.kimetsu/project.toml →
    global distiller enabled + key in ~/.kimetsu/.env → scope = GlobalUser
  → distill transcript with the global model → add_memory(_, GlobalUser, …)
    → ~/.kimetsu/brain.db  (available in every project)
```

## Error handling

Best-effort and silent on failure everywhere (no `~/.kimetsu`, user brain disabled,
disabled distiller, missing key, unreadable transcript, model/HTTP error, malformed
config). The wizard skips global setup with a message if `user_kimetsu_dir()` is
`None`. Never breaks session shutdown or the install.

## Testing

- **Wizard target by scope:** `run_harvest_setup` with a `SetupTarget` pointing at a
  temp "global dir" writes `project.toml` + `.env` there; with a workspace target
  writes under the workspace. (No `discover`; tests are deterministic.)
- **`resolve_distiller` precedence:** with a temp `KIMETSU_USER_BRAIN_DIR` (under the
  env lock) — workspace-enabled wins (Project); workspace-absent + global-enabled →
  global (GlobalUser); neither → `None`. Key-absent disables each tier.
- **`distill_and_record` GlobalUser:** writes to a temp user brain (`KIMETSU_USER_BRAIN_DIR`
  + `with_user_brain_disabled`-style lock); assert the lesson lands in the user brain
  via `list_user_memories`.
- **Stop suppression:** `global_distiller_enabled()` true (global toml) suppresses the
  cue even with no workspace distiller.

## Risks / trade-offs

- **`--scope global` writes `[learning.distiller]` into the user-brain
  `~/.kimetsu/project.toml`** — intentional (the global config home); `brain.db`
  untouched.
- **No per-project opt-out** when a global distiller is configured (disabled
  workspace distiller falls through to global). Documented; a future explicit
  `disabled` sentinel could opt out.
- **Global recording is exact-dedup only** (user brain has no proposal queue) — fine
  for distilled lessons; cross-session duplicates collapse by normalized text.

## Out of scope / follow-ups

- OpenAI / Codex distiller providers (still deferred).
- A `kimetsu brain distiller test` command to validate the configured endpoint.
- Per-project explicit opt-out sentinel.
