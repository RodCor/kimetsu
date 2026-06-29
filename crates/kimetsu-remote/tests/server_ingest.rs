//! Server-side ingest end-to-end: register a (local) git repo, ingest it via
//! the MCP tool, and confirm a file capsule is retrievable through `context`.
//! Hermetic — clones a temp local git repo, NoopEmbedder (FTS recall).

use std::collections::HashMap;
use std::process::Command;
use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use kimetsu_remote::app::build_router;
use kimetsu_remote::auth::AuthConfig;
use kimetsu_remote::ingest::{IngestState, RepoSpec};
use kimetsu_remote::state::AppState;
use serde_json::{Value, json};
use tower::ServiceExt;

fn git(args: &[&str]) {
    let out = Command::new("git").args(args).output().expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn state(data: &std::path::Path, ingest: IngestState) -> AppState {
    let auth = AuthConfig {
        global: vec!["T".to_string()],
        per_repo: Default::default(),
        ..Default::default()
    };
    AppState::new(data.to_path_buf(), auth).with_ingest(Arc::new(ingest))
}

async fn send(app: axum::Router, repo: &str, body: Value) -> Value {
    let req = Request::builder()
        .method("POST")
        .uri(format!("/mcp/{repo}"))
        .header("authorization", "Bearer T")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn inner(resp: &Value) -> Value {
    let text = resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("no result text in {resp}"));
    serde_json::from_str(text).unwrap()
}

#[tokio::test]
async fn registered_repo_ingests_and_files_are_retrievable() {
    // SAFETY: single test in this binary; set before any brain access.
    unsafe {
        std::env::set_var("KIMETSU_USER_BRAIN", "0");
        std::env::set_var("KIMETSU_BRAIN_EMBEDDER", "noop");
    }
    kimetsu_core::paths::pin_discover_to_root();

    // A throwaway "remote" git repo with a distinctively-worded file.
    let remote = tempfile::tempdir().unwrap();
    let rp = remote.path().to_str().unwrap();
    git(&["init", "-q", rp]);
    std::fs::write(
        remote.path().join("README.md"),
        "the snorblax module calibrates the frobnitz manifold before launch",
    )
    .unwrap();
    git(&["-C", rp, "add", "-A"]);
    git(&[
        "-C",
        rp,
        "-c",
        "user.email=t@example.com",
        "-c",
        "user.name=t",
        "commit",
        "-q",
        "-m",
        "init",
    ]);

    let data = tempfile::tempdir().unwrap();
    let checkout = tempfile::tempdir().unwrap();
    let mut repos = HashMap::new();
    repos.insert(
        "demo".to_string(),
        RepoSpec {
            url: rp.to_string(),
            branch: None,
        },
    );
    let ingest = IngestState::new(checkout.path().to_path_buf(), repos);
    let app = build_router(state(data.path(), ingest));

    // tools/list advertises ingest when enabled.
    let list = send(
        app.clone(),
        "demo",
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
    )
    .await;
    let names: Vec<String> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert!(names.contains(&"kimetsu_brain_ingest_repo".to_string()));

    // Ingest the registered repo.
    let ing = inner(
        &send(
            app.clone(),
            "demo",
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
                   "params":{"name":"kimetsu_brain_ingest_repo","arguments":{}}}),
        )
        .await,
    );
    assert_eq!(ing["ingested"], json!(true), "ingest result: {ing}");
    assert!(
        ing["indexed_files"].as_u64().unwrap_or(0) >= 1,
        "expected ≥1 indexed file: {ing}"
    );

    // The file content is now retrievable via context (FTS over the snippet).
    let ctx = inner(
        &send(
            app.clone(),
            "demo",
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call",
                   "params":{"name":"kimetsu_brain_context",
                             "arguments":{"query":"snorblax frobnitz manifold","min_score":0.0}}}),
        )
        .await,
    );
    assert!(
        ctx.to_string().contains("snorblax"),
        "context did not surface the ingested file: {ctx}"
    );

    // An unregistered repo cannot be ingested → JSON-RPC error.
    let resp = send(
        app,
        "not-registered",
        json!({"jsonrpc":"2.0","id":4,"method":"tools/call",
               "params":{"name":"kimetsu_brain_ingest_repo","arguments":{}}}),
    )
    .await;
    let msg = resp["error"]["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("not registered"),
        "expected not-registered error, got: {resp}"
    );
}
