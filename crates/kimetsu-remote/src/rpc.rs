//! JSON-RPC request handling over HTTP — the `POST /mcp/{repo}` endpoint.
//! A minimal compliant Streamable-HTTP subset: request/response JSON only (no
//! SSE stream, no session store; `Mcp-Session-Id` is echoed if present).

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::auth::{self, AuthOutcome};
use crate::catalog::REMOTE_TOOLS;
use crate::metrics::Outcome;
use crate::repo;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct JsonRpcRequest {
    #[serde(default)]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

fn session_header(headers: &HeaderMap) -> Option<HeaderValue> {
    headers.get("mcp-session-id").cloned()
}

fn with_session(mut resp: Response, session: Option<HeaderValue>) -> Response {
    if let Some(sid) = session {
        resp.headers_mut()
            .insert(HeaderName::from_static("mcp-session-id"), sid);
    }
    resp
}

fn http_error(status: StatusCode, message: &str, session: Option<HeaderValue>) -> Response {
    with_session(
        (status, Json(json!({ "error": message }))).into_response(),
        session,
    )
}

fn jsonrpc(status: StatusCode, body: Value, session: Option<HeaderValue>) -> Response {
    with_session((status, Json(body)).into_response(), session)
}

fn jsonrpc_ok(id: Value, result: Value, session: Option<HeaderValue>) -> Response {
    jsonrpc(
        StatusCode::OK,
        json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        session,
    )
}

fn jsonrpc_err(
    status: StatusCode,
    id: Value,
    code: i64,
    message: &str,
    session: Option<HeaderValue>,
) -> Response {
    jsonrpc(
        status,
        json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }),
        session,
    )
}

/// `POST /mcp/{repo}` — outer wrapper: time the request, record the outcome
/// metric, and emit a structured per-request log line.
pub async fn handle_mcp(
    State(state): State<AppState>,
    Path(repo): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let start = std::time::Instant::now();
    let (outcome, resp) = dispatch_request(&state, &repo, &headers, body).await;
    state.metrics.record(outcome);
    tracing::info!(
        repo = %repo,
        status = resp.status().as_u16(),
        outcome = outcome.as_str(),
        latency_ms = start.elapsed().as_millis() as u64,
        "mcp request"
    );
    resp
}

/// Authenticate, rate-limit, resolve the repo's brain, and dispatch the
/// JSON-RPC method (filtered to the remote tool allowlist). Returns the outcome
/// label alongside the response so the wrapper can record metrics.
async fn dispatch_request(
    state: &AppState,
    repo: &str,
    headers: &HeaderMap,
    body: Bytes,
) -> (Outcome, Response) {
    let session = session_header(headers);

    // 1. Auth (transport-level → real HTTP status).
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    match auth::check(&state.auth, repo, bearer) {
        AuthOutcome::Ok => {}
        AuthOutcome::Unauthorized => {
            return (
                Outcome::Unauthorized,
                http_error(StatusCode::UNAUTHORIZED, "unauthorized", session),
            );
        }
        AuthOutcome::Forbidden => {
            return (
                Outcome::Forbidden,
                http_error(StatusCode::FORBIDDEN, "forbidden for this repo", session),
            );
        }
    }

    // 2. Per-token rate limit.
    if let Some(tok) = bearer
        && !state.limiter.allow(tok)
    {
        return (
            Outcome::RateLimited,
            http_error(StatusCode::TOO_MANY_REQUESTS, "rate limited", session),
        );
    }

    // 3. Resolve the brain root (path-traversal safe).
    let root = match repo::resolve_brain_root(&state.data_dir, repo) {
        Ok(r) => r,
        Err(e) => {
            return (
                Outcome::BadRequest,
                http_error(StatusCode::BAD_REQUEST, &e, session),
            );
        }
    };

    // 4. Parse the JSON-RPC envelope.
    let req: JsonRpcRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return (
                Outcome::BadRequest,
                jsonrpc_err(
                    StatusCode::BAD_REQUEST,
                    Value::Null,
                    -32700,
                    &format!("parse error: {e}"),
                    session,
                ),
            );
        }
    };

    // 5. Notifications (no id) get no response body.
    let Some(id) = req.id.clone() else {
        return (
            Outcome::Ok,
            with_session(StatusCode::ACCEPTED.into_response(), session),
        );
    };

    // 6. Ensure the brain exists (first-use init).
    if let Err(e) = repo::ensure_initialized(&root) {
        return (
            Outcome::Error,
            jsonrpc_err(StatusCode::INTERNAL_SERVER_ERROR, id, -32603, &e, session),
        );
    }

    // 7. Run the (blocking) dispatch off the async pool.
    let skills = state.skills.clone();
    let method = req.method.clone();
    let params = req.params.clone();
    let res = tokio::task::spawn_blocking(move || {
        kimetsu_chat::dispatch(&method, params, &root, skills.as_ref(), Some(&REMOTE_TOOLS))
    })
    .await;

    match res {
        Ok(Ok(value)) => (Outcome::Ok, jsonrpc_ok(id, value, session)),
        // App-level tool errors ride the body with HTTP 200 — the request was
        // served, so it counts as `ok` for metrics.
        Ok(Err(msg)) => (
            Outcome::Ok,
            jsonrpc_err(StatusCode::OK, id, -32000, &msg, session),
        ),
        Err(join) => (
            Outcome::Error,
            jsonrpc_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                id,
                -32603,
                &format!("internal error: {join}"),
                session,
            ),
        ),
    }
}
