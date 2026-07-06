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
