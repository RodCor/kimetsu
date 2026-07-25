//! v0.5.3: CLI plumbing smoke tests.
//!
//! Shells out to the built `kimetsu` binary and verifies argument
//! parsing, subcommand wiring, and JSON output schemas don't drift.
//! Deliberately small (~5 tests, no model calls, no API keys) — the
//! purpose is to catch regressions that the in-process
//! `kimetsu-e2e` tests can't see by construction.
//!
//! Resolves the binary path via `CARGO_BIN_EXE_kimetsu`, which Cargo
//! sets when running integration tests for a crate that defines a
//! `[[bin]]`. No PATH hacks required.

use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

// C7 hook tests use kimetsu_brain and kimetsu_core directly for project setup.
// Both are direct [dependencies] of kimetsu-cli so they're available in
// integration tests.
use kimetsu_brain::project as brain_project;
use kimetsu_core::ids::RunId;

/// Path to the freshly-built `kimetsu` binary. Cargo injects
/// `CARGO_BIN_EXE_<name>` for each `[[bin]]` declared in the crate
/// being tested.
fn kimetsu_bin() -> &'static str {
    env!("CARGO_BIN_EXE_kimetsu")
}

#[test]
fn kimetsu_version_prints_a_version_string_and_exits_clean() {
    let output = Command::new(kimetsu_bin())
        .arg("--version")
        .output()
        .expect("spawn kimetsu --version");
    assert!(
        output.status.success(),
        "kimetsu --version should exit 0; got {:?}, stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("kimetsu"),
        "kimetsu --version should mention 'kimetsu'; got: {stdout}"
    );
    // QQ2: --version must also include the build flavor.
    assert!(
        stdout.contains("(embeddings") || stdout.contains("(lean"),
        "kimetsu --version should contain build flavor '(embeddings' or '(lean'; got: {stdout}"
    );
}

#[test]
fn kimetsu_help_lists_top_level_subcommands() {
    let output = Command::new(kimetsu_bin())
        .arg("--help")
        .output()
        .expect("spawn kimetsu --help");
    assert!(output.status.success(), "kimetsu --help should exit 0");

    let stdout = String::from_utf8_lossy(&output.stdout);
    // Spot-check the key user-facing subcommands.
    // If one of these disappears, --help drift caught at PR time.
    for expected in [
        "init",
        "brain",
        "chat",
        "doctor",
        "bridge",
        "mcp",
        "update",
        "uninstall",
    ] {
        assert!(
            stdout.contains(expected),
            "kimetsu --help should mention `{expected}`; got: {stdout}"
        );
    }
}

#[test]
fn kimetsu_uninstall_help_lists_confirmation_flags() {
    let output = Command::new(kimetsu_bin())
        .args(["uninstall", "--help"])
        .output()
        .expect("spawn kimetsu uninstall --help");
    assert!(output.status.success(), "uninstall --help should exit 0");

    let stdout = String::from_utf8_lossy(&output.stdout);
    for expected in ["--yes", "--dry-run", "--delete-user-data", "--keep-plugins"] {
        assert!(
            stdout.contains(expected),
            "uninstall --help should mention `{expected}`; got: {stdout}"
        );
    }
}

#[test]
fn kimetsu_update_help_lists_check_mode() {
    let output = Command::new(kimetsu_bin())
        .args(["update", "--help"])
        .output()
        .expect("spawn kimetsu update --help");
    assert!(output.status.success(), "update --help should exit 0");

    let stdout = String::from_utf8_lossy(&output.stdout);
    for expected in ["--check", "--dry-run", "--flavor"] {
        assert!(
            stdout.contains(expected),
            "update --help should mention `{expected}`; got: {stdout}"
        );
    }
}

#[test]
fn kimetsu_brain_help_lists_brain_subcommands() {
    let output = Command::new(kimetsu_bin())
        .args(["brain", "--help"])
        .output()
        .expect("spawn kimetsu brain --help");
    assert!(output.status.success(), "brain --help should exit 0");

    let stdout = String::from_utf8_lossy(&output.stdout);
    for expected in ["memory", "ingest", "rebuild"] {
        assert!(
            stdout.contains(expected),
            "brain --help should mention `{expected}`; got: {stdout}"
        );
    }
}

#[test]
fn kimetsu_brain_memory_help_lists_v05_subcommands() {
    let output = Command::new(kimetsu_bin())
        .args(["brain", "memory", "--help"])
        .output()
        .expect("spawn kimetsu brain memory --help");
    assert!(output.status.success(), "brain memory --help should exit 0");

    let stdout = String::from_utf8_lossy(&output.stdout);
    // v0.5.0 added `blame`, v0.5.2 added `conflicts`. Both must be
    // discoverable via --help or the user can't find them.
    for expected in ["add", "list", "blame", "conflicts"] {
        assert!(
            stdout.contains(expected),
            "brain memory --help should mention `{expected}`; got: {stdout}"
        );
    }
}

#[test]
fn kimetsu_brain_insights_help_lists_args() {
    let output = Command::new(kimetsu_bin())
        .args(["brain", "insights", "--help"])
        .output()
        .expect("spawn kimetsu brain insights --help");
    assert!(
        output.status.success(),
        "brain insights --help should exit 0; got {:?}, stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    for expected in ["--json", "--last-n-runs", "--since", "--top"] {
        assert!(
            stdout.contains(expected),
            "brain insights --help should mention `{expected}`; got: {stdout}"
        );
    }
}

// ---------------------------------------------------------------------------
// C7: context-hook miss-logging and env-var suppression
// ---------------------------------------------------------------------------

/// Helper: initialise a temp project dir and return its path.
fn temp_project_dir(label: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("kimetsu-smoke-hook-{label}-{}", RunId::new()));
    fs::create_dir_all(&root).expect("create temp dir");
    kimetsu_core::paths::git_init_boundary(&root);
    brain_project::init_project(&root, false).expect("init_project");
    root
}

/// Count `context.served` events by running `kimetsu brain insights --json`
/// and parsing the `retrieval.served` field.
fn count_context_served_via_insights(bin: &str, root: &std::path::Path) -> u64 {
    let out = Command::new(bin)
        .args(["brain", "insights", "--json"])
        .current_dir(root)
        .env("KIMETSU_USER_BRAIN", "0")
        .output()
        .expect("spawn insights");
    if !out.status.success() {
        return 0;
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
    v.get("retrieval")
        .and_then(|r| r.get("served"))
        .and_then(|s| s.as_u64())
        .unwrap_or(0)
}

#[test]
fn context_hook_miss_logs_context_served_event() {
    // The hook should log a context.served event even when the brain
    // returns zero capsules (the "miss" path). Use a prompt long enough
    // to pass the 10-char guard.
    let root = temp_project_dir("hook_miss");
    let bin = kimetsu_bin();

    let before = count_context_served_via_insights(bin, &root);

    let mut child = Command::new(bin)
        .args(["brain", "context-hook", "--workspace"])
        .arg(&root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("KIMETSU_BRAIN_LOG_RETRIEVAL", "1") // explicitly ON
        .env("KIMETSU_USER_BRAIN", "0") // disable user brain for isolation
        .spawn()
        .expect("spawn context-hook");

    // Write a prompt long enough to pass the 10-char guard.
    if let Some(mut stdin) = child.stdin.take() {
        let _ =
            stdin.write_all(br#"{"prompt": "investigate this failing test in the CI pipeline"}"#);
    }
    let _ = child.wait().expect("wait context-hook");

    let after = count_context_served_via_insights(bin, &root);
    assert!(
        after > before,
        "context-hook should log at least one context.served event on a miss; \
         before={before}, after={after} in {:?}",
        root
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn context_hook_suppressed_when_env_var_zero() {
    let root = temp_project_dir("hook_suppress");
    let bin = kimetsu_bin();

    let before = count_context_served_via_insights(bin, &root);

    let mut child = Command::new(bin)
        .args(["brain", "context-hook", "--workspace"])
        .arg(&root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("KIMETSU_BRAIN_LOG_RETRIEVAL", "0") // SUPPRESSED
        .env("KIMETSU_USER_BRAIN", "0")
        .spawn()
        .expect("spawn context-hook suppressed");

    if let Some(mut stdin) = child.stdin.take() {
        let _ =
            stdin.write_all(br#"{"prompt": "investigate this failing test in the CI pipeline"}"#);
    }
    let _ = child.wait().expect("wait context-hook suppressed");

    let after = count_context_served_via_insights(bin, &root);
    assert_eq!(
        after, before,
        "KIMETSU_BRAIN_LOG_RETRIEVAL=0 should suppress context.served logging; \
         before={before}, after={after} in {:?}",
        root
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn kimetsu_unknown_subcommand_exits_nonzero_with_helpful_message() {
    let output = Command::new(kimetsu_bin())
        .arg("not-a-real-subcommand")
        .output()
        .expect("spawn kimetsu with bogus subcommand");
    assert!(
        !output.status.success(),
        "kimetsu with bogus subcommand should NOT exit 0"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    // clap's standard "unrecognized subcommand" wording — if this
    // changes upstream we update; the test asserts the user gets
    // SOMETHING actionable on stderr.
    assert!(
        !stderr.trim().is_empty(),
        "stderr should not be empty on bad subcommand"
    );
}

/// Read the `(memory_id, use_count)` of the first memory from
/// `brain memory list --json`.
fn first_memory(bin: &str, root: &std::path::Path) -> Option<(String, u64)> {
    let out = Command::new(bin)
        .args(["brain", "memory", "list", "--json"])
        .current_dir(root)
        .env("KIMETSU_USER_BRAIN", "0")
        .output()
        .expect("spawn memory list");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let arr = v
        .as_array()
        .or_else(|| v.get("memories").and_then(|m| m.as_array()))?;
    let first = arr.first()?;
    let id = first.get("memory_id")?.as_str()?.to_string();
    let uc = first.get("use_count").and_then(|u| u.as_u64()).unwrap_or(0);
    Some((id, uc))
}

/// v3.0 #3 (fleet write-safety): MULTI-PROCESS concurrency proof. Spawns several
/// `kimetsu` processes that hammer `brain cite` on the same memory in the same
/// brain.db at once, then asserts no citation was lost (final use_count equals
/// the total) and the projection rebuilds cleanly. `#[ignore]` because it spawns
/// dozens of processes (slow); run explicitly with
/// `cargo test --test cli_smoke -- --ignored concurrent_processes`.
#[test]
#[ignore = "spawns many processes; run on demand"]
fn concurrent_processes_lose_no_cites() {
    let root = temp_project_dir("fleet-concurrency");
    let bin = kimetsu_bin();

    // Seed one project-scoped memory via add-batch (stdin).
    let mut child = Command::new(bin)
        .args(["brain", "memory", "add-batch", "-"])
        .current_dir(&root)
        .env("KIMETSU_USER_BRAIN", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn add-batch");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"text":"fleet concurrency target memory","scope":"project","kind":"fact"}"#)
        .expect("write batch");
    assert!(
        child.wait().expect("wait add-batch").success(),
        "add-batch failed"
    );

    let (mem_id, _) = first_memory(bin, &root).expect("seeded memory id");

    const PROCS: usize = 4;
    const CITES_PER_PROC: usize = 8;
    let root = std::sync::Arc::new(root);
    let mem_id = std::sync::Arc::new(mem_id);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(PROCS));

    let mut handles = Vec::new();
    for _ in 0..PROCS {
        let root = std::sync::Arc::clone(&root);
        let mem_id = std::sync::Arc::clone(&mem_id);
        let barrier = std::sync::Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            for _ in 0..CITES_PER_PROC {
                let ok = Command::new(kimetsu_bin())
                    .args(["brain", "cite", "--memory-id", &mem_id])
                    .current_dir(&*root)
                    .env("KIMETSU_USER_BRAIN", "0")
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .expect("spawn cite")
                    .success();
                assert!(ok, "concurrent `brain cite` process must succeed");
            }
        }));
    }
    for h in handles {
        h.join().expect("join");
    }

    let expected = (PROCS * CITES_PER_PROC) as u64;
    let (_, use_count) = first_memory(bin, &root).expect("memory after cites");
    assert_eq!(
        use_count, expected,
        "multi-process lost updates: got {use_count}, expected {expected}"
    );

    // Rebuild is stable.
    let rebuilt = Command::new(bin)
        .args(["brain", "rebuild"])
        .current_dir(&*root)
        .env("KIMETSU_USER_BRAIN", "0")
        .status()
        .expect("spawn rebuild")
        .success();
    assert!(rebuilt, "rebuild should succeed");
    let (_, after) = first_memory(bin, &root).expect("memory after rebuild");
    assert_eq!(after, expected, "rebuild changed the use_count");
}

// ---------------------------------------------------------------------------
// v3.0: first-turn warm start for hosts with no session-start event
// ---------------------------------------------------------------------------

/// Run `kimetsu brain context-hook` against `root` with `payload` on stdin and
/// return its stdout.
///
/// `KIMETSU_USER_BRAIN_DIR` points the per-session sidecar (and the user brain)
/// at `cache_home` so these tests never touch the developer's real `~/.kimetsu`.
fn run_context_hook(
    root: &std::path::Path,
    cache_home: &std::path::Path,
    extra_args: &[&str],
    payload: &str,
) -> String {
    let mut cmd = Command::new(kimetsu_bin());
    cmd.args(["brain", "context-hook", "--workspace"])
        .arg(root)
        .args(extra_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env("KIMETSU_USER_BRAIN", "0")
        .env("KIMETSU_USER_BRAIN_DIR", cache_home);
    let mut child = cmd.spawn().expect("spawn context-hook");
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(payload.as_bytes());
    }
    let out = child.wait_with_output().expect("wait context-hook");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Regression guard for the hosts Kimetsu could not speak first on.
///
/// Codex, Pi and OpenClaw expose only a per-turn hook — no session-start event —
/// so the repo digest and episodic resume have to ride along with the session's
/// first prompt. Exactly once: the block is expensive and the agent only needs
/// orienting once per session.
#[test]
fn context_hook_warm_starts_once_per_session() {
    let root = temp_project_dir("warm_first_prompt");
    let cache_home = root.join("cache-home");
    fs::create_dir_all(&cache_home).expect("create cache home");

    // Seed a memory so the digest has something to report.
    brain_project::add_memory(
        &root,
        kimetsu_core::memory::MemoryScope::Project,
        kimetsu_core::memory::MemoryKind::Convention,
        "Regenerate the schema with `cargo xtask gen` before committing",
    )
    .expect("add_memory");

    let payload = r#"{"session_id":"warm-session-1","prompt":"investigate this failing test in the CI pipeline"}"#;

    let first = run_context_hook(&root, &cache_home, &["--warm-on-first-prompt"], payload);
    assert!(
        first.contains("## Repo context"),
        "first prompt of a session must carry the warm-start block; got: {first}"
    );

    let second = run_context_hook(&root, &cache_home, &["--warm-on-first-prompt"], payload);
    assert!(
        !second.contains("## Repo context"),
        "warm start must not repeat on later turns of the same session; got: {second}"
    );

    let _ = fs::remove_dir_all(&root);
}

/// Claude Code has its own `SessionStart` hook, so it must NOT pass
/// `--warm-on-first-prompt` — and without the flag the hook must stay exactly
/// as it was, or those users would get the block twice.
#[test]
fn context_hook_without_the_flag_never_warm_starts() {
    let root = temp_project_dir("warm_opt_in");
    let cache_home = root.join("cache-home");
    fs::create_dir_all(&cache_home).expect("create cache home");

    brain_project::add_memory(
        &root,
        kimetsu_core::memory::MemoryScope::Project,
        kimetsu_core::memory::MemoryKind::Convention,
        "Regenerate the schema with `cargo xtask gen` before committing",
    )
    .expect("add_memory");

    let payload = r#"{"session_id":"warm-session-2","prompt":"investigate this failing test in the CI pipeline"}"#;
    let out = run_context_hook(&root, &cache_home, &[], payload);
    assert!(
        !out.contains("## Repo context"),
        "warm start is opt-in per host; got: {out}"
    );

    let _ = fs::remove_dir_all(&root);
}

/// A prompt too short for retrieval still gets the warm start: the guard exists
/// to skip meaningless *retrieval*, not to withhold the session's orientation.
#[test]
fn context_hook_warm_starts_even_on_a_short_prompt() {
    let root = temp_project_dir("warm_short_prompt");
    let cache_home = root.join("cache-home");
    fs::create_dir_all(&cache_home).expect("create cache home");

    brain_project::add_memory(
        &root,
        kimetsu_core::memory::MemoryScope::Project,
        kimetsu_core::memory::MemoryKind::Fact,
        "The broker halves budget_tokens before filling capsules",
    )
    .expect("add_memory");

    let payload = r#"{"session_id":"warm-session-3","prompt":"hi"}"#;
    let out = run_context_hook(&root, &cache_home, &["--warm-on-first-prompt"], payload);
    assert!(
        out.contains("## Repo context"),
        "a short prompt must still receive the warm start; got: {out}"
    );

    let _ = fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------
// v3.0: the Free/Deep tier
// ---------------------------------------------------------------------------

/// Read `kimetsu brain status --json` for `root`.
fn brain_status_json(root: &std::path::Path) -> serde_json::Value {
    let out = Command::new(kimetsu_bin())
        .args(["brain", "status", "--json"])
        .current_dir(root)
        .env("KIMETSU_USER_BRAIN", "0")
        .output()
        .expect("spawn brain status");
    serde_json::from_slice(&out.stdout).unwrap_or_default()
}

/// Rewrite `project.toml` with `extra` appended.
fn append_to_project_toml(root: &std::path::Path, extra: &str) {
    let paths = kimetsu_core::paths::ProjectPaths::discover(root).expect("paths");
    let mut text = fs::read_to_string(&paths.project_toml).expect("read project.toml");
    text.push('\n');
    text.push_str(extra);
    fs::write(&paths.project_toml, text).expect("write project.toml");
}

/// A brand-new brain is Free — that is what the "zero LLM calls in the memory
/// pipeline" claim is measured on, so it must be the shipped default.
#[test]
fn brain_status_reports_the_free_tier_by_default() {
    let root = temp_project_dir("tier_default");
    let status = brain_status_json(&root);
    assert_eq!(status["tier"], "free", "got: {status}");
    assert_eq!(status["tier_downgraded"], false);
    let _ = fs::remove_dir_all(&root);
}

/// Auto-resolution: a brain that already has a cheap model configured is
/// already making model calls, so it reports Deep without anyone editing a
/// tier field. This is what keeps every pre-v3.0 config behaving as it did.
#[test]
fn configuring_a_cheap_model_resolves_to_the_deep_tier() {
    let root = temp_project_dir("tier_auto_deep");
    append_to_project_toml(
        &root,
        "[cheap_model]\nenabled = true\nprovider = \"ollama\"\nmodel = \"qwen2.5:7b\"\n",
    );
    let status = brain_status_json(&root);
    assert_eq!(status["tier"], "deep", "got: {status}");
    let _ = fs::remove_dir_all(&root);
}

/// `tier = "deep"` with nothing to run on is Free wearing a label. It must
/// resolve down AND be reported, or a user reads "deep" while every Deep
/// feature is silently off.
#[test]
fn deep_without_a_model_downgrades_and_is_reported() {
    let root = temp_project_dir("tier_downgrade");
    append_to_project_toml(&root, "");
    // Set the tier without configuring any model.
    let paths = kimetsu_core::paths::ProjectPaths::discover(&root).expect("paths");
    let text = fs::read_to_string(&paths.project_toml).expect("read");
    let patched = text.replace("[kimetsu]", "[kimetsu]\ntier = \"deep\"");
    fs::write(&paths.project_toml, patched).expect("write");

    let status = brain_status_json(&root);
    assert_eq!(status["tier"], "free", "must resolve down; got: {status}");
    assert_eq!(
        status["tier_downgraded"], true,
        "the discrepancy must be visible; got: {status}"
    );
    let _ = fs::remove_dir_all(&root);
}

/// `tier = "free"` is a durable opt-out: credentials present, pipeline model
/// calls off. Without it the only way to stop the distiller is to remove the
/// credentials, which is not a thing you can do per-project.
#[test]
fn explicit_free_tier_overrides_a_configured_model() {
    let root = temp_project_dir("tier_explicit_free");
    append_to_project_toml(
        &root,
        "[cheap_model]\nenabled = true\nprovider = \"ollama\"\nmodel = \"qwen2.5:7b\"\n",
    );
    let paths = kimetsu_core::paths::ProjectPaths::discover(&root).expect("paths");
    let text = fs::read_to_string(&paths.project_toml).expect("read");
    fs::write(
        &paths.project_toml,
        text.replace("[kimetsu]", "[kimetsu]\ntier = \"free\""),
    )
    .expect("write");

    let status = brain_status_json(&root);
    assert_eq!(status["tier"], "free", "got: {status}");
    assert_eq!(status["tier_downgraded"], false, "free was asked for");
    let _ = fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------
// v3.0: fusion rule selection
// ---------------------------------------------------------------------------

/// `[broker] fusion` must be wired end to end, and must be a no-op on the lean
/// build: with only a lexical ranking there is nothing to fuse, so rank fusion
/// is the identity and FTS-only users see byte-identical results.
#[test]
fn fusion_mode_is_wired_and_is_a_no_op_on_the_lean_path() {
    let root = temp_project_dir("fusion_wiring");
    for text in [
        "[tags: sqlite wal] Checkpoint the WAL before copying brain.db",
        "[tags: sqlite wal] Opening a WAL database read-only skips recovery",
        "[tags: rust cargo] Regenerate the schema with cargo xtask gen",
    ] {
        brain_project::add_memory(
            &root,
            kimetsu_core::memory::MemoryScope::Project,
            kimetsu_core::memory::MemoryKind::Convention,
            text,
        )
        .expect("add_memory");
    }

    let context = |mode: &str| -> String {
        let paths = kimetsu_core::paths::ProjectPaths::discover(&root).expect("paths");
        let text = fs::read_to_string(&paths.project_toml).expect("read");
        let patched = if text.contains("fusion = ") {
            text.lines()
                .map(|l| {
                    if l.trim_start().starts_with("fusion = ") {
                        format!("fusion = \"{mode}\"")
                    } else {
                        l.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            text.replace("[broker]", &format!("[broker]\nfusion = \"{mode}\""))
        };
        fs::write(&paths.project_toml, patched).expect("write");

        let out = Command::new(kimetsu_bin())
            .args(["brain", "context", "wal recovery", "--json"])
            .current_dir(&root)
            .env("KIMETSU_USER_BRAIN", "0")
            .output()
            .expect("spawn brain context");
        assert!(
            out.status.success(),
            "brain context must succeed with fusion={mode}; stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    };

    let linear = context("linear");
    let rrf = context("rrf");
    assert!(
        linear.contains("capsules"),
        "expected a context bundle; got: {linear}"
    );
    // Capsule ids are fresh ULIDs per retrieval, so compare the summaries.
    let summaries = |json: &str| -> Vec<String> {
        let v: serde_json::Value = serde_json::from_str(json).unwrap_or_default();
        v["capsules"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|c| c["summary"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    assert_eq!(
        summaries(&linear),
        summaries(&rrf),
        "with one candidate list, rank fusion must not reshuffle anything"
    );

    // A typo must degrade to the previous behaviour rather than break retrieval.
    let typo = context("reciprocal-rank");
    assert_eq!(summaries(&typo), summaries(&linear));

    let _ = fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------
// v3.0: proactive failure detection + the injection policy
// ---------------------------------------------------------------------------

/// Run `kimetsu brain posttool-hook` with a PostToolUse payload and return
/// whatever it printed.
fn run_posttool_hook(
    root: &std::path::Path,
    cache_home: &std::path::Path,
    session: &str,
    command: &str,
    tool_response: &str,
) -> String {
    let payload = serde_json::json!({
        "session_id": session,
        "tool_name": "Bash",
        "tool_input": { "command": command },
        "tool_response": tool_response,
    })
    .to_string();

    let mut child = Command::new(kimetsu_bin())
        .args(["brain", "posttool-hook", "--workspace"])
        .arg(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env("KIMETSU_USER_BRAIN", "0")
        .env("KIMETSU_USER_BRAIN_DIR", cache_home)
        .spawn()
        .expect("spawn posttool-hook");
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(payload.as_bytes());
    }
    let out = child.wait_with_output().expect("wait posttool-hook");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn seeded_proactive_project(label: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let root = temp_project_dir(label);
    let cache_home = root.join("cache-home");
    fs::create_dir_all(&cache_home).expect("create cache home");
    brain_project::add_memory(
        &root,
        kimetsu_core::memory::MemoryScope::Project,
        kimetsu_core::memory::MemoryKind::FailurePattern,
        "cargo test needs --test-threads=1; the brain tests share on-disk state",
    )
    .expect("add_memory");
    (root, cache_home)
}

/// A real compile error surfaces the matching lesson.
#[test]
fn a_real_failure_surfaces_a_matching_memory() {
    let (root, cache_home) = seeded_proactive_project("proactive_real_failure");
    let out = run_posttool_hook(
        &root,
        &cache_home,
        "s1",
        "cargo test",
        "error[E0433]: failed to resolve: use of undeclared crate `foo`",
    );
    assert!(
        out.contains("additionalContext"),
        "a real failure should surface the lesson; got: {out:?}"
    );
    let _ = fs::remove_dir_all(&root);
}

/// The regression this slice exists for: a fully passing test run contains the
/// word "failed" on its summary line and used to trigger an interruption.
#[test]
fn a_passing_test_run_does_not_trigger_a_proactive_interruption() {
    let (root, cache_home) = seeded_proactive_project("proactive_passing_run");
    let out = run_posttool_hook(
        &root,
        &cache_home,
        "s2",
        "cargo test",
        "running 517 tests\n\ntest result: ok. 517 passed; 0 failed; 1 ignored",
    );
    assert!(
        out.trim().is_empty(),
        "a passing test run is not a failure; got: {out:?}"
    );
    let _ = fs::remove_dir_all(&root);
}

/// An untrained brain must report — and behave as — the legacy threshold rule.
/// This is the upgrade-safety property: nothing changes until there is data.
#[test]
fn the_injection_policy_starts_as_the_legacy_rule_and_records_its_decisions() {
    let (root, cache_home) = seeded_proactive_project("policy_status");

    let status = |root: &std::path::Path| -> serde_json::Value {
        let out = Command::new(kimetsu_bin())
            .args(["brain", "policy", "--json", "--workspace"])
            .arg(root)
            .env("KIMETSU_USER_BRAIN", "0")
            .env("KIMETSU_USER_BRAIN_DIR", &cache_home)
            .output()
            .expect("spawn brain policy");
        serde_json::from_slice(&out.stdout).unwrap_or_default()
    };

    let before = status(&root);
    assert_eq!(before["trained"], false, "got: {before}");
    assert_eq!(before["labelled_examples"], 0);
    assert!(
        (before["weights"]["score"].as_f64().unwrap_or(0.0) - 20.0).abs() < 1e-6,
        "the prior must be the pinned legacy boundary; got: {before}"
    );

    run_posttool_hook(
        &root,
        &cache_home,
        "s3",
        "cargo test",
        "error[E0433]: failed to resolve: use of undeclared crate `foo`",
    );

    let after = status(&root);
    assert!(
        after["labelled_examples"].as_u64().unwrap_or(0) >= 1,
        "an injection decision must be recorded as training data; got: {after}"
    );
    assert_eq!(
        after["trained"], false,
        "one example is nowhere near enough to retrain"
    );

    let _ = fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------
// v3.0: background maintenance
// ---------------------------------------------------------------------------

/// The gap this closes: consolidation, digest refresh, prune detection and
/// skill graduation were all CLI commands a human had to remember to run, so
/// on a real brain none of them ever ran.
#[test]
fn maintenance_runs_what_is_due_and_then_stops() {
    let root = temp_project_dir("maintenance");
    brain_project::add_memory(
        &root,
        kimetsu_core::memory::MemoryScope::Project,
        kimetsu_core::memory::MemoryKind::Convention,
        "[tags: sqlite wal] Checkpoint the WAL before copying brain.db",
    )
    .expect("add_memory");

    let maintain = |args: &[&str]| -> String {
        let out = Command::new(kimetsu_bin())
            .args(["brain", "maintain"])
            .args(args)
            .arg("--workspace")
            .arg(&root)
            .env("KIMETSU_USER_BRAIN", "0")
            .output()
            .expect("spawn brain maintain");
        assert!(
            out.status.success(),
            "brain maintain must succeed; stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    };

    // A brain that has never run upkeep has everything due.
    let status = maintain(&["--status"]);
    assert!(
        status.contains("4 pass(es) due"),
        "everything is due on a fresh brain; got: {status}"
    );

    // Running it does the work and reports per pass.
    let ran = maintain(&[]);
    for pass in ["reinforce", "digest", "prune", "skills"] {
        assert!(ran.contains(pass), "{pass} should have run; got: {ran}");
    }
    assert!(!ran.contains('✗'), "no pass should fail here; got: {ran}");

    // And immediately after, nothing is due — upkeep must not spin.
    assert!(
        maintain(&["--status"]).contains("nothing due"),
        "a completed pass must not be due again immediately"
    );
    assert!(maintain(&[]).contains("nothing due"));

    // --force ignores the schedule.
    let forced: serde_json::Value =
        serde_json::from_str(&maintain(&["--force", "--json"])).expect("json");
    assert_eq!(
        forced.as_array().map(Vec::len),
        Some(4),
        "--force runs every pass: {forced}"
    );

    let _ = fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------
// v3.0: as-of (bitemporal) queries
// ---------------------------------------------------------------------------

/// The question default retrieval cannot answer: what did the agent know then?
///
/// The semantics are unit-tested in `kimetsu_brain::bitemporal`; this covers
/// the command surface — date parsing, the JSON shape, and that a memory
/// written after the as-of point is genuinely absent.
#[test]
fn as_of_reports_what_the_brain_believed_at_a_point_in_time() {
    let root = temp_project_dir("as_of");
    brain_project::add_memory(
        &root,
        kimetsu_core::memory::MemoryScope::Project,
        kimetsu_core::memory::MemoryKind::Convention,
        "Run cargo fmt before committing",
    )
    .expect("add_memory");

    let as_of = |when: &str| -> serde_json::Value {
        let out = Command::new(kimetsu_bin())
            .args(["brain", "as-of", when, "--json", "--workspace"])
            .arg(&root)
            .env("KIMETSU_USER_BRAIN", "0")
            .output()
            .expect("spawn brain as-of");
        assert!(
            out.status.success(),
            "brain as-of {when} should succeed; stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).expect("json")
    };

    // A bare date is accepted and read as midnight UTC.
    let past = as_of("2025-01-01");
    assert_eq!(past["as_of"], "2025-01-01T00:00:00Z");
    assert_eq!(
        past["count"], 0,
        "nothing had been written yet; got: {past}"
    );

    // A full RFC 3339 timestamp works too.
    let future = as_of("2099-01-01T00:00:00Z");
    assert_eq!(
        future["count"], 1,
        "the memory exists by then; got: {future}"
    );

    // `--since` reports the delta rather than the view.
    let out = Command::new(kimetsu_bin())
        .args([
            "brain",
            "as-of",
            "2099-01-01",
            "--since",
            "2025-01-01",
            "--json",
            "--workspace",
        ])
        .arg(&root)
        .env("KIMETSU_USER_BRAIN", "0")
        .output()
        .expect("spawn brain as-of --since");
    let delta: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(
        delta["learned"].as_array().map(Vec::len),
        Some(1),
        "the memory was learned in that window; got: {delta}"
    );
    assert_eq!(delta["retired"].as_array().map(Vec::len), Some(0));

    // An unparseable time is an error, not a silently empty result.
    let bad = Command::new(kimetsu_bin())
        .args(["brain", "as-of", "last tuesday", "--workspace"])
        .arg(&root)
        .env("KIMETSU_USER_BRAIN", "0")
        .output()
        .expect("spawn");
    assert!(
        !bad.status.success(),
        "a bad time must not read as 'nothing'"
    );

    let _ = fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------
// v3.0: standing preferences in the warm start
// ---------------------------------------------------------------------------

/// Preference following is the second-weakest measured ability, and the
/// diagnosis is that a preference is semantically far from the question. So it
/// must reach the agent WITHOUT being retrieved — on a query that shares no
/// words with it.
#[test]
fn standing_preferences_reach_the_agent_without_being_retrieved() {
    let root = temp_project_dir("standing_prefs");
    let cache_home = root.join("cache-home");
    fs::create_dir_all(&cache_home).expect("create cache home");

    brain_project::add_memory(
        &root,
        kimetsu_core::memory::MemoryScope::Project,
        kimetsu_core::memory::MemoryKind::Preference,
        "Prefer thiserror for library error types",
    )
    .expect("add preference");
    brain_project::add_memory(
        &root,
        kimetsu_core::memory::MemoryScope::Project,
        kimetsu_core::memory::MemoryKind::Fact,
        "The schema is at version 11",
    )
    .expect("add fact");

    // A query with no lexical overlap with the preference at all.
    let out = run_context_hook(
        &root,
        &cache_home,
        &["--warm-on-first-prompt"],
        r#"{"session_id":"prefs-1","prompt":"rename the widget module to gadget"}"#,
    );
    let context = serde_json::from_str::<serde_json::Value>(&out)
        .ok()
        .and_then(|v| {
            v["hookSpecificOutput"]["additionalContext"]
                .as_str()
                .map(str::to_string)
        })
        .unwrap_or_default();

    assert!(
        context.contains("How you like to work"),
        "the preferences section must be present; got: {context}"
    );
    assert!(
        context.contains("thiserror"),
        "a preference must arrive even when the query never mentions it; got: {context}"
    );
    assert!(
        context.contains("follow these without being asked"),
        "preferences are instructions, not retrieved facts; got: {context}"
    );

    // …and must not also be duplicated into the repo digest above it.
    let digest_section = context
        .split("## How you like to work")
        .next()
        .unwrap_or_default();
    assert!(
        !digest_section.contains("thiserror"),
        "the digest must not repeat what the preferences section carries; got: {digest_section}"
    );

    let _ = fs::remove_dir_all(&root);
}
