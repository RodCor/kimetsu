//! Two processes on loopback: invite, join, share, request, check the
//! signature by importing, then leave. Built only with `--features network`.

#![cfg(feature = "network")]

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kimetsu_brain::project;
use kimetsu_core::memory::{MemoryKind, MountLevel};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_kimetsu")
}

fn scratch(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "kimetsu-loop-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
}

fn init_brain(root: &Path) {
    kimetsu_core::paths::git_init_boundary(root);
    project::init_project_at_root(root, true).unwrap();
}

fn record(root: &Path, text: &str) {
    let mut grants = kimetsu_brain::mount::MountGrants::default();
    grants.allow_write(MountLevel::Repo);
    kimetsu_brain::mount::record(
        root,
        &grants,
        MountLevel::Repo,
        MemoryKind::Fact,
        text,
        0.95,
        None,
    )
    .unwrap();
}

fn active(root: &Path, text: &str) -> i64 {
    let (_, _, conn) = project::load_project_readonly(root).unwrap();
    conn.query_row(
        "SELECT COUNT(*) FROM memories WHERE text = ?1 AND invalidated_at IS NULL",
        [text],
        |row| row.get(0),
    )
    .unwrap()
}

fn run(args: &[&str]) -> std::process::Output {
    Command::new(bin())
        .args(args)
        .env("KIMETSU_USER_BRAIN", "0")
        .output()
        .expect("spawn kimetsu")
}

fn stdout_text(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

#[test]
fn mcp_status_is_quiet_until_a_request_names_a_peer() {
    unsafe { std::env::set_var("KIMETSU_USER_BRAIN", "0") };
    let workspace = scratch("mcp-ws");
    init_brain(&workspace);
    let state = scratch("mcp-state");
    let mut child = Command::new(bin())
        .args(["mcp", "serve", "--workspace", workspace.to_str().unwrap()])
        .env("KIMETSU_USER_BRAIN", "0")
        .env("KIMETSU_NETWORK_DIR", &state)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("mcp serve");
    let mut stdin = child.stdin.take().unwrap();
    let requests = [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"kimetsu_network_status","arguments":{}}}"#,
        r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"kimetsu_network_request","arguments":{}}}"#,
    ];
    for request in requests {
        writeln!(stdin, "{request}").unwrap();
    }
    drop(stdin);
    let output = child.wait_with_output().expect("mcp serve output");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut saw_tools = false;
    let mut saw_status = false;
    let mut saw_refusal = false;
    for line in stdout.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match value.get("id").and_then(|id| id.as_u64()) {
            Some(2) => {
                let tools = value["result"]["tools"].to_string();
                assert!(tools.contains("kimetsu_network_status"), "{tools}");
                assert!(tools.contains("kimetsu_network_request"), "{tools}");
                saw_tools = true;
            }
            Some(3) => {
                let text = value["result"]["content"][0]["text"]
                    .as_str()
                    .expect("status text");
                let body: serde_json::Value = serde_json::from_str(text).expect(text);
                assert_eq!(body["joined"], false, "{body}");
                assert_eq!(body["listening"], false, "{body}");
                saw_status = true;
            }
            Some(4) => {
                let message = value["error"]["message"].as_str().unwrap_or("");
                assert!(
                    message.contains("Nothing was contacted"),
                    "request without a peer must not dial: {message}"
                );
                saw_refusal = true;
            }
            _ => {}
        }
    }
    assert!(
        saw_tools && saw_status && saw_refusal,
        "missing MCP replies. stdout={stdout}\nstderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let _ = fs::remove_dir_all(&workspace);
    let _ = fs::remove_dir_all(&state);
}

#[test]
fn two_nodes_invite_join_share_request_then_leave() {
    unsafe { std::env::set_var("KIMETSU_USER_BRAIN", "0") };
    let lesson = "LOOPBACK_PACK_LESSON signed repo fact for the network test";
    let alice_ws = scratch("alice-ws");
    let bob_ws = scratch("bob-ws");
    let alice_state = scratch("alice-state");
    let bob_state = scratch("bob-state");
    init_brain(&alice_ws);
    init_brain(&bob_ws);
    record(&alice_ws, lesson);

    let invited = run(&[
        "network",
        "invite",
        "--name",
        "loopback",
        "--json",
        "--state-dir",
        alice_state.to_str().unwrap(),
    ]);
    assert!(
        invited.status.success(),
        "invite failed: {}",
        String::from_utf8_lossy(&invited.stderr)
    );
    let invited_json: serde_json::Value =
        serde_json::from_str(&stdout_text(&invited)).expect("invite json");
    let code = invited_json["code"].as_str().expect("code").to_string();
    assert!(code.starts_with("knet1."), "{code}");
    assert!(
        !code.contains(lesson),
        "join code must not carry memory text"
    );
    assert_eq!(invited_json["listening"], false);
    let alice_saved = fs::read_to_string(alice_state.join("state.json")).unwrap();
    let admin_secret =
        serde_json::from_str::<serde_json::Value>(&alice_saved).unwrap()["admin_secret"]
            .as_str()
            .unwrap()
            .to_string();
    assert!(!code.contains(&admin_secret));

    let joined = run(&[
        "network",
        "join",
        &code,
        "--json",
        "--state-dir",
        bob_state.to_str().unwrap(),
    ]);
    assert!(
        joined.status.success(),
        "join failed: {}\n{}",
        String::from_utf8_lossy(&joined.stderr),
        stdout_text(&joined)
    );
    let joined_json: serde_json::Value =
        serde_json::from_str(&stdout_text(&joined)).expect("join json");
    assert_eq!(joined_json["joined"], true);
    assert_eq!(joined_json["listening"], false);
    assert_eq!(active(&bob_ws, lesson), 0, "join must not import a pack");

    let ready = scratch("ready").join("ready.json");
    let logs = Arc::new(Mutex::new(String::new()));
    let mut share = Command::new(bin())
        .args([
            "network",
            "share",
            "--workspace",
            alice_ws.to_str().unwrap(),
            "--state-dir",
            alice_state.to_str().unwrap(),
            "--bind",
            "127.0.0.1:0",
            "--no-mdns",
            "--ready-file",
            ready.to_str().unwrap(),
        ])
        .env("KIMETSU_USER_BRAIN", "0")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("share");
    let share_logs = Arc::clone(&logs);
    let stdout = share.stdout.take().unwrap();
    let stderr = share.stderr.take().unwrap();
    std::thread::spawn(move || {
        let mut buf = String::new();
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            buf.push_str(&line);
            buf.push('\n');
        }
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            buf.push_str(&line);
            buf.push('\n');
        }
        *share_logs.lock().unwrap() = buf;
    });

    let deadline = Instant::now() + Duration::from_secs(60);
    while !ready.exists() && Instant::now() < deadline {
        if let Some(status) = share.try_wait().unwrap() {
            panic!(
                "share exited {status} before the ready file. {}",
                logs.lock().unwrap()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        ready.exists(),
        "share did not publish: {}",
        logs.lock().unwrap()
    );
    let ready_json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&ready).unwrap()).unwrap();
    let peer = ready_json["peer"].as_str().expect("peer").to_string();
    assert_eq!(ready_json["listening"], true);

    let fetched = run(&[
        "network",
        "request",
        "--json",
        "--workspace",
        bob_ws.to_str().unwrap(),
        "--state-dir",
        bob_state.to_str().unwrap(),
        "--bind",
        "127.0.0.1:0",
        "--no-mdns",
        "--peer",
        &peer,
    ]);
    let _ = share.kill();
    let _ = share.wait();
    assert!(
        fetched.status.success(),
        "request failed: {}\nstdout={}\nshare={}",
        String::from_utf8_lossy(&fetched.stderr),
        stdout_text(&fetched),
        logs.lock().unwrap()
    );
    let fetched_json: serde_json::Value =
        serde_json::from_str(&stdout_text(&fetched)).expect("request json");
    assert_eq!(fetched_json["verified"], true, "{fetched_json}");
    assert_eq!(fetched_json["imported"], 1, "{fetched_json}");
    assert_eq!(fetched_json["quarantined"], 0, "{fetched_json}");
    assert_eq!(fetched_json["listening"], false);
    assert_eq!(
        fetched_json["digest"], ready_json["digest"],
        "the imported pack is the one that was announced"
    );
    assert_eq!(active(&bob_ws, lesson), 1);

    for state in [&alice_state, &bob_state] {
        let left = run(&["network", "leave", "--state-dir", state.to_str().unwrap()]);
        assert!(
            left.status.success(),
            "{}",
            String::from_utf8_lossy(&left.stderr)
        );
        let status = run(&[
            "network",
            "status",
            "--json",
            "--state-dir",
            state.to_str().unwrap(),
        ]);
        let body: serde_json::Value = serde_json::from_str(&stdout_text(&status)).unwrap();
        assert_eq!(body["joined"], false, "{body}");
        assert_eq!(body["listening"], false, "{body}");
    }

    let _ = fs::remove_dir_all(&alice_ws);
    let _ = fs::remove_dir_all(&bob_ws);
    let _ = fs::remove_dir_all(&alice_state);
    let _ = fs::remove_dir_all(&bob_state);
}
