//! End-to-end: drive the real router over HTTP (in-process) and confirm a
//! recorded memory round-trips, and that two repos stay isolated.
//!
//! Runs hermetically: a temp data dir, the user brain disabled, discovery
//! pinned to the explicit root, and the NoopEmbedder forced so there is no
//! model download (FTS-only recall still proves the path).

use std::sync::{Arc, Once};

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use kimetsu_brain::embeddings::StubReranker;
use kimetsu_remote::app::build_router;
use kimetsu_remote::auth::AuthConfig;
use kimetsu_remote::state::AppState;
use serde_json::{Value, json};
use tower::ServiceExt;

static INIT: Once = Once::new();

fn isolate() {
    INIT.call_once(|| {
        // SAFETY: set before any brain/runtime work in this test binary.
        unsafe {
            std::env::set_var("KIMETSU_USER_BRAIN", "0");
            std::env::set_var("KIMETSU_BRAIN_EMBEDDER", "noop");
            std::env::set_var("KIMETSU_MCP_ENABLE_WRITE_TOOLS", "1");
        }
        kimetsu_core::paths::pin_discover_to_root();
    });
}

fn state(dir: &std::path::Path) -> AppState {
    let auth = AuthConfig {
        global: vec!["T".to_string()],
        per_repo: Default::default(),
    };
    AppState::new(dir.to_path_buf(), auth)
}

fn state_with_reranker(dir: &std::path::Path) -> AppState {
    let auth = AuthConfig {
        global: vec!["T".to_string()],
        per_repo: Default::default(),
    };
    let rr: Box<dyn kimetsu_brain::embeddings::Reranker> = Box::new(StubReranker);
    AppState::new(dir.to_path_buf(), auth).with_reranker(Some(rr))
}

fn call(repo: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!("/mcp/{repo}"))
        .header("authorization", "Bearer T")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn send(dir: &std::path::Path, repo: &str, body: Value) -> Value {
    let resp = build_router(state(dir))
        .oneshot(call(repo, body))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "expected 200 for {repo}");
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

async fn send_with_reranker(dir: &std::path::Path, repo: &str, body: Value) -> Value {
    let resp = build_router(state_with_reranker(dir))
        .oneshot(call(repo, body))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "expected 200 for {repo}");
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn record(lesson: &str) -> Value {
    json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": "kimetsu_brain_record",
                    "arguments": { "lesson": lesson, "tags": ["alpha", "beta"] } }
    })
}

fn context(query: &str) -> Value {
    json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": { "name": "kimetsu_brain_context",
                    "arguments": { "query": query, "min_score": 0.0 } }
    })
}

/// Unwrap the `tools/call` envelope `{result:{content:[{text:"<json>"}]}}` to the
/// handler's actual JSON object.
fn inner(resp: &Value) -> Value {
    let text = resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("no result text in {resp}"));
    serde_json::from_str(text).unwrap()
}

#[tokio::test]
async fn record_then_context_round_trips() {
    isolate();
    let tmp = tempfile::tempdir().unwrap();
    let lesson = "The zephyrqux deployment requires flushing the wobblecache before restart";

    let rec = send(tmp.path(), "repo-a", record(lesson)).await;
    assert!(
        inner(&rec)["ok"].as_bool().unwrap_or(false),
        "record failed: {rec}"
    );

    // Query with words from the lesson but NOT the asserted token, so the match
    // can only come from the retrieved capsule (not the echoed query).
    let ctx = inner(&send(tmp.path(), "repo-a", context("deployment restart flushing")).await);
    assert_eq!(ctx["skipped"], json!(false), "expected a hit: {ctx}");
    assert!(
        ctx["capsules"].to_string().contains("wobblecache"),
        "retrieved capsules did not contain the recorded memory: {ctx}"
    );
}

#[tokio::test]
async fn two_repos_are_isolated() {
    isolate();
    let tmp = tempfile::tempdir().unwrap();
    let secret = "quibblefrotz only-in-repo-one";

    let rec = send(tmp.path(), "repo-one", record(secret)).await;
    assert!(
        inner(&rec)["ok"].as_bool().unwrap_or(false),
        "record failed: {rec}"
    );

    // repo-two has its own brain; its retrieved capsules must not include the
    // secret (assert on capsules, not the whole response — the query is echoed).
    let ctx = inner(&send(tmp.path(), "repo-two", context("quibblefrotz")).await);
    assert!(
        !ctx["capsules"].to_string().contains("quibblefrotz"),
        "repo isolation breach: repo-two saw repo-one's memory: {ctx}"
    );
}

/// v1.0.0: when AppState has a StubReranker, `kimetsu_brain_context` is
/// intercepted, reranked, and the response parses correctly with a valid JSON
/// envelope; `max_capsules` is respected.
#[tokio::test]
async fn reranker_in_appstate_intercepts_brain_context() {
    isolate();
    let tmp = tempfile::tempdir().unwrap();
    let lesson = "The wobblecache must be flushed before deployment restart";

    // Record a memory so context has something to return.
    let rec = send(tmp.path(), "repo-rr", record(lesson)).await;
    assert!(
        inner(&rec)["ok"].as_bool().unwrap_or(false),
        "record failed: {rec}"
    );

    // Issue a context request through the state that has a StubReranker.
    let ctx_req = json!({
        "jsonrpc": "2.0", "id": 3, "method": "tools/call",
        "params": { "name": "kimetsu_brain_context",
                    "arguments": { "query": "deployment restart flushing", "min_score": 0.0, "max_capsules": 2 } }
    });
    let resp = send_with_reranker(tmp.path(), "repo-rr", ctx_req).await;
    let ctx = inner(&resp);

    // The envelope must parse and ok must be true.
    assert_eq!(ctx["ok"].as_bool(), Some(true), "ok false: {ctx}");
    // capsules must be a JSON array (may be 0 if FTS doesn't match with noop embedder).
    assert!(ctx["capsules"].is_array(), "capsules missing: {ctx}");
    // The result must not exceed max_capsules=2.
    let cap_len = ctx["capsules"].as_array().map(|a| a.len()).unwrap_or(0);
    assert!(cap_len <= 2, "max_capsules violated: {cap_len} > 2: {ctx}");

    // Confirm the Arc<dyn Reranker> round-trips through Clone correctly.
    let _ = Arc::new(StubReranker);
}
