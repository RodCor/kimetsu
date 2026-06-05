//! Router assembly (kept separate so tests can build the app in-process).

use axum::Router;
use axum::routing::{get, post};

use crate::rpc::handle_mcp;
use crate::state::AppState;

async fn healthz() -> &'static str {
    "ok"
}

/// Build the axum app: an unauthenticated health probe and the authenticated
/// per-repo MCP endpoint.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
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
        let auth = AuthConfig {
            global: vec!["tok_admin".to_string()],
            per_repo,
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
