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
use crate::ingest::IngestState;
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

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?.trim();
    let mut parts = value.splitn(2, char::is_whitespace);
    let scheme = parts.next()?;
    let token = parts.next()?.trim();
    if scheme.eq_ignore_ascii_case("bearer") && !token.is_empty() {
        Some(token)
    } else {
        None
    }
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
    let repo = match repo::sanitize_repo_id(repo) {
        Ok(id) => id,
        Err(e) => {
            return (
                Outcome::BadRequest,
                http_error(StatusCode::BAD_REQUEST, &e, session),
            );
        }
    };

    // 1. Auth (transport-level → real HTTP status).
    let bearer = bearer_token(headers);
    match auth::check(&state.auth, &repo, bearer) {
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
    let root = match repo::resolve_brain_root(&state.data_dir, &repo) {
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

    if requires_global_memory_grant(&req.method, &req.params)
        && !auth::is_global_token(&state.auth, bearer)
    {
        return (
            Outcome::Forbidden,
            jsonrpc_err(
                StatusCode::FORBIDDEN,
                id,
                -32000,
                "shared org/user memory writes require a global token",
                session,
            ),
        );
    }

    // 6. Ensure the brain exists (first-use init).
    if let Err(e) = repo::ensure_initialized(&root) {
        return (
            Outcome::Error,
            jsonrpc_err(StatusCode::INTERNAL_SERVER_ERROR, id, -32603, &e, session),
        );
    }

    // 6b. Server-side ingest is intercepted here — it clones/refreshes the
    // managed checkout and indexes THOSE files into the brain, which the normal
    // dispatch can't do (it would walk the brain dir, not a checkout).
    if let Some(ingest) = &state.ingest
        && req.method == "tools/call"
        && req.params.get("name").and_then(|n| n.as_str()) == Some("kimetsu_brain_ingest_repo")
    {
        return handle_server_ingest(ingest, &repo, &root, id, session).await;
    }

    // 6c. `kimetsu_brain_context` with a server-side reranker: intercept before
    // generic dispatch so we can inject the reranker into the tool body.
    // The allowlist + auth + rate-limit checks above have already run.
    if req.method == "tools/call"
        && req.params.get("name").and_then(|n| n.as_str()) == Some("kimetsu_brain_context")
        && state.reranker.is_some()
    {
        let reranker = state.reranker.clone().expect("checked above");
        let arguments = req
            .params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        let res = tokio::task::spawn_blocking(move || {
            kimetsu_chat::brain_context_tool(&root, &arguments, Some(reranker.as_ref()))
        })
        .await;

        return match res {
            Ok(Ok(value)) => {
                // Wrap in the same `{content:[{type,text}]}` envelope that
                // generic dispatch produces for tools/call results.
                let text =
                    serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
                (
                    Outcome::Ok,
                    jsonrpc_ok(
                        id,
                        serde_json::json!({
                            "content": [{ "type": "text", "text": text }]
                        }),
                        session,
                    ),
                )
            }
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
        };
    }

    // 7. Run the (blocking) dispatch off the async pool.
    let allow = crate::catalog::allowlist(state.ingest.is_some());
    let skills = state.skills.clone();
    let method = req.method.clone();
    let params = req.params.clone();
    // Slice C: attribute this request's writes to the authenticated user. Each
    // request runs on ONE blocking thread, so an OriginScope set inside the
    // closure is visible to Event::new during the write and cleared on return
    // (tokio reuses blocking threads, so the RAII restore is essential).
    let origin = format!(
        "{}/user:{}",
        state.server_node,
        crate::auth::user_for_token(&state.auth, bearer)
    );
    let tool_name = tool_name_of(&req.method, &req.params);
    let res = tokio::task::spawn_blocking(move || {
        let _scope = kimetsu_core::event::OriginScope::new(origin);
        kimetsu_chat::dispatch(&method, params, &root, skills.as_ref(), Some(allow))
    })
    .await;

    match res {
        Ok(Ok(value)) => {
            // Slice C: count successful memory-write tool calls (aggregate).
            if let Some(name) = &tool_name {
                if crate::metrics::is_write_tool(name) {
                    state.metrics.record_write();
                }
            }
            (Outcome::Ok, jsonrpc_ok(id, value, session))
        }
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

/// The tool name of a `tools/call` request, if any (Slice C: used to count
/// successful writes).
fn tool_name_of(method: &str, params: &Value) -> Option<String> {
    if method != "tools/call" {
        return None;
    }
    params
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn requires_global_memory_grant(method: &str, params: &Value) -> bool {
    if method != "tools/call" {
        return false;
    }
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return false;
    };
    let arguments = params.get("arguments").unwrap_or(&Value::Null);
    match name {
        "kimetsu_benchmark_record_outcome" => true,
        "kimetsu_brain_memory_add" => argument_scope_is_global_user(arguments),
        "kimetsu_brain_memory_accept" => argument_scope_is_global_user(arguments),
        _ => false,
    }
}

fn argument_scope_is_global_user(arguments: &Value) -> bool {
    arguments
        .get("scope")
        .and_then(Value::as_str)
        .map(|scope| {
            matches!(
                scope.trim().to_ascii_lowercase().as_str(),
                "global_user" | "user"
            )
        })
        .unwrap_or(false)
}

/// Clone/refresh the repo's managed checkout and ingest its files into the
/// repo's brain. Only repos registered in `--repos-file` are ingestable.
async fn handle_server_ingest(
    ingest: &IngestState,
    repo: &str,
    brain_root: &std::path::Path,
    id: Value,
    session: Option<HeaderValue>,
) -> (Outcome, Response) {
    let Some(spec) = ingest.repos.get(repo) else {
        return (
            Outcome::Ok,
            jsonrpc_err(
                StatusCode::OK,
                id,
                -32000,
                &format!(
                    "repo `{repo}` is not registered for server-side ingest (add it to --repos-file)"
                ),
                session,
            ),
        );
    };

    // Serialize ingests so concurrent calls don't race on a checkout.
    let _guard = ingest.lock.lock().await;
    let checkout_dir = ingest.checkout_dir.clone();
    let repo_id = repo.to_string();
    let url = spec.url.clone();
    let branch = spec.branch.clone();
    let brain_root = brain_root.to_path_buf();

    let res = tokio::task::spawn_blocking(move || {
        let files_root =
            crate::git::ensure_checkout(&checkout_dir, &repo_id, &url, branch.as_deref())?;
        kimetsu_brain::project::ingest_repo_at_root(&brain_root, &files_root)
            .map_err(|e| e.to_string())
    })
    .await;

    match res {
        Ok(Ok(summary)) => {
            let text = serde_json::to_string_pretty(&json!({
                "ingested": true,
                "indexed_files": summary.indexed_files,
                "skipped_files": summary.skipped_files,
                "manifests": summary.manifests,
            }))
            .unwrap_or_default();
            (
                Outcome::Ok,
                jsonrpc_ok(
                    id,
                    json!({ "content": [{ "type": "text", "text": text }] }),
                    session,
                ),
            )
        }
        Ok(Err(e)) => (
            Outcome::Error,
            jsonrpc_err(
                StatusCode::OK,
                id,
                -32000,
                &format!("ingest failed: {e}"),
                session,
            ),
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
