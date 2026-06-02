# Global Distiller Config Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let `kimetsu plugin install --scope global` configure a distiller once (in `~/.kimetsu/`) that distills every session and records into the always-available user brain, with workspace distillers taking precedence.

**Architecture:** The distiller config gets a second home — `~/.kimetsu/project.toml` `[learning.distiller]` + `~/.kimetsu/.env`. A `resolve_distiller(cwd)` resolver picks the workspace distiller (record `Project`) if enabled, else the global one (record `GlobalUser` → `~/.kimetsu/brain.db`). The wizard writes to the workspace or `~/.kimetsu` per `--scope`.

**Tech Stack:** Rust; `kimetsu_core::paths::{user_kimetsu_dir, user_brain_enabled}`; `kimetsu_brain::project::{add_memory, propose_or_merge_memory}` (GlobalUser already routes to the user brain).

**Spec:** `docs/superpowers/specs/2026-06-02-global-distiller-design.md`

## File Map
- **Modify** `crates/kimetsu-cli/src/distiller.rs` — `distill_and_record` gains a `scope`; new `ResolvedDistiller` + `resolve_distiller`/`resolve_distiller_with`; `global_distiller_enabled`/`global_distiller_enabled_in`; `run_session_end_hook` uses the resolver.
- **Modify** `crates/kimetsu-cli/src/harvest_setup.rs` — `SetupTarget` struct; `run_harvest_setup`/`apply_distiller_config` take it.
- **Modify** `crates/kimetsu-cli/src/main.rs` — CLI gate builds `SetupTarget` per `--scope`; `brain_stop_hook` ORs in the global distiller.

---

## Task 1: `distill_and_record` records by scope

**Files:** Modify `crates/kimetsu-cli/src/distiller.rs` (the `distill_and_record` fn + its caller in `run_session_end_hook` + the existing/new tests).

- [ ] **Step 1: Write the failing test**

Add a test helper + test to `distiller.rs` `mod tests` (the `text_response` helper already exists there):

```rust
    /// Run `f` with the user brain pointed at a temp dir (enabled), under
    /// the process-wide env lock, restoring the previous env afterward.
    fn with_user_brain_dir<R>(dir: &std::path::Path, f: impl FnOnce() -> R) -> R {
        let _g = kimetsu_brain::user_brain::test_env_lock()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let prev_dir = std::env::var("KIMETSU_USER_BRAIN_DIR").ok();
        let prev_en = std::env::var("KIMETSU_USER_BRAIN").ok();
        // SAFETY: scoped by the shared lock.
        unsafe {
            std::env::set_var("KIMETSU_USER_BRAIN_DIR", dir);
            std::env::remove_var("KIMETSU_USER_BRAIN");
        }
        let out = f();
        unsafe {
            match prev_dir {
                Some(v) => std::env::set_var("KIMETSU_USER_BRAIN_DIR", v),
                None => std::env::remove_var("KIMETSU_USER_BRAIN_DIR"),
            }
            match prev_en {
                Some(v) => std::env::set_var("KIMETSU_USER_BRAIN", v),
                None => std::env::remove_var("KIMETSU_USER_BRAIN"),
            }
        }
        out
    }

    #[test]
    fn distill_and_record_global_writes_to_user_brain() {
        let dir = std::env::temp_dir().join(format!(
            "kimetsu_userbrain_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        with_user_brain_dir(&dir, || {
            let mut provider = MockProvider::new([text_response(
                "[{\"lesson\":\"Global lesson kept everywhere\",\"tags\":[\"x\"],\"confidence\":0.9}]",
            )]);
            // `start` is ignored on the GlobalUser path; pass the temp dir.
            let n = distill_and_record(&dir, "user: a", &mut provider, MemoryScope::GlobalUser);
            assert_eq!(n, 1);
            let conn = kimetsu_brain::user_brain::open_user_brain_readonly()
                .unwrap()
                .expect("user brain exists");
            let mems = kimetsu_brain::user_brain::list_user_memories(&conn).unwrap();
            assert!(mems.iter().any(|m| m.text.contains("Global lesson kept everywhere")));
        });
        std::fs::remove_dir_all(dir).ok();
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p kimetsu-cli distill_and_record_global_writes_to_user_brain`
Expected: FAIL — `distill_and_record` takes 3 args, not 4 (arity mismatch).

- [ ] **Step 3: Add the `scope` parameter**

Replace `distill_and_record` with:

```rust
/// Distill lessons from `view` and record each. `Project` scope uses the
/// confidence-gated `propose_or_merge_memory` (workspace brain); `GlobalUser`
/// uses `add_memory`, which routes to `~/.kimetsu/brain.db` (the user brain
/// has no proposal queue, so this is add-or-dedup). Returns the count recorded.
pub fn distill_and_record(
    start: &Path,
    view: &str,
    provider: &mut dyn ModelProvider,
    scope: MemoryScope,
) -> usize {
    let mut recorded = 0;
    for lesson in distill_lessons(view, provider) {
        // Mirror kimetsu_brain_record's MCP kind mapping; semantic_operator + default store as Fact.
        let kind = match lesson.kind.as_str() {
            "anti_pattern" => MemoryKind::FailurePattern,
            "convention" => MemoryKind::Convention,
            _ => MemoryKind::Fact,
        };
        let text = lesson.lesson.trim();
        let ok = match scope {
            MemoryScope::GlobalUser => project::add_memory(start, MemoryScope::GlobalUser, kind, text).is_ok(),
            _ => project::propose_or_merge_memory(
                start,
                scope,
                kind,
                text,
                lesson.confidence.clamp(0.0, 1.0),
                "auto-harvested at session end",
            )
            .is_ok(),
        };
        if ok {
            recorded += 1;
        }
    }
    recorded
}
```

- [ ] **Step 4: Update the existing caller + the existing temp-brain test**

In `run_session_end_hook`, the current call is `distill_and_record(&paths.repo_root, &view, &mut provider)`. Change it to pass the project scope for now (Task 2 reworks this fn fully):

```rust
    let recorded = distill_and_record(&paths.repo_root, &view, &mut provider, MemoryScope::Project);
```

In the existing `distill_and_record_writes_to_a_temp_brain` test, add the scope arg to its call:

```rust
            let n = distill_and_record(&root, "user: a\nuser: b", &mut provider, MemoryScope::Project);
```

- [ ] **Step 5: Run tests**

Run: `cargo test -p kimetsu-cli distiller`
Expected: PASS (existing distiller tests + the new GlobalUser test).

If `list_user_memories` or `open_user_brain_readonly` aren't `pub` at `kimetsu_brain::user_brain::…`, confirm their path (they are used from `project.rs` as `user_brain::list_user_memories` / `open_user_brain_readonly`, so they are `pub`). `MemoryRow` (returned by `list_user_memories`) has a `.text` field.

- [ ] **Step 6: Commit**

```bash
git add crates/kimetsu-cli/src/distiller.rs
git commit -m "feat: distill_and_record records by MemoryScope (GlobalUser -> user brain)"
```

---

## Task 2: `resolve_distiller` — workspace-overrides-global resolution

**Files:** Modify `crates/kimetsu-cli/src/distiller.rs` (new types/fns + rewrite `run_session_end_hook`; tests).

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` (uses `with_user_brain_dir` from Task 1 only indirectly — here we inject the global dir directly, so no env mutation needed):

```rust
    fn write_distiller_toml(dir: &std::path::Path, enabled: bool, model: &str) {
        std::fs::create_dir_all(dir).unwrap();
        let toml = format!(
            "[learning.distiller]\nenabled = {enabled}\nprovider = \"anthropic\"\n\
             model = \"{model}\"\napi_key_env = \"ANTHROPIC_API_KEY\"\n\
             base_url_env = \"ANTHROPIC_BASE_URL\"\n"
        );
        std::fs::write(dir.join("project.toml"), toml).unwrap();
    }

    #[test]
    fn resolve_distiller_global_when_no_workspace() {
        // No workspace config; a global dir with an enabled distiller + a key in
        // its .env -> resolves to the global distiller, GlobalUser scope.
        let ws = std::env::temp_dir().join(format!(
            "km_rd_ws_{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&ws).unwrap();
        let gdir = std::env::temp_dir().join(format!(
            "km_rd_g_{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        write_distiller_toml(&gdir, true, "claude-haiku-4-5");
        std::fs::write(gdir.join(".env"), "ANTHROPIC_API_KEY=sk-global\n").unwrap();

        let r = resolve_distiller_with(&ws, Some(gdir.clone())).expect("global resolved");
        assert_eq!(r.scope, MemoryScope::GlobalUser);
        assert_eq!(r.model, "claude-haiku-4-5");
        assert_eq!(r.key, "sk-global");

        // Disabled global -> None.
        write_distiller_toml(&gdir, false, "claude-haiku-4-5");
        assert!(resolve_distiller_with(&ws, Some(gdir.clone())).is_none());

        std::fs::remove_dir_all(ws).ok();
        std::fs::remove_dir_all(gdir).ok();
    }

    #[test]
    fn resolve_distiller_workspace_wins() {
        // Workspace dir (its own git toplevel) with an enabled distiller + .env
        // key wins over a global dir.
        let ws = std::env::temp_dir().join(format!(
            "km_rd_wsw_{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(ws.join(".kimetsu")).unwrap();
        kimetsu_core::paths::git_init_boundary(&ws);
        write_distiller_toml(&ws.join(".kimetsu"), true, "ws-model");
        std::fs::write(ws.join(".env"), "ANTHROPIC_API_KEY=sk-ws\n").unwrap();

        let gdir = std::env::temp_dir().join(format!(
            "km_rd_gw_{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        write_distiller_toml(&gdir, true, "g-model");
        std::fs::write(gdir.join(".env"), "ANTHROPIC_API_KEY=sk-global\n").unwrap();

        let r = resolve_distiller_with(&ws, Some(gdir.clone())).expect("workspace resolved");
        assert_eq!(r.scope, MemoryScope::Project);
        assert_eq!(r.model, "ws-model");
        assert_eq!(r.key, "sk-ws");

        std::fs::remove_dir_all(ws).ok();
        std::fs::remove_dir_all(gdir).ok();
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p kimetsu-cli resolve_distiller`
Expected: FAIL — `resolve_distiller_with`/`ResolvedDistiller` not found.

- [ ] **Step 3: Add the resolver**

Add near the top of `distiller.rs` (after the imports), extending the existing `use` lines as needed:

```rust
use kimetsu_core::config::ProjectConfig;
use kimetsu_core::paths::{user_brain_enabled, user_kimetsu_dir};
```

Add the type + functions (place above `run_session_end_hook`):

```rust
/// The distiller selected for this session: which model/key/endpoint to use,
/// and how to record (project vs the global user brain).
pub struct ResolvedDistiller {
    pub model: String,
    pub key: String,
    pub base_url: Option<String>,
    pub timeout_secs: u64,
    pub scope: MemoryScope,
    pub record_start: std::path::PathBuf,
}

/// Resolve the distiller for `workspace`, preferring the workspace distiller
/// over the global one (`~/.kimetsu`). `None` when neither is enabled +
/// credentialed.
pub fn resolve_distiller(workspace: &Path) -> Option<ResolvedDistiller> {
    let global_dir = if user_brain_enabled() {
        user_kimetsu_dir()
    } else {
        None
    };
    resolve_distiller_with(workspace, global_dir)
}

/// Testable core: `global_dir` is injected (the `~/.kimetsu` dir, or `None`).
fn resolve_distiller_with(
    workspace: &Path,
    global_dir: Option<std::path::PathBuf>,
) -> Option<ResolvedDistiller> {
    // 1. Workspace distiller (Project scope).
    if let Ok(paths) = ProjectPaths::discover(workspace)
        && let Ok(config) = project::load_config(&paths)
    {
        let d = &config.learning.distiller;
        if d.enabled
            && d.provider == "anthropic"
            && let Some(key) = resolve_env_value(&paths.repo_root, &d.api_key_env)
        {
            return Some(ResolvedDistiller {
                model: d.model.clone(),
                key,
                base_url: resolve_env_value(&paths.repo_root, &d.base_url_env),
                timeout_secs: config.model.request_timeout_secs,
                scope: MemoryScope::Project,
                record_start: paths.repo_root.clone(),
            });
        }
    }
    // 2. Global distiller (GlobalUser scope).
    if let Some(dir) = global_dir
        && let Ok(text) = std::fs::read_to_string(dir.join("project.toml"))
        && let Ok(config) = ProjectConfig::from_toml(&text)
    {
        let d = &config.learning.distiller;
        if d.enabled
            && d.provider == "anthropic"
            && let Some(key) = resolve_env_value(&dir, &d.api_key_env)
        {
            return Some(ResolvedDistiller {
                model: d.model.clone(),
                key,
                base_url: resolve_env_value(&dir, &d.base_url_env),
                timeout_secs: config.model.request_timeout_secs,
                scope: MemoryScope::GlobalUser,
                record_start: workspace.to_path_buf(),
            });
        }
    }
    None
}
```

- [ ] **Step 4: Rewrite `run_session_end_hook` to use the resolver**

Replace the whole `run_session_end_hook` body with:

```rust
pub fn run_session_end_hook(workspace: &Path) {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).ok();
    let payload: serde_json::Value =
        serde_json::from_str(input.trim()).unwrap_or(serde_json::Value::Null);

    let Some(transcript_path) = payload
        .get("transcript_path")
        .and_then(|v| v.as_str())
        .filter(|p| !p.trim().is_empty())
    else {
        return;
    };
    let Some(resolved) = resolve_distiller(workspace) else {
        return;
    };
    let view = build_transcript_view(transcript_path, MAX_VIEW_CHARS);
    if view.trim().is_empty() {
        return;
    }
    let Ok(mut provider) = AnthropicProvider::for_distiller(
        &resolved.model,
        resolved.key,
        resolved.base_url,
        resolved.timeout_secs,
    ) else {
        return;
    };
    let recorded =
        distill_and_record(&resolved.record_start, &view, &mut provider, resolved.scope);
    if recorded > 0 {
        println!(
            "[Kimetsu] distilled {recorded} lesson{} at session end.",
            if recorded == 1 { "" } else { "s" }
        );
    }
}
```

This removes the now-unused inline `config`/`distiller`/`api_key`/`base_url` locals. The imports `resolve_env_value`, `AnthropicProvider`, `project`, `ProjectPaths` are still used (by the resolver). Remove any import that becomes unused after this change (the compiler will warn).

- [ ] **Step 5: Run tests + build**

Run: `cargo test -p kimetsu-cli distiller`
Expected: PASS (the two new resolve tests + prior ones).

Run: `cargo build -p kimetsu-cli`
Expected: clean (no unused-import warnings; fix any the compiler flags).

- [ ] **Step 6: Commit**

```bash
git add crates/kimetsu-cli/src/distiller.rs
git commit -m "feat: resolve_distiller (workspace overrides global ~/.kimetsu)"
```

---

## Task 3: `global_distiller_enabled` for Stop-cue suppression

**Files:** Modify `crates/kimetsu-cli/src/distiller.rs` (new fns + test).

- [ ] **Step 1: Write the failing test**

Add to `mod tests`:

```rust
    #[test]
    fn global_distiller_enabled_reads_global_toml() {
        let gdir = std::env::temp_dir().join(format!(
            "km_gde_{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        write_distiller_toml(&gdir, true, "claude-haiku-4-5");
        assert!(global_distiller_enabled_in(Some(gdir.clone())));

        write_distiller_toml(&gdir, false, "claude-haiku-4-5");
        assert!(!global_distiller_enabled_in(Some(gdir.clone())));

        assert!(!global_distiller_enabled_in(None));
        std::fs::remove_dir_all(gdir).ok();
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p kimetsu-cli global_distiller_enabled_reads_global_toml`
Expected: FAIL — `global_distiller_enabled_in` not found.

- [ ] **Step 3: Add the functions**

In `distiller.rs`:

```rust
/// True when a global distiller (`~/.kimetsu/project.toml`) is enabled. Used by
/// the Stop hook to suppress its end-of-session cue (the distiller owns it).
pub fn global_distiller_enabled() -> bool {
    let dir = if user_brain_enabled() {
        user_kimetsu_dir()
    } else {
        None
    };
    global_distiller_enabled_in(dir)
}

fn global_distiller_enabled_in(global_dir: Option<std::path::PathBuf>) -> bool {
    global_dir
        .and_then(|dir| std::fs::read_to_string(dir.join("project.toml")).ok())
        .and_then(|text| ProjectConfig::from_toml(&text).ok())
        .map(|c| c.learning.distiller.enabled)
        .unwrap_or(false)
}
```

- [ ] **Step 4: Run test**

Run: `cargo test -p kimetsu-cli global_distiller_enabled`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/kimetsu-cli/src/distiller.rs
git commit -m "feat: global_distiller_enabled helper for Stop-cue suppression"
```

---

## Task 4: `SetupTarget` — wizard writes to workspace or `~/.kimetsu`

**Files:** Modify `crates/kimetsu-cli/src/harvest_setup.rs` (struct + signatures + tests).

- [ ] **Step 1: Write the failing test**

Replace the test module's `paths_for` helper and the two wizard tests with target-based versions. Add to `harvest_setup.rs` `mod tests`:

```rust
    fn target_at(dir: &Path) -> SetupTarget {
        SetupTarget {
            project_toml: dir.join(".kimetsu").join("project.toml"),
            env_path: dir.join(".env"),
            gitignore_dir: dir.to_path_buf(),
        }
    }
```

Replace the three wizard tests' bodies (`wizard_writes_env_and_config`,
`wizard_declined_writes_nothing`, `wizard_unrecognized_harness_aborts`) to use a
`SetupTarget` and read via `root` directly. The happy-path test in full:

```rust
    #[test]
    fn wizard_writes_env_and_config() {
        let root = std::env::temp_dir().join(format!(
            "kimetsu_wizard_{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir_all(root.join(".kimetsu")).unwrap();
        let mut input =
            Cursor::new("y\nclaude\nsk-litellm-123\nhttp://localhost:4000\n\n".as_bytes().to_vec());
        let mut output = Vec::new();
        let configured =
            run_harvest_setup(&mut input, &mut output, &target_at(&root), "this workspace").unwrap();
        assert!(configured);

        let env = fs::read_to_string(root.join(".env")).unwrap();
        assert!(env.contains("ANTHROPIC_API_KEY=sk-litellm-123"));
        assert!(env.contains("ANTHROPIC_BASE_URL=http://localhost:4000"));
        let toml = fs::read_to_string(root.join(".kimetsu").join("project.toml")).unwrap();
        assert!(toml.contains("enabled = true"));
        assert!(toml.contains("claude-haiku-4-5"));
        assert!(fs::read_to_string(root.join(".gitignore")).unwrap().contains(".env"));
        fs::remove_dir_all(root).ok();
    }
```

For `wizard_declined_writes_nothing` and `wizard_unrecognized_harness_aborts`:
same as the current versions, but (a) build the target with `let paths = target_at(&root);`
(after `fs::create_dir_all(&root).unwrap();`), (b) call
`run_harvest_setup(&mut input, &mut output, &paths, "this workspace")`, and (c) assert
`!root.join(".env").exists()`. Keep the `upsert_env_var_replaces_existing` test unchanged.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p kimetsu-cli harvest_setup`
Expected: FAIL — `SetupTarget` not found / arity mismatch.

- [ ] **Step 3: Add `SetupTarget` and re-thread the wizard**

In `harvest_setup.rs`, change the imports and add the struct:

```rust
use std::fs;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use kimetsu_core::config::ProjectConfig;

/// Where the wizard writes the distiller config + secret. Built by the CLI gate
/// from the install scope (workspace dir, or `~/.kimetsu`).
pub struct SetupTarget {
    pub project_toml: PathBuf,
    pub env_path: PathBuf,
    pub gitignore_dir: PathBuf,
}
```

(Remove the now-unused `use kimetsu_core::paths::ProjectPaths;` if nothing else references it.)

Change `run_harvest_setup`'s signature + the write block:

```rust
pub fn run_harvest_setup<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    target: &SetupTarget,
    scope_label: &str,
) -> std::io::Result<bool> {
```

(keep the prompt flow unchanged through the model prompt), then replace the write block at the end:

```rust
    apply_distiller_config(&target.project_toml, &model)?;
    // Gitignore `.env` BEFORE writing the secret into it.
    ensure_gitignored(&target.gitignore_dir, ".env")?;
    upsert_env_var(&target.env_path, "ANTHROPIC_API_KEY", &key)?;
    if !base_url.is_empty() {
        upsert_env_var(&target.env_path, "ANTHROPIC_BASE_URL", &base_url)?;
    }

    writeln!(
        writer,
        "\u{2713} Distiller configured for {scope_label} (model {model}). \
         Key stored in {} (gitignored). Note: the key was entered in plain text.",
        target.env_path.display()
    )?;
    Ok(true)
}
```

Change `apply_distiller_config` to take the toml path directly:

```rust
fn apply_distiller_config(project_toml: &Path, model: &str) -> std::io::Result<()> {
    let io_err =
        |e: Box<dyn std::error::Error + Send + Sync>| std::io::Error::other(e.to_string());
    let mut config = if project_toml.exists() {
        ProjectConfig::from_toml(&fs::read_to_string(project_toml)?).map_err(io_err)?
    } else {
        // <root>/.kimetsu/project.toml -> project_id from <root>.
        let project_id = project_toml
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("workspace")
            .to_string();
        ProjectConfig::default_for_project(project_id)
    };
    config.learning.distiller.enabled = true;
    config.learning.distiller.provider = "anthropic".to_string();
    config.learning.distiller.model = model.to_string();
    config.learning.distiller.api_key_env = "ANTHROPIC_API_KEY".to_string();
    config.learning.distiller.base_url_env = "ANTHROPIC_BASE_URL".to_string();
    if let Some(parent) = project_toml.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(project_toml, config.to_toml().map_err(io_err)?)
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p kimetsu-cli harvest_setup`
Expected: PASS (4 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/kimetsu-cli/src/harvest_setup.rs
git commit -m "feat: wizard writes to an explicit SetupTarget (workspace or ~/.kimetsu)"
```

---

## Task 5: CLI gate + Stop hook wiring

**Files:** Modify `crates/kimetsu-cli/src/main.rs` (the `plugin` Install arm gate; `brain_stop_hook`).

- [ ] **Step 1: Update the CLI gate to build a `SetupTarget` by scope**

In the `plugin` fn's `Install` arm, replace the interactive block (the `if matches!(target, BridgeTarget::ClaudeCode) && !args.no_setup && interactive { … }` body) with:

```rust
            if matches!(target, BridgeTarget::ClaudeCode) && !args.no_setup && interactive {
                let target_for_scope = match scope {
                    InstallScope::Global => match kimetsu_core::paths::user_kimetsu_dir() {
                        Some(dir) => Some((
                            harvest_setup::SetupTarget {
                                project_toml: dir.join("project.toml"),
                                env_path: dir.join(".env"),
                                gitignore_dir: dir,
                            },
                            "globally (all projects, ~/.kimetsu)",
                        )),
                        None => {
                            eprintln!(
                                "kimetsu plugin install: cannot resolve ~/.kimetsu; skipping distiller setup."
                            );
                            None
                        }
                    },
                    InstallScope::Workspace => {
                        let p = kimetsu_core::paths::ProjectPaths::at_root(&workspace);
                        Some((
                            harvest_setup::SetupTarget {
                                project_toml: p.project_toml.clone(),
                                env_path: p.repo_root.join(".env"),
                                gitignore_dir: p.repo_root.clone(),
                            },
                            "this workspace",
                        ))
                    }
                };
                if let Some((setup_target, label)) = target_for_scope {
                    let stdin = std::io::stdin();
                    let mut reader = stdin.lock();
                    let mut stdout = std::io::stdout();
                    if let Err(err) = harvest_setup::run_harvest_setup(
                        &mut reader,
                        &mut stdout,
                        &setup_target,
                        label,
                    ) {
                        eprintln!("kimetsu plugin install: distiller setup skipped: {err}");
                    }
                }
            }
```

(`scope` and `workspace` are in scope in the `Install` arm; `InstallScope` is already imported in `fn plugin`.)

- [ ] **Step 2: Update `brain_stop_hook` to OR in the global distiller**

In `brain_stop_hook`, the config load is:

```rust
    let (auto_harvest, distiller_enabled) = paths
        .as_ref()
        .and_then(|p| project::load_config(p).ok())
        .map(|c| (c.learning.auto_harvest, c.learning.distiller.enabled))
        .unwrap_or((true, false));
```

Change the `distiller_enabled` to also consider the global distiller:

```rust
    let (auto_harvest, workspace_distiller) = paths
        .as_ref()
        .and_then(|p| project::load_config(p).ok())
        .map(|c| (c.learning.auto_harvest, c.learning.distiller.enabled))
        .unwrap_or((true, false));
    let distiller_enabled = workspace_distiller || distiller::global_distiller_enabled();
```

- [ ] **Step 3: Build + test**

Run: `cargo build -p kimetsu-cli`
Expected: clean.

Run: `cargo test -p kimetsu-cli`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/kimetsu-cli/src/main.rs
git commit -m "feat: --scope global wizard target + global distiller Stop suppression"
```

---

## Task 6: Verification + docs

- [ ] **Step 1: Full build/test/fmt**

Run: `cargo build --workspace` → clean.
Run: `cargo test --workspace` → all green.
Run: `cargo fmt --all && cargo fmt --all --check` → clean.

- [ ] **Step 2: Manual global smoke (sandbox `KIMETSU_USER_BRAIN_DIR`, non-home)**

```bash
g=$(mktemp -d); ws=$(mktemp -d)
KIMETSU_USER_BRAIN_DIR="$(cygpath -m "$g")" \
  printf 'y\nclaude\nsk-global\n\n\n' | \
  KIMETSU_USER_BRAIN_DIR="$(cygpath -m "$g")" target/debug/kimetsu.exe \
  plugin install claude-code --scope global --workspace "$(cygpath -m "$ws")" --setup-harvest
cat "$g/project.toml" | grep -A2 distiller     # enabled = true at ~/.kimetsu (sandbox)
cat "$g/.env"                                   # ANTHROPIC_API_KEY=sk-global
ls "$ws/.env" 2>/dev/null && echo "WS POLLUTED" || echo "workspace clean (global only) ✓"
rm -rf "$g" "$ws"
```

Expected: the distiller config + key land in the sandbox global dir, not the workspace.

- [ ] **Step 3: Update CHANGELOG + README**

Extend the v0.9.0 distiller CHANGELOG bullet to mention `--scope global` configures a global distiller in `~/.kimetsu/` recording to the user brain (workspace overrides global). Add a sentence to the README distiller note.

- [ ] **Step 4: Commit**

```bash
git add CHANGELOG.md README.md
git commit -m "docs: document the global distiller (~/.kimetsu)"
```

---

## Done

`--scope global` now configures a distiller in `~/.kimetsu/` that distills every session and records into the user brain (`MemoryScope::GlobalUser`), with per-workspace distillers taking precedence. The Stop hook suppresses its end-of-session cue when either a workspace or global distiller is enabled.
