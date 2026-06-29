//! Router assembly (kept separate so tests can build the app in-process).

use axum::Router;
use axum::extract::State;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};

use crate::rpc::handle_mcp;
use crate::state::AppState;

async fn healthz() -> &'static str {
    "ok"
}

/// Aggregate request counters in Prometheus text format. Unauthenticated (no
/// secrets, no repo labels) — keep it on a private network or scrape via proxy.
async fn metrics(State(state): State<AppState>) -> Response {
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        state.metrics.render_prometheus(),
    )
        .into_response()
}

/// Build the axum app: unauthenticated health + metrics probes and the
/// authenticated per-repo MCP endpoint.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/metrics", get(metrics))
        .route("/mcp/:repo", post(handle_mcp))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthConfig;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use serde_json::{Value, json};
    use std::collections::HashMap;
    use tower::ServiceExt; // oneshot

    fn state_with(dir: &std::path::Path) -> AppState {
        let mut per_repo = HashMap::new();
        per_repo.insert("web".to_string(), vec!["tok_web".to_string()]);
        let mut token_names = HashMap::new();
        token_names.insert("tok_web".to_string(), "webuser".to_string());
        let auth = AuthConfig {
            global: vec!["tok_admin".to_string()],
            per_repo,
            token_names,
        };
        AppState::new(dir.to_path_buf(), auth)
    }

    async fn body_json(resp: axum::response::Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        }
    }

    fn post(repo: &str, token: Option<&str>, body: Value) -> Request<Body> {
        let mut b = Request::builder()
            .method("POST")
            .uri(format!("/mcp/{repo}"))
            .header("content-type", "application/json");
        if let Some(t) = token {
            b = b.header("authorization", format!("Bearer {t}"));
        }
        b.body(Body::from(body.to_string())).unwrap()
    }

    #[tokio::test]
    async fn healthz_needs_no_auth() {
        let tmp = tempfile::tempdir().unwrap();
        let app = build_router(state_with(tmp.path()));
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn missing_token_is_401() {
        let tmp = tempfile::tempdir().unwrap();
        let app = build_router(state_with(tmp.path()));
        let resp = app
            .oneshot(post(
                "web",
                None,
                json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn bearer_scheme_is_case_insensitive() {
        let tmp = tempfile::tempdir().unwrap();
        let app = build_router(state_with(tmp.path()));
        let req = Request::builder()
            .method("POST")
            .uri("/mcp/web")
            .header("content-type", "application/json")
            .header("authorization", "bearer   tok_admin")
            .body(Body::from(
                json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}).to_string(),
            ))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn per_repo_token_wrong_repo_is_403() {
        let tmp = tempfile::tempdir().unwrap();
        let app = build_router(state_with(tmp.path()));
        // tok_web is only valid for repo "web"; use it on "api".
        let resp = app
            .oneshot(post(
                "api",
                Some("tok_web"),
                json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn tools_list_filtered_to_remote_catalog() {
        let tmp = tempfile::tempdir().unwrap();
        let app = build_router(state_with(tmp.path()));
        let resp = app
            .oneshot(post(
                "web",
                Some("tok_admin"),
                json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        let names: Vec<String> = v["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert!(names.contains(&"kimetsu_brain_record".to_string()));
        assert!(
            !names.contains(&"kimetsu_brain_ingest_repo".to_string()),
            "workdir tool must be excluded"
        );
        assert!(
            !names.contains(&"kimetsu_plugin_install".to_string()),
            "host-local tool must be excluded"
        );
    }

    #[tokio::test]
    async fn excluded_tool_call_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let app = build_router(state_with(tmp.path()));
        let resp = app
            .oneshot(post(
                "web",
                Some("tok_admin"),
                json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
                       "params":{"name":"kimetsu_brain_ingest_repo","arguments":{}}}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        let msg = v["error"]["message"].as_str().unwrap_or_default();
        assert!(msg.contains("not available in remote mode"), "got: {v}");
    }

    #[tokio::test]
    async fn per_repo_token_cannot_write_shared_user_memory() {
        let tmp = tempfile::tempdir().unwrap();
        let app = build_router(state_with(tmp.path()));
        let resp = app
            .oneshot(post(
                "web",
                Some("tok_web"),
                json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
                "params":{"name":"kimetsu_brain_memory_add","arguments":{
                    "scope":"global_user",
                    "text":"shared memory should require admin token"
                }}}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let v = body_json(resp).await;
        let msg = v["error"]["message"].as_str().unwrap_or_default();
        assert!(msg.contains("shared org/user memory writes require a global token"));
    }

    #[tokio::test]
    async fn remote_write_is_attributed_to_the_token_user() {
        // Slice C: a write through the remote server stamps the event origin with
        // `<server_node>/user:<name>` resolved from the bearer token.
        // Remote writes are operator-gated behind this env (set by `serve`).
        // SAFETY: tests run single-threaded; no other thread reads env concurrently.
        unsafe { std::env::set_var("KIMETSU_MCP_ENABLE_WRITE_TOOLS", "1") };
        let tmp = tempfile::tempdir().unwrap();
        let app = build_router(state_with(tmp.path())); // server_node defaults to "remote"
        let resp = app
            .oneshot(post(
                "web",
                Some("tok_web"),
                json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                "params":{"name":"kimetsu_brain_memory_add","arguments":{
                    "scope":"project","kind":"fact","text":"attributed remote write"
                }}}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert!(v.get("error").is_none(), "write should succeed: {v}");

        // The persisted event carries the per-user origin.
        let db = tmp.path().join("web").join(".kimetsu").join("brain.db");
        let conn = rusqlite::Connection::open(&db).expect("open repo brain");
        let origin: Option<String> = conn
            .query_row(
                "SELECT origin FROM events WHERE kind='memory.accepted' ORDER BY rowid DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .expect("read accepted event origin");
        assert_eq!(
            origin.as_deref(),
            Some("remote/user:webuser"),
            "remote write must be attributed to the token's user"
        );
    }

    #[tokio::test]
    async fn rate_limit_returns_429() {
        let tmp = tempfile::tempdir().unwrap();
        let auth = AuthConfig {
            global: vec!["tok_admin".to_string()],
            per_repo: HashMap::new(),
            ..Default::default()
        };
        let app = build_router(AppState::with_rate_limit(tmp.path().to_path_buf(), auth, 1));
        let body = json!({"jsonrpc":"2.0","id":1,"method":"tools/list"});
        let r1 = app
            .clone()
            .oneshot(post("web", Some("tok_admin"), body.clone()))
            .await
            .unwrap();
        assert_eq!(r1.status(), StatusCode::OK);
        let r2 = app
            .oneshot(post("web", Some("tok_admin"), body))
            .await
            .unwrap();
        assert_eq!(r2.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn metrics_endpoint_counts_outcomes() {
        let tmp = tempfile::tempdir().unwrap();
        let app = build_router(state_with(tmp.path()));
        // One unauthenticated request bumps the `unauthorized` counter.
        let _ = app
            .clone()
            .oneshot(post(
                "web",
                None,
                json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
            ))
            .await
            .unwrap();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            text.contains("kimetsu_remote_requests_total{outcome=\"unauthorized\"} 1"),
            "metrics did not count the unauthorized request: {text}"
        );
    }

    #[tokio::test]
    async fn initialize_advertises_protocol() {
        let tmp = tempfile::tempdir().unwrap();
        let app = build_router(state_with(tmp.path()));
        let resp = app
            .oneshot(post(
                "web",
                Some("tok_admin"),
                json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
            ))
            .await
            .unwrap();
        let v = body_json(resp).await;
        assert_eq!(v["result"]["protocolVersion"], "2024-11-05");
    }
}
