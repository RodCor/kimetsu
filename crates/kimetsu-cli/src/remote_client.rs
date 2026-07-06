//! v3.0 #3 Slice C: a tiny HTTP client for writing to a `kimetsu-remote` server
//! from the CLI. Posts a JSON-RPC `tools/call` to `POST <base>/mcp/<repo>` with a
//! bearer token, so a user can write to the shared team brain without going
//! through a host MCP harness. Reads still go through the host or local brain.

use kimetsu_core::KimetsuResult;
use serde_json::{Value, json};

/// Resolve the bearer token: explicit `token` wins, else `KIMETSU_REMOTE_TOKEN`.
pub fn resolve_token(token: Option<&str>) -> KimetsuResult<String> {
    if let Some(t) = token.map(str::trim).filter(|t| !t.is_empty()) {
        return Ok(t.to_string());
    }
    match std::env::var("KIMETSU_REMOTE_TOKEN") {
        Ok(t) if !t.trim().is_empty() => Ok(t.trim().to_string()),
        _ => Err("no remote token: pass --token or set KIMETSU_REMOTE_TOKEN".into()),
    }
}

/// POST a JSON-RPC `tools/call` to the remote server and return the tool's
/// `result` value. JSON-RPC tool errors and transport errors both map to `Err`.
pub fn remote_call(
    base_url: &str,
    repo: &str,
    token: &str,
    tool: &str,
    arguments: Value,
) -> KimetsuResult<Value> {
    let url = format!("{}/mcp/{}", base_url.trim_end_matches('/'), repo);
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": tool, "arguments": arguments },
    });
    let client = reqwest::blocking::Client::new();
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .map_err(|e| format!("remote call to {url}: {e}"))?;

    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(format!("remote rejected the token (401) for {url}").into());
    }
    if status == reqwest::StatusCode::FORBIDDEN {
        return Err(format!("token not granted for repo {repo:?} (403)").into());
    }
    let v: Value = resp
        .json()
        .map_err(|e| format!("remote returned non-JSON (HTTP {status}): {e}"))?;
    if let Some(err) = v.get("error") {
        let msg = err
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| err.to_string());
        return Err(format!("remote tool error: {msg}").into());
    }
    Ok(v.get("result").cloned().unwrap_or(Value::Null))
}

/// Render a JSON-RPC `result` (the `{content:[{type:text,text}]}` shape MCP tools
/// return) as a single human-readable string for CLI output.
pub fn render_result(result: &Value) -> String {
    if let Some(items) = result.get("content").and_then(Value::as_array) {
        let text: Vec<String> = items
            .iter()
            .filter_map(|c| c.get("text").and_then(Value::as_str))
            .map(str::to_string)
            .collect();
        if !text.is_empty() {
            return text.join("\n");
        }
    }
    result.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_token_prefers_explicit() {
        assert_eq!(resolve_token(Some("  abc ")).unwrap(), "abc");
    }

    #[test]
    fn render_result_extracts_text_content() {
        let r = json!({"content":[{"type":"text","text":"added memory 01K…"}]});
        assert_eq!(render_result(&r), "added memory 01K…");
        // Falls back to raw JSON when no text content.
        let r2 = json!({"ok": true});
        assert!(render_result(&r2).contains("ok"));
    }
}
