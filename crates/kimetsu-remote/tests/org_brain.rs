//! Shared/org brain: a `global_user`-scoped memory recorded against one repo is
//! visible in another repo's retrieval (via the shared brain), while a
//! `project`-scoped memory stays local to its repo.
//!
//! Runs in its own test binary because it sets `KIMETSU_USER_BRAIN=1` +
//! `KIMETSU_USER_BRAIN_DIR` (the standalone round-trip test sets `=0`).

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use kimetsu_remote::app::build_router;
use kimetsu_remote::auth::AuthConfig;
use kimetsu_remote::state::AppState;
use serde_json::{Value, json};
use tower::ServiceExt;

fn state(dir: &std::path::Path) -> AppState {
    let auth = AuthConfig {
        global: vec!["T".to_string()],
        per_repo: Default::default(),
    };
    AppState::new(dir.to_path_buf(), auth)
}

async fn send(dir: &std::path::Path, repo: &str, body: Value) -> Value {
    let req = Request::builder()
        .method("POST")
        .uri(format!("/mcp/{repo}"))
        .header("authorization", "Bearer T")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = build_router(state(dir)).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "expected 200 for {repo}");
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn add(scope: &str, text: &str) -> Value {
    json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
           "params":{"name":"kimetsu_brain_memory_add",
                     "arguments":{"scope":scope,"kind":"fact","text":text}}})
}

fn context(query: &str) -> Value {
    json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
           "params":{"name":"kimetsu_brain_context","arguments":{"query":query,"min_score":0.0}}})
}

fn inner(resp: &Value) -> Value {
    let text = resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("no result text in {resp}"));
    serde_json::from_str(text).unwrap()
}

#[tokio::test]
async fn org_brain_shares_global_user_but_not_project() {
    // Org brain lives outside the per-repo data dir.
    let org = std::env::temp_dir().join(format!("kimetsu_org_test_{}", std::process::id()));
    std::fs::create_dir_all(&org).unwrap();
    // SAFETY: this test binary runs a single test; set before any brain access.
    unsafe {
        std::env::set_var("KIMETSU_USER_BRAIN", "1");
        std::env::set_var("KIMETSU_USER_BRAIN_DIR", &org);
        std::env::set_var("KIMETSU_BRAIN_EMBEDDER", "noop");
        std::env::set_var("KIMETSU_MCP_ENABLE_WRITE_TOOLS", "1");
    }
    kimetsu_core::paths::pin_discover_to_root();

    let data = tempfile::tempdir().unwrap();

    // repo-a records: one shared (global_user), one local (project).
    let shared = inner(
        &send(
            data.path(),
            "repo-a",
            add("global_user", "orgwide flumbex policy applies everywhere"),
        )
        .await,
    );
    assert!(
        shared["memory_id"].is_string(),
        "global add failed: {shared}"
    );
    let local = inner(
        &send(
            data.path(),
            "repo-a",
            add("project", "projlocal zonquar only here"),
        )
        .await,
    );
    assert!(
        local["memory_id"].is_string(),
        "project add failed: {local}"
    );

    // repo-b sees the shared memory (merged from the org brain)...
    let shared_ctx = inner(&send(data.path(), "repo-b", context("flumbex orgwide")).await);
    assert!(
        shared_ctx["capsules"].to_string().contains("flumbex"),
        "repo-b should see the org-wide memory: {shared_ctx}"
    );

    // ...but NOT repo-a's project-scoped memory.
    let local_ctx = inner(&send(data.path(), "repo-b", context("zonquar projlocal")).await);
    assert!(
        !local_ctx["capsules"].to_string().contains("zonquar"),
        "repo-b must NOT see repo-a's project-scoped memory: {local_ctx}"
    );

    std::fs::remove_dir_all(&org).ok();
}
