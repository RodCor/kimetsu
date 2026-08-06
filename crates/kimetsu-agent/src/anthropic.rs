use std::path::Path;
use std::time::Duration;

use kimetsu_core::KimetsuResult;
use kimetsu_core::config::ProjectConfig;
use kimetsu_core::env_file::resolve_env_value;
use kimetsu_core::secret::SecretString;
use reqwest::blocking::Client;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::model::{
    MessageContent, MessageRole, ModelProvider, ModelRequest, ModelResponse, StopReason,
    TokenUsage, ToolCall, ToolChoice,
};

const MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";

#[derive(Debug, Clone)]
pub struct AnthropicProvider {
    client: Client,
    // v0.4.9: SecretString — same rationale as ClaudeCodeProvider.
    // Debug / Display / serde all emit "[REDACTED]"; cleartext
    // only flows out via expose_secret() at the HTTP header
    // injection below.
    api_key: SecretString,
    model: String,
    /// When set, requests POST to `<base_url>/v1/messages` (Anthropic-
    /// compatible endpoints such as a LiteLLM proxy). When `None`, the
    /// default Anthropic API URL is used.
    base_url: Option<String>,
}

impl AnthropicProvider {
    pub fn from_config(repo_root: &Path, config: &ProjectConfig) -> KimetsuResult<Option<Self>> {
        Self::from_config_with_key(repo_root, config, None)
    }

    pub fn from_config_with_key(
        repo_root: &Path,
        config: &ProjectConfig,
        api_key_override: Option<&str>,
    ) -> KimetsuResult<Option<Self>> {
        if config.model.provider != "anthropic" {
            return Err(format!("unsupported model provider: {}", config.model.provider).into());
        }

        let api_key = api_key_override
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string)
            .or_else(|| resolve_env_value(repo_root, &config.model.api_key_env));
        let Some(api_key) = api_key else {
            return Ok(None);
        };

        let client = Client::builder()
            .timeout(Duration::from_secs(config.model.request_timeout_secs))
            .build()?;

        Ok(Some(Self {
            client,
            api_key: SecretString::new(api_key),
            model: config.model.model.clone(),
            base_url: None,
        }))
    }

    pub fn model_name(&self) -> &str {
        &self.model
    }

    /// Build a provider directly from resolved values — used by the
    /// SessionEnd distiller, which controls model/key/base independent of
    /// the project's `[model]` section. `base_url` is normalized: empty/
    /// whitespace becomes `None`.
    pub fn for_distiller(
        model: impl Into<String>,
        api_key: impl Into<String>,
        base_url: Option<String>,
        timeout_secs: u64,
    ) -> KimetsuResult<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(timeout_secs))
            .build()?;
        Ok(Self {
            client,
            api_key: SecretString::new(api_key.into()),
            model: model.into(),
            base_url: base_url.filter(|value| !value.trim().is_empty()),
        })
    }
}

/// Resolve the messages endpoint: `<base>/v1/messages` when a base URL is
/// configured, else the default Anthropic API URL.
fn messages_url(base_url: &Option<String>) -> String {
    match base_url {
        Some(base) => format!("{}/v1/messages", base.trim_end_matches('/')),
        None => MESSAGES_URL.to_string(),
    }
}

impl ModelProvider for AnthropicProvider {
    fn complete(&mut self, request: ModelRequest) -> KimetsuResult<ModelResponse> {
        // Pass Some(model) — the direct API needs the model in the body.
        // anthropic-version is carried in the HTTP header for the direct API,
        // so we pass None here; the header is set explicitly below.
        let body = build_anthropic_body(Some(&self.model), &self.model, None, &request);
        let url = messages_url(&self.base_url);
        let response = self
            .client
            .post(&url)
            // v0.4.9: explicit cleartext leak point for the HTTP
            // header. The reqwest client never logs header values
            // by default; if a future caller adds request logging
            // this is the line to add redaction at.
            .header("x-api-key", self.api_key.expose_secret())
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()?;

        let status = response.status();
        let response_text = response.text()?;
        if !status.is_success() {
            return Err(format!(
                "anthropic request failed ({status}): {}",
                anthropic_error_summary(&response_text)
            )
            .into());
        }

        parse_anthropic_response(&response_text)
    }
}

/// v2.6: does this Anthropic model still accept a `temperature`?
///
/// Sampling parameters were **removed** from the Claude line: `temperature`,
/// `top_p` and `top_k` return a hard 400 on Opus 4.7 and every model after it,
/// and Sonnet 5 rejects any non-default value. Kimetsu sends a deliberately
/// low temperature (0.1–0.3) on every pipeline call, so on those models every
/// Deep-tier request fails outright — the distiller, `ask`, and reflection are
/// dead, not degraded.
///
/// The list is an **allowlist of models known to accept it**, and an unknown
/// model omits the parameter. That direction is deliberate. Getting it wrong
/// by omitting costs a little determinism on one distillation; getting it
/// wrong by sending costs the entire feature, loudly, on a model that did not
/// exist when this code was written. Sampling removal has only ever moved one
/// way across the Claude line, so "unknown means newer" is the safer read.
///
/// Ids arrive in several shapes — bare (`claude-opus-5`), Bedrock
/// (`anthropic.claude-opus-5`), and Bedrock cross-region inference profiles
/// (`us.anthropic.claude-opus-5`). Rather than strip a fixed prefix, match from
/// the first `claude-`, which is common to all of them.
pub(crate) fn accepts_temperature(model: &str) -> bool {
    let lowered = model.trim().to_ascii_lowercase();
    let Some(start) = lowered.find("claude-") else {
        // Not a Claude id at all (a proxy alias, say). Omit, per the rule above.
        return false;
    };
    let id = &lowered[start..];
    // Everything at or below the Opus 4.6 / Sonnet 4.6 generation still takes
    // sampling parameters. Opus 4.7 is where they were removed.
    const ACCEPTS: [&str; 8] = [
        "claude-opus-4-6",
        "claude-opus-4-5",
        "claude-opus-4-1",
        "claude-opus-4-0",
        "claude-sonnet-4-6",
        "claude-sonnet-4-5",
        "claude-sonnet-4-0",
        "claude-haiku-4-5",
    ];
    ACCEPTS.iter().any(|prefix| id.starts_with(prefix))
        // Claude 3.x and 2.x predate the removal entirely.
        || id.starts_with("claude-3")
        || id.starts_with("claude-2")
}

/// Build the JSON body for an Anthropic-wire request.
///
/// - `model`: when `Some`, injects `"model": <value>` into the body (direct
///   Anthropic API). Pass `None` for Bedrock — the model lives in the URL, not
///   the body.
/// - `anthropic_version`: when `Some`, injects `"anthropic_version": <value>`
///   (Bedrock requires `"bedrock-2023-05-31"` here). Pass `None` for the direct
///   API — the version is carried in the `anthropic-version` HTTP header there.
/// - `model_id`: the model's identity, used only to decide which parameters it
///   accepts. Separate from `model` because Bedrock puts the id in the URL and
///   omits it from the body — but the body still has to be built *for* that
///   model. Always pass the real id.
pub(crate) fn build_anthropic_body(
    model: Option<&str>,
    model_id: &str,
    anthropic_version: Option<&str>,
    request: &ModelRequest,
) -> Value {
    let mut system_parts = Vec::new();
    let mut messages = Vec::new();

    for message in &request.messages {
        if matches!(message.role, MessageRole::System) {
            let system_text = message.text_content();
            if !system_text.trim().is_empty() {
                system_parts.push(system_text);
            }
            continue;
        }

        let role = match message.role {
            MessageRole::Assistant => "assistant",
            MessageRole::User | MessageRole::Tool => "user",
            MessageRole::System => unreachable!(),
        };
        let content = message
            .content
            .iter()
            .filter_map(map_content_block)
            .collect::<Vec<_>>();
        if !content.is_empty() {
            messages.push(json!({
                "role": role,
                "content": content,
            }));
        }
    }

    let mut body = json!({
        "max_tokens": request.max_output_tokens,
        "messages": messages,
    });

    // Omitted entirely on models that removed sampling parameters — sending it
    // there is a 400, not a soft fallback. See `accepts_temperature`.
    if accepts_temperature(model_id) {
        body["temperature"] = json!(request.temperature);
    }

    if let Some(m) = model {
        body["model"] = json!(m);
    }

    if let Some(v) = anthropic_version {
        body["anthropic_version"] = json!(v);
    }

    if !system_parts.is_empty() {
        body["system"] = json!(system_parts.join("\n\n"));
    }

    if !request.tools.is_empty() && !matches!(request.tool_choice, ToolChoice::None) {
        body["tools"] = json!(
            request
                .tools
                .iter()
                .map(|tool| json!({
                    "name": &tool.name,
                    "description": &tool.description,
                    "input_schema": &tool.input_schema,
                }))
                .collect::<Vec<_>>()
        );
        body["tool_choice"] = match request.tool_choice {
            ToolChoice::Auto => json!({ "type": "auto" }),
            ToolChoice::Required => json!({ "type": "any" }),
            ToolChoice::None => Value::Null,
        };
    }

    body
}

/// Kept for back-compat within this module's tests (see below).
#[cfg(test)]
fn build_request_body(model: &str, request: &ModelRequest) -> Value {
    build_anthropic_body(Some(model), model, None, request)
}

fn map_content_block(content: &MessageContent) -> Option<Value> {
    match content {
        MessageContent::Text { text } => {
            if text.trim().is_empty() {
                None
            } else {
                Some(json!({ "type": "text", "text": text }))
            }
        }
        MessageContent::ToolCall { id, name, input } => Some(json!({
            "type": "tool_use",
            "id": id,
            "name": name,
            "input": input,
        })),
        MessageContent::ToolResult {
            tool_call_id,
            output,
            ..
        } => Some(json!({
            "type": "tool_result",
            "tool_use_id": tool_call_id,
            "content": serde_json::to_string(output).unwrap_or_else(|_| output.to_string()),
        })),
    }
}

pub(crate) fn parse_anthropic_response(response_text: &str) -> KimetsuResult<ModelResponse> {
    let response: AnthropicResponse = serde_json::from_str(response_text)?;
    let mut text_parts = Vec::new();
    let mut tool_calls = Vec::new();

    for block in response.content {
        match block {
            AnthropicContent::Text { text } => text_parts.push(text),
            AnthropicContent::ToolUse { id, name, input } => {
                tool_calls.push(ToolCall { id, name, input });
            }
            AnthropicContent::Other => {}
        }
    }

    let stop_reason = if !tool_calls.is_empty() {
        StopReason::ToolUse
    } else {
        match response.stop_reason.as_deref() {
            Some("end_turn") | None => StopReason::EndTurn,
            Some("tool_use") => StopReason::ToolUse,
            Some("max_tokens") => StopReason::MaxTokens,
            Some("refusal") => StopReason::Refusal,
            _ => StopReason::Error,
        }
    };

    Ok(ModelResponse {
        text: if text_parts.is_empty() {
            None
        } else {
            Some(text_parts.join("\n"))
        },
        tool_calls,
        stop_reason,
        usage: response.usage.unwrap_or_default().into(),
    })
}

pub(crate) fn anthropic_error_summary(response_text: &str) -> String {
    let parsed = serde_json::from_str::<Value>(response_text).ok();
    let message = parsed
        .as_ref()
        .and_then(|value| value.pointer("/error/message"))
        .and_then(Value::as_str)
        .or_else(|| {
            parsed
                .as_ref()
                .and_then(|value| value.get("message"))
                .and_then(Value::as_str)
        })
        .unwrap_or(response_text);
    truncate(message, 700)
}

fn truncate(value: &str, max_chars: usize) -> String {
    let mut out = value.chars().take(max_chars).collect::<String>();
    if value.chars().count() > max_chars {
        out.push_str("...");
    }
    out
}

#[derive(Debug, Deserialize)]
struct AnthropicResponse {
    content: Vec<AnthropicContent>,
    stop_reason: Option<String>,
    usage: Option<AnthropicUsage>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicContent {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Default, Deserialize)]
struct AnthropicUsage {
    input_tokens: u32,
    output_tokens: u32,
    /// v0.3.4a: Anthropic returns 0 (not absent) when no cache write
    /// happened on this call, but we accept absence too via Option +
    /// `unwrap_or_default` for forward-compat with older API responses.
    #[serde(default)]
    cache_creation_input_tokens: Option<u32>,
    #[serde(default)]
    cache_read_input_tokens: Option<u32>,
}

impl From<AnthropicUsage> for TokenUsage {
    fn from(value: AnthropicUsage) -> Self {
        Self {
            input_tokens: value.input_tokens,
            output_tokens: value.output_tokens,
            cost_usd: 0.0,
            // v0.3.4a: surface cache stats so callers get the same
            // visibility as the claude_code provider.
            cache_creation_input_tokens: value.cache_creation_input_tokens.unwrap_or_default(),
            cache_read_input_tokens: value.cache_read_input_tokens.unwrap_or_default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ModelMessage, ToolDefinition};

    /// v2.6: every current frontier Claude model rejects `temperature` with a
    /// 400. Kimetsu sends 0.1–0.3 on every pipeline call, so before this gate
    /// the whole Deep tier was dead on anything from Opus 4.7 onward.
    #[test]
    fn temperature_is_omitted_on_models_that_reject_it() {
        for model in [
            "claude-opus-5",
            "claude-opus-4-8",
            "claude-opus-4-7",
            "claude-fable-5",
            "claude-mythos-5",
            "claude-sonnet-5",
            "anthropic.claude-opus-5", // Bedrock id
        ] {
            assert!(
                !accepts_temperature(model),
                "{model} rejects sampling params; sending temperature is a 400"
            );
        }
    }

    /// The older generation still takes it, and silently dropping it there
    /// would cost the determinism the low temperature was chosen for.
    #[test]
    fn temperature_is_kept_on_models_that_accept_it() {
        for model in [
            "claude-opus-4-6",
            "claude-sonnet-4-6",
            "claude-sonnet-4-5",
            "claude-haiku-4-5",
            "claude-3-5-sonnet-20241022",
            "anthropic.claude-sonnet-4-6",
            // Bedrock legacy ARN-style and cross-region inference-profile ids.
            "anthropic.claude-3-5-sonnet-20241022-v2:0",
            "us.anthropic.claude-sonnet-4-5",
        ] {
            assert!(accepts_temperature(model), "{model} accepts temperature");
        }
    }

    /// The cross-region form must not be misread as a *new* model and silently
    /// lose the temperature it is entitled to.
    #[test]
    fn a_region_prefixed_frontier_id_still_omits_temperature() {
        assert!(!accepts_temperature("us.anthropic.claude-opus-5"));
        assert!(!accepts_temperature("eu.anthropic.claude-opus-4-7"));
    }

    /// A model this build has never heard of is assumed to be newer than the
    /// removal, because that is the failure that costs less: omitting loses a
    /// little determinism, sending loses the entire request.
    #[test]
    fn an_unknown_model_omits_temperature() {
        assert!(!accepts_temperature("claude-opus-6"));
        assert!(!accepts_temperature("some-future-model"));
        assert!(!accepts_temperature(""));
    }

    /// The gate has to reach the wire, not just the helper.
    #[test]
    fn the_request_body_drops_temperature_for_a_frontier_model() {
        let req = ModelRequest {
            messages: vec![ModelMessage::user_text("hi")],
            temperature: 0.2,
            max_output_tokens: 512,
            tools: Vec::new(),
            tool_choice: ToolChoice::None,
            metadata: serde_json::Value::Null,
        };
        let frontier = build_anthropic_body(Some("claude-opus-5"), "claude-opus-5", None, &req);
        assert!(
            frontier.get("temperature").is_none(),
            "temperature must not reach a model that 400s on it: {frontier}"
        );
        let older = build_anthropic_body(Some("claude-opus-4-6"), "claude-opus-4-6", None, &req);
        let sent = older["temperature"].as_f64().expect("temperature present");
        assert!((sent - 0.2).abs() < 1e-6, "got {sent}");
    }

    /// v0.4.9 regression guard. Mirror of the ClaudeCodeProvider
    /// test — `#[derive(Debug)]` must not leak `api_key`.
    #[test]
    fn debug_format_does_not_leak_api_key() {
        let token = "sk-ant-api03-DEFINITELY-LEAKED-IF-BROKEN-1234567890";
        let provider = AnthropicProvider {
            client: Client::new(),
            api_key: SecretString::new(token),
            model: "claude-opus-4-7".into(),
            base_url: None,
        };
        let dbg = format!("{:?}", provider);
        assert!(
            !dbg.contains("DEFINITELY-LEAKED-IF-BROKEN"),
            "Debug-print MUST NOT include the inner token: {dbg}"
        );
        assert!(dbg.contains("REDACTED"));
        assert!(dbg.contains("claude-opus-4-7"));
    }

    #[test]
    fn messages_url_uses_base_when_set() {
        assert_eq!(messages_url(&None), MESSAGES_URL);
        assert_eq!(
            messages_url(&Some("http://localhost:4000".to_string())),
            "http://localhost:4000/v1/messages"
        );
        assert_eq!(
            messages_url(&Some("http://localhost:4000/".to_string())),
            "http://localhost:4000/v1/messages"
        );
    }

    #[test]
    fn for_distiller_builds_provider_with_base_url() {
        let p = AnthropicProvider::for_distiller(
            "claude-haiku-4-5",
            "sk-test",
            Some("http://localhost:4000".to_string()),
            60,
        )
        .expect("build");
        assert_eq!(p.model_name(), "claude-haiku-4-5");
        assert_eq!(p.base_url.as_deref(), Some("http://localhost:4000"));
    }

    #[test]
    fn request_maps_system_and_tool_blocks_to_anthropic_shape() {
        let request = ModelRequest {
            messages: vec![
                ModelMessage {
                    role: MessageRole::System,
                    content: vec![MessageContent::Text {
                        text: "Use strict JSON.".to_string(),
                    }],
                },
                ModelMessage::user_text("Plan the change."),
                ModelMessage::assistant_tool_calls(vec![ToolCall {
                    id: "toolu_1".to_string(),
                    name: "read_file".to_string(),
                    input: json!({ "path": "src/lib.rs" }),
                }]),
                ModelMessage::tool_result(
                    "toolu_1",
                    "read_file",
                    json!({ "ok": true, "output": "pub fn x() {}" }),
                ),
            ],
            tools: vec![ToolDefinition {
                name: "read_file".to_string(),
                description: "Read a file.".to_string(),
                input_schema: json!({ "type": "object" }),
            }],
            tool_choice: ToolChoice::Auto,
            max_output_tokens: 1024,
            temperature: 0.2,
            metadata: Value::Null,
        };

        let body = build_request_body("claude-opus-4-7", &request);
        assert_eq!(body["system"], "Use strict JSON.");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][1]["content"][0]["type"], "tool_use");
        assert_eq!(body["messages"][2]["content"][0]["type"], "tool_result");
        assert_eq!(body["tools"][0]["name"], "read_file");
    }

    #[test]
    fn response_maps_text_tool_use_and_usage() {
        let response = parse_anthropic_response(
            r#"{
                "content": [
                    {"type": "text", "text": "Reading file."},
                    {"type": "tool_use", "id": "toolu_1", "name": "read_file", "input": {"path": "src/lib.rs"}}
                ],
                "stop_reason": "tool_use",
                "usage": {"input_tokens": 10, "output_tokens": 5}
            }"#,
        )
        .expect("parse response");

        assert_eq!(response.text.as_deref(), Some("Reading file."));
        assert_eq!(response.tool_calls.len(), 1);
        assert!(matches!(response.stop_reason, StopReason::ToolUse));
        assert_eq!(response.usage.input_tokens, 10);
        assert_eq!(response.usage.output_tokens, 5);
    }
}
