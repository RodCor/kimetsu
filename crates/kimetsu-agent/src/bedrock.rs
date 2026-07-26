//! AWS Bedrock provider — Anthropic models on Bedrock via the InvokeModel API.
//!
//! Wire format: Anthropic's messages API, body-encoded (`anthropic_version:
//! "bedrock-2023-05-31"`).  Auth: AWS SigV4 signed with env-var credentials
//! (no aws-sdk, no tokio, fits the blocking pipeline).
//!
//! Region resolution precedence (mirrors AWS SDK convention):
//!   1. `config.model.region` literal (project.toml)
//!   2. env-var named by `config.model.region_env` (default `AWS_REGION`)
//!   3. `AWS_DEFAULT_REGION` env-var
//!
//! `_key_override` is a NO-OP for Bedrock (the AWS credentials come from the
//! three dedicated env-vars, not a single "API key"). The parameter is kept
//! so `from_config_with_key` has the same signature shape as the other
//! providers.

use std::path::Path;
use std::time::{Duration, SystemTime};

use aws_credential_types::Credentials;
use aws_sigv4::http_request::{SignableBody, SignableRequest, SigningSettings, sign};
use aws_sigv4::sign::v4;
use kimetsu_core::KimetsuResult;
use kimetsu_core::config::ProjectConfig;
use kimetsu_core::env_file::resolve_env_value;
use kimetsu_core::secret::SecretString;
use reqwest::blocking::Client;

use crate::anthropic::{anthropic_error_summary, build_anthropic_body, parse_anthropic_response};
use crate::model::{ModelProvider, ModelRequest, ModelResponse};

const BEDROCK_ANTHROPIC_VERSION: &str = "bedrock-2023-05-31";
const BEDROCK_SERVICE: &str = "bedrock";

#[derive(Debug, Clone)]
pub struct BedrockProvider {
    client: Client,
    access_key: SecretString,
    secret_key: SecretString,
    /// `None` when no session token was configured (long-term credentials).
    session_token: Option<SecretString>,
    region: String,
    model_id: String,
    // Stored for potential future use (e.g., override request defaults).
    // Not read in the current `complete` implementation because the request
    // carries its own max_output_tokens and temperature.
    #[allow(dead_code)]
    max_output_tokens: u32,
    #[allow(dead_code)]
    temperature: f32,
    // Stored alongside the client for diagnostics / clone equality; the
    // client already has the timeout baked in from construction.
    #[allow(dead_code)]
    timeout_secs: u64,
}

impl BedrockProvider {
    /// Construct from project config. Returns `Ok(None)` when:
    ///   - `config.model.provider != "bedrock"` (different provider configured)
    ///   - any of `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, or region is
    ///     absent (parity with `AnthropicProvider::from_config`).
    pub fn from_config(repo_root: &Path, config: &ProjectConfig) -> KimetsuResult<Option<Self>> {
        Self::from_config_with_key(repo_root, config, None)
    }

    /// Same as `from_config`; `_key_override` is intentionally ignored — AWS
    /// credentials are always resolved from the three dedicated env-vars
    /// (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, optional
    /// `AWS_SESSION_TOKEN`). A single "API key" override is not meaningful for
    /// SigV4-signed requests.
    pub fn from_config_with_key(
        repo_root: &Path,
        config: &ProjectConfig,
        _key_override: Option<&str>,
    ) -> KimetsuResult<Option<Self>> {
        if config.model.provider != "bedrock" {
            return Ok(None);
        }

        let Some(access_key) = resolve_env_value(repo_root, "AWS_ACCESS_KEY_ID") else {
            return Ok(None);
        };
        let Some(secret_key) = resolve_env_value(repo_root, "AWS_SECRET_ACCESS_KEY") else {
            return Ok(None);
        };
        let session_token = resolve_env_value(repo_root, "AWS_SESSION_TOKEN");

        let region = resolve_region(
            repo_root,
            config.model.region.as_deref(),
            &config.model.region_env,
        );
        let Some(region) = region else {
            return Ok(None);
        };

        let client = Client::builder()
            .timeout(Duration::from_secs(config.model.request_timeout_secs))
            .build()?;

        Ok(Some(Self {
            client,
            access_key: SecretString::new(access_key),
            secret_key: SecretString::new(secret_key),
            session_token: session_token.map(SecretString::new),
            region,
            model_id: config.model.model.clone(),
            max_output_tokens: config.model.max_output_tokens,
            temperature: config.model.temperature,
            timeout_secs: config.model.request_timeout_secs,
        }))
    }

    /// Build a provider directly from resolved distiller values.
    #[allow(clippy::too_many_arguments)]
    pub fn for_distiller(
        model_id: impl Into<String>,
        region: impl Into<String>,
        access_key: impl Into<String>,
        secret_key: impl Into<String>,
        session_token: Option<String>,
        max_output_tokens: u32,
        temperature: f32,
        timeout_secs: u64,
    ) -> KimetsuResult<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(timeout_secs))
            .build()?;
        Ok(Self {
            client,
            access_key: SecretString::new(access_key.into()),
            secret_key: SecretString::new(secret_key.into()),
            session_token: session_token.map(SecretString::new),
            region: region.into(),
            model_id: model_id.into(),
            max_output_tokens,
            temperature,
            timeout_secs,
        })
    }

    /// Returns the Bedrock model ID (e.g. `anthropic.claude-3-5-haiku-20241022-v1:0`).
    pub fn model_name(&self) -> &str {
        &self.model_id
    }
}

/// Resolve AWS region: literal in config → env-var named by region_env →
/// `AWS_DEFAULT_REGION` fallback.
fn resolve_region(repo_root: &Path, literal: Option<&str>, region_env: &str) -> Option<String> {
    if let Some(r) = literal.filter(|s| !s.trim().is_empty()) {
        return Some(r.to_string());
    }
    if let Some(r) = resolve_env_value(repo_root, region_env) {
        return Some(r);
    }
    resolve_env_value(repo_root, "AWS_DEFAULT_REGION")
}

/// Sign `payload` for a Bedrock InvokeModel POST to `url` and return the
/// headers that must be added to the request (as `(name, value)` pairs).
/// `time` is injectable so tests can pin it to a fixed instant.
pub(crate) fn sign_bedrock_headers(
    access_key: &str,
    secret_key: &str,
    session_token: Option<&str>,
    region: &str,
    url: &str,
    payload: &[u8],
    time: SystemTime,
) -> KimetsuResult<Vec<(String, String)>> {
    let creds = Credentials::new(
        access_key,
        secret_key,
        session_token.map(str::to_string),
        None,
        "kimetsu-bedrock",
    );
    let identity: aws_smithy_runtime_api::client::identity::Identity = creds.into();

    let settings = SigningSettings::default();
    let params: aws_sigv4::http_request::SigningParams<'_> = v4::SigningParams::builder()
        .identity(&identity)
        .region(region)
        .name(BEDROCK_SERVICE)
        .time(time)
        .settings(settings)
        .build()
        .map_err(|e| format!("bedrock signing params: {e}"))?
        .into();

    // Build the signable request. The signer computes host from the URL, so we
    // only need to pass content-type alongside the payload; the signer adds
    // x-amz-date and Authorization (and x-amz-security-token when present).
    let signable = SignableRequest::new(
        "POST",
        url,
        [("content-type", "application/json")].into_iter(),
        SignableBody::Bytes(payload),
    )
    .map_err(|e| format!("bedrock signable request: {e}"))?;

    let (instructions, _signature) = sign(signable, &params)?.into_parts();

    // Materialise the signing instructions into an http::Request so we can
    // read the final header set.
    let mut http_req = http::Request::builder()
        .uri(url)
        .method("POST")
        .header("content-type", "application/json")
        .body(())
        .map_err(|e| format!("bedrock http request builder: {e}"))?;

    instructions.apply_to_request_http1x(&mut http_req);

    let headers = http_req
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_string(),
                value.to_str().unwrap_or("").to_string(),
            )
        })
        .collect();

    Ok(headers)
}

impl ModelProvider for BedrockProvider {
    fn complete(&mut self, request: ModelRequest) -> KimetsuResult<ModelResponse> {
        // Bedrock InvokeModel: model lives in the URL path, not the body.
        // anthropic_version must be in the body (not a header).
        let body = build_anthropic_body(
            None,
            &self.model_id,
            Some(BEDROCK_ANTHROPIC_VERSION),
            &request,
        );
        let payload = serde_json::to_vec(&body)?;
        let url = format!(
            "https://bedrock-runtime.{}.amazonaws.com/model/{}/invoke",
            self.region,
            url_encode_model_id(&self.model_id),
        );

        let headers = sign_bedrock_headers(
            self.access_key.expose_secret(),
            self.secret_key.expose_secret(),
            self.session_token.as_ref().map(|s| s.expose_secret()),
            &self.region,
            &url,
            &payload,
            SystemTime::now(),
        )?;

        let mut req = self.client.post(&url);
        for (name, value) in &headers {
            req = req.header(name.as_str(), value.as_str());
        }
        let response = req.body(payload).send()?;

        let status = response.status();
        let response_text = response.text()?;
        if !status.is_success() {
            return Err(format!(
                "bedrock request failed ({status}): {}",
                anthropic_error_summary(&response_text)
            )
            .into());
        }

        parse_anthropic_response(&response_text)
    }
}

/// Percent-encode characters in model IDs that could be misinterpreted in URL
/// paths. Bedrock model IDs typically contain only alphanumerics, hyphens,
/// dots, and colons — but the colon must be percent-encoded in URL paths to
/// avoid ambiguity with `scheme:`.
fn url_encode_model_id(model_id: &str) -> String {
    // Only colons need encoding in practice; percent-encode the rest if needed.
    model_id.replace(':', "%3A")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{MessageContent, MessageRole, ModelMessage, ToolChoice};
    use serde_json::json;

    fn simple_request() -> ModelRequest {
        ModelRequest {
            messages: vec![ModelMessage::user_text("Hello")],
            tools: Vec::new(),
            tool_choice: ToolChoice::None,
            max_output_tokens: 100,
            temperature: 0.5,
            metadata: serde_json::Value::Null,
        }
    }

    // ── A7: build_anthropic_body (Bedrock mode) ───────────────────────────

    #[test]
    fn bedrock_body_has_anthropic_version_and_no_model_key() {
        let req = simple_request();
        let body = build_anthropic_body(
            None,
            "claude-opus-4-6",
            Some(BEDROCK_ANTHROPIC_VERSION),
            &req,
        );
        assert_eq!(
            body["anthropic_version"], BEDROCK_ANTHROPIC_VERSION,
            "anthropic_version must match Bedrock spec"
        );
        assert!(
            body.get("model").is_none(),
            "model key must be absent in Bedrock body (lives in URL)"
        );
        assert!(body.get("messages").is_some(), "messages must be present");
        assert_eq!(body["max_tokens"], 100);
    }

    #[test]
    fn direct_anthropic_body_regression() {
        // build_anthropic_body(Some(model), None, req) must preserve the old
        // behaviour: model in body, no anthropic_version in body.
        let req = simple_request();
        let body = build_anthropic_body(Some("claude-opus-4-7"), "claude-opus-4-7", None, &req);
        assert_eq!(body["model"], "claude-opus-4-7");
        assert!(
            body.get("anthropic_version").is_none(),
            "direct Anthropic mode must not inject anthropic_version into body"
        );
    }

    #[test]
    fn bedrock_body_includes_system_and_tools() {
        use crate::model::{ToolCall, ToolDefinition};
        let req = ModelRequest {
            messages: vec![
                ModelMessage {
                    role: MessageRole::System,
                    content: vec![MessageContent::Text {
                        text: "Be helpful.".to_string(),
                    }],
                },
                ModelMessage::user_text("Do the thing."),
                ModelMessage::assistant_tool_calls(vec![ToolCall {
                    id: "t1".to_string(),
                    name: "do_thing".to_string(),
                    input: json!({}),
                }]),
                ModelMessage::tool_result("t1", "do_thing", json!({"ok": true})),
            ],
            tools: vec![ToolDefinition {
                name: "do_thing".to_string(),
                description: "Does the thing.".to_string(),
                input_schema: json!({ "type": "object" }),
            }],
            tool_choice: ToolChoice::Auto,
            max_output_tokens: 256,
            temperature: 0.2,
            metadata: serde_json::Value::Null,
        };
        let body = build_anthropic_body(
            None,
            "claude-opus-4-6",
            Some(BEDROCK_ANTHROPIC_VERSION),
            &req,
        );
        assert_eq!(body["system"], "Be helpful.");
        assert!(body.get("tools").is_some());
        assert!(body.get("model").is_none());
        assert_eq!(body["anthropic_version"], BEDROCK_ANTHROPIC_VERSION);
    }

    // ── A7: parse_anthropic_response on Bedrock-shaped JSON ───────────────

    #[test]
    fn parse_bedrock_response_shape() {
        // Bedrock returns the same JSON structure as the direct Anthropic API.
        let json = r#"{
            "content": [{"type": "text", "text": "Hello from Bedrock!"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 15, "output_tokens": 8}
        }"#;
        let resp = parse_anthropic_response(json).expect("parse");
        assert_eq!(resp.text.as_deref(), Some("Hello from Bedrock!"));
        assert!(resp.tool_calls.is_empty());
        assert_eq!(resp.usage.input_tokens, 15);
        assert_eq!(resp.usage.output_tokens, 8);
    }

    // ── A7: SigV4 determinism ─────────────────────────────────────────────

    #[test]
    fn sigv4_headers_contain_expected_structure() {
        // Fixed time so the date-based parts of the signature are deterministic.
        // 2024-01-15T12:00:00Z
        let fixed_time = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_705_320_000);

        let payload = b"{\"test\":true}";
        let region = "us-east-1";
        let model_id = "anthropic.claude-3-haiku-20240307-v1%3A0";
        let url = format!("https://bedrock-runtime.{region}.amazonaws.com/model/{model_id}/invoke");

        let headers = sign_bedrock_headers(
            "AKIAIOSFODNN7EXAMPLE",
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            None,
            region,
            &url,
            payload,
            fixed_time,
        )
        .expect("signing must not fail");

        let header_map: std::collections::HashMap<String, String> = headers.into_iter().collect();

        // Authorization must use AWS4-HMAC-SHA256
        let auth = header_map
            .get("authorization")
            .expect("authorization header must be present");
        assert!(
            auth.contains("AWS4-HMAC-SHA256"),
            "authorization must use AWS4-HMAC-SHA256, got: {auth}"
        );

        // Credential scope must contain date/region/bedrock/aws4_request
        assert!(
            auth.contains(&format!("/{region}/bedrock/aws4_request")),
            "credential scope must contain /{region}/bedrock/aws4_request, got: {auth}"
        );

        // x-amz-date must be present
        assert!(
            header_map.contains_key("x-amz-date"),
            "x-amz-date header must be present"
        );

        // x-amz-date must start with the date part of fixed_time (2024-01-15 → 20240115)
        let amz_date = header_map.get("x-amz-date").unwrap();
        assert!(
            amz_date.starts_with("20240115"),
            "x-amz-date must start with 20240115, got: {amz_date}"
        );
    }

    #[test]
    fn sigv4_with_session_token_adds_security_token_header() {
        let fixed_time = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_705_320_000);
        let payload = b"{}";
        let url = "https://bedrock-runtime.us-west-2.amazonaws.com/model/anthropic.claude/invoke";

        let headers = sign_bedrock_headers(
            "AKID",
            "SECRET",
            Some("MY-SESSION-TOKEN"),
            "us-west-2",
            url,
            payload,
            fixed_time,
        )
        .expect("signing must not fail");

        let header_map: std::collections::HashMap<String, String> = headers.into_iter().collect();

        assert!(
            header_map.contains_key("x-amz-security-token"),
            "x-amz-security-token must be present when session_token is set"
        );
        assert_eq!(
            header_map.get("x-amz-security-token").unwrap(),
            "MY-SESSION-TOKEN"
        );
    }

    // ── A7: from_config returns Ok(None) when creds/region absent ────────

    #[test]
    fn from_config_returns_none_when_not_bedrock_provider() {
        let dir = tempdir();
        let mut config = ProjectConfig::default_for_project("test");
        config.model.provider = "anthropic".to_string();
        // No env file — just no bedrock provider
        let result = BedrockProvider::from_config(&dir, &config).expect("should not error");
        assert!(result.is_none(), "non-bedrock provider must yield None");
        cleanup(&dir);
    }

    #[test]
    fn from_config_returns_none_when_access_key_missing() {
        let dir = tempdir();
        // Write only the secret key, no access key
        std::fs::write(
            dir.join(".env"),
            "AWS_SECRET_ACCESS_KEY=mysecret\nAWS_REGION=us-east-1\n",
        )
        .unwrap();
        let mut config = ProjectConfig::default_for_project("test");
        config.model.provider = "bedrock".to_string();
        config.model.model = "anthropic.claude-3-haiku-20240307-v1:0".to_string();

        let result = BedrockProvider::from_config(&dir, &config).expect("should not error");
        assert!(result.is_none(), "missing access key must yield None");
        cleanup(&dir);
    }

    #[test]
    fn from_config_returns_none_when_region_missing() {
        let dir = tempdir();
        // Write creds but no region
        std::fs::write(
            dir.join(".env"),
            "AWS_ACCESS_KEY_ID=mykey\nAWS_SECRET_ACCESS_KEY=mysecret\n",
        )
        .unwrap();
        let mut config = ProjectConfig::default_for_project("test");
        config.model.provider = "bedrock".to_string();
        config.model.model = "anthropic.claude-3-haiku-20240307-v1:0".to_string();
        // Ensure no literal region set
        config.model.region = None;

        let result = BedrockProvider::from_config(&dir, &config).expect("should not error");
        assert!(result.is_none(), "missing region must yield None");
        cleanup(&dir);
    }

    #[test]
    fn from_config_builds_provider_when_all_present() {
        let dir = tempdir();
        std::fs::write(
            dir.join(".env"),
            "AWS_ACCESS_KEY_ID=AKIATEST\nAWS_SECRET_ACCESS_KEY=SECRETTEST\nAWS_REGION=us-east-1\n",
        )
        .unwrap();
        let mut config = ProjectConfig::default_for_project("test");
        config.model.provider = "bedrock".to_string();
        config.model.model = "anthropic.claude-3-haiku-20240307-v1:0".to_string();

        let result = BedrockProvider::from_config(&dir, &config).expect("should not error");
        let provider = result.expect("provider must be Some when all creds present");
        assert_eq!(
            provider.model_name(),
            "anthropic.claude-3-haiku-20240307-v1:0"
        );
        assert_eq!(provider.region, "us-east-1");
        cleanup(&dir);
    }

    // ── A7: normalize_distiller_provider coverage (tested in distiller) ──
    // (tested in crates/kimetsu-cli/src/distiller.rs)

    // ── A7: ignored integration test (requires real AWS creds + Bedrock) ──

    /// Run with `cargo test -p kimetsu-agent -- --ignored bedrock_live_invoke`
    /// Requires: AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY, AWS_REGION in env
    /// and Bedrock model access for `anthropic.claude-3-haiku-20240307-v1:0`.
    #[test]
    #[ignore = "requires real AWS credentials and Bedrock model access"]
    fn bedrock_live_invoke() {
        let access = std::env::var("AWS_ACCESS_KEY_ID").expect("AWS_ACCESS_KEY_ID required");
        let secret =
            std::env::var("AWS_SECRET_ACCESS_KEY").expect("AWS_SECRET_ACCESS_KEY required");
        let region = std::env::var("AWS_REGION")
            .or_else(|_| std::env::var("AWS_DEFAULT_REGION"))
            .expect("AWS_REGION or AWS_DEFAULT_REGION required");
        let session = std::env::var("AWS_SESSION_TOKEN").ok();

        let mut provider = BedrockProvider::for_distiller(
            "anthropic.claude-3-haiku-20240307-v1:0",
            region,
            access,
            secret,
            session,
            256,
            0.2,
            30,
        )
        .expect("build provider");

        let request = ModelRequest {
            messages: vec![ModelMessage::user_text("Reply with exactly one word: pong")],
            tools: Vec::new(),
            tool_choice: ToolChoice::None,
            max_output_tokens: 32,
            temperature: 0.0,
            metadata: serde_json::Value::Null,
        };
        let response = provider.complete(request).expect("live Bedrock call");
        println!("live response: {:?}", response.text);
        assert!(response.text.is_some(), "expected a text response");
    }

    // ── helpers ───────────────────────────────────────────────────────────

    fn tempdir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kimetsu_bedrock_test_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn cleanup(dir: &std::path::Path) {
        std::fs::remove_dir_all(dir).ok();
    }
}
