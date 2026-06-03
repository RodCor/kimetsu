use std::time::Duration;

use kimetsu_core::KimetsuResult;
use kimetsu_core::secret::SecretString;
use reqwest::blocking::Client;
use serde_json::{Value, json};

use crate::model::{
    MessageContent, MessageRole, ModelProvider, ModelRequest, ModelResponse, StopReason,
    TokenUsage, ToolCall,
};

const RESPONSES_URL: &str = "https://api.openai.com/v1/responses";

#[derive(Debug, Clone)]
pub struct OpenAiProvider {
    client: Client,
    api_key: SecretString,
    model: String,
    /// When set, requests POST to an OpenAI-compatible Responses API
    /// endpoint. Accepts either a root URL (`<base>/v1/responses`) or a
    /// conventional OpenAI-style base URL ending in `/v1`
    /// (`<base>/responses`).
    base_url: Option<String>,
}

impl OpenAiProvider {
    pub fn model_name(&self) -> &str {
        &self.model
    }

    /// Build a provider directly from resolved distiller values. Empty
    /// `base_url` is normalized to `None`.
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

fn responses_url(base_url: &Option<String>) -> String {
    match base_url {
        Some(base) => {
            let base = base.trim_end_matches('/');
            if base.ends_with("/v1") {
                format!("{base}/responses")
            } else {
                format!("{base}/v1/responses")
            }
        }
        None => RESPONSES_URL.to_string(),
    }
}

impl ModelProvider for OpenAiProvider {
    fn complete(&mut self, request: ModelRequest) -> KimetsuResult<ModelResponse> {
        let body = build_request_body(&self.model, &request);
        let response = self
            .client
            .post(responses_url(&self.base_url))
            .bearer_auth(self.api_key.expose_secret())
            .header("content-type", "application/json")
            .json(&body)
            .send()?;

        let status = response.status();
        let response_text = response.text()?;
        if !status.is_success() {
            return Err(format!(
                "openai request failed ({status}): {}",
                response_error_summary(&response_text)
            )
            .into());
        }

        parse_response(&response_text)
    }
}

fn build_request_body(model: &str, request: &ModelRequest) -> Value {
    let mut system_parts = Vec::new();
    let mut input_parts = Vec::new();

    for message in &request.messages {
        let text = message_text(message);
        if text.trim().is_empty() {
            continue;
        }
        if matches!(message.role, MessageRole::System) {
            system_parts.push(text);
        } else {
            input_parts.push(format!("{}: {text}", role_name(&message.role)));
        }
    }

    let mut body = json!({
        "model": model,
        "input": input_parts.join("\n\n"),
        "max_output_tokens": request.max_output_tokens,
    });

    if !system_parts.is_empty() {
        body["instructions"] = json!(system_parts.join("\n\n"));
    }

    body
}

fn role_name(role: &MessageRole) -> &'static str {
    match role {
        MessageRole::System => "system",
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::Tool => "tool",
    }
}

fn message_text(message: &crate::model::ModelMessage) -> String {
    message
        .content
        .iter()
        .filter_map(content_text)
        .collect::<Vec<_>>()
        .join("\n")
}

fn content_text(content: &MessageContent) -> Option<String> {
    match content {
        MessageContent::Text { text } => Some(text.clone()),
        MessageContent::ToolCall { name, input, .. } => {
            Some(format!("[tool {name}: {}]", compact_json(input)))
        }
        MessageContent::ToolResult { name, output, .. } => {
            Some(format!("[tool result {name}: {}]", compact_json(output)))
        }
    }
}

fn compact_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| value.to_string())
}

fn parse_response(response_text: &str) -> KimetsuResult<ModelResponse> {
    let value: Value = serde_json::from_str(response_text)?;
    let mut nested_text_parts = Vec::new();
    let mut saw_refusal = false;
    let mut tool_calls = Vec::new();

    for item in value
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match item.get("type").and_then(Value::as_str) {
            Some("message") | None => {
                for content in item
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    match content.get("type").and_then(Value::as_str) {
                        Some("output_text") => {
                            if let Some(text) = content.get("text").and_then(Value::as_str)
                                && !text.trim().is_empty()
                            {
                                nested_text_parts.push(text.to_string());
                            }
                        }
                        Some("refusal") => {
                            saw_refusal = true;
                            if let Some(text) = content.get("refusal").and_then(Value::as_str)
                                && !text.trim().is_empty()
                            {
                                nested_text_parts.push(text.to_string());
                            }
                        }
                        _ => {}
                    }
                }
            }
            Some("function_call") | Some("custom_tool_call") => {
                if let Some(call) = parse_tool_call(item) {
                    tool_calls.push(call);
                }
            }
            _ => {}
        }
    }

    let text_parts = if nested_text_parts.is_empty() {
        value
            .get("output_text")
            .and_then(Value::as_str)
            .filter(|text| !text.trim().is_empty())
            .map(|text| vec![text.to_string()])
            .unwrap_or_default()
    } else {
        nested_text_parts
    };

    let stop_reason = if !tool_calls.is_empty() {
        StopReason::ToolUse
    } else if saw_refusal {
        StopReason::Refusal
    } else {
        map_stop_reason(&value)
    };

    Ok(ModelResponse {
        text: if text_parts.is_empty() {
            None
        } else {
            Some(text_parts.join("\n"))
        },
        tool_calls,
        stop_reason,
        usage: parse_usage(&value),
    })
}

fn parse_tool_call(item: &Value) -> Option<ToolCall> {
    let name = item.get("name").and_then(Value::as_str)?.to_string();
    let id = item
        .get("call_id")
        .or_else(|| item.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let input = item
        .get("arguments")
        .or_else(|| item.get("input"))
        .map(parse_tool_input)
        .unwrap_or(Value::Null);
    Some(ToolCall { id, name, input })
}

fn parse_tool_input(value: &Value) -> Value {
    if let Some(text) = value.as_str() {
        serde_json::from_str(text).unwrap_or_else(|_| json!({ "input": text }))
    } else {
        value.clone()
    }
}

fn map_stop_reason(value: &Value) -> StopReason {
    if !value.get("error").unwrap_or(&Value::Null).is_null() {
        return StopReason::Error;
    }
    match value.get("status").and_then(Value::as_str) {
        Some("completed") | None => StopReason::EndTurn,
        Some("incomplete") => match value
            .pointer("/incomplete_details/reason")
            .and_then(Value::as_str)
        {
            Some("max_output_tokens") => StopReason::MaxTokens,
            Some("content_filter") => StopReason::Refusal,
            _ => StopReason::Error,
        },
        Some("failed") => StopReason::Error,
        _ => StopReason::Error,
    }
}

fn parse_usage(value: &Value) -> TokenUsage {
    TokenUsage {
        input_tokens: value_u32(value.pointer("/usage/input_tokens")),
        output_tokens: value_u32(value.pointer("/usage/output_tokens")),
        cost_usd: 0.0,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: value_u32(
            value.pointer("/usage/input_tokens_details/cached_tokens"),
        ),
    }
}

fn value_u32(value: Option<&Value>) -> u32 {
    value
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or_default()
}

fn response_error_summary(response_text: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ModelMessage;

    #[test]
    fn debug_format_does_not_leak_api_key() {
        let token = "sk-DEFINITELY-LEAKED-IF-BROKEN-1234567890abcdef";
        let provider = OpenAiProvider {
            client: Client::new(),
            api_key: SecretString::new(token),
            model: "gpt-5.4-mini".into(),
            base_url: None,
        };
        let dbg = format!("{:?}", provider);
        assert!(
            !dbg.contains("DEFINITELY-LEAKED-IF-BROKEN"),
            "Debug-print MUST NOT include the inner token: {dbg}"
        );
        assert!(dbg.contains("REDACTED"));
        assert!(dbg.contains("gpt-5.4-mini"));
    }

    #[test]
    fn responses_url_uses_base_when_set() {
        assert_eq!(responses_url(&None), RESPONSES_URL);
        assert_eq!(
            responses_url(&Some("http://localhost:4000".to_string())),
            "http://localhost:4000/v1/responses"
        );
        assert_eq!(
            responses_url(&Some("http://localhost:4000/v1".to_string())),
            "http://localhost:4000/v1/responses"
        );
        assert_eq!(
            responses_url(&Some("http://localhost:4000/v1/".to_string())),
            "http://localhost:4000/v1/responses"
        );
    }

    #[test]
    fn for_distiller_builds_provider_with_base_url() {
        let p = OpenAiProvider::for_distiller(
            "gpt-5.4-mini",
            "sk-test",
            Some("http://localhost:4000/v1".to_string()),
            60,
        )
        .expect("build");
        assert_eq!(p.model_name(), "gpt-5.4-mini");
        assert_eq!(p.base_url.as_deref(), Some("http://localhost:4000/v1"));
    }

    #[test]
    fn request_maps_system_and_user_to_responses_body() {
        let request = ModelRequest {
            messages: vec![
                ModelMessage {
                    role: MessageRole::System,
                    content: vec![MessageContent::Text {
                        text: "Use strict JSON.".to_string(),
                    }],
                },
                ModelMessage::user_text("Distill the transcript."),
                ModelMessage::assistant_text("Previous assistant text."),
            ],
            tools: Vec::new(),
            tool_choice: crate::model::ToolChoice::None,
            max_output_tokens: 1024,
            temperature: 0.2,
            metadata: Value::Null,
        };

        let body = build_request_body("gpt-5.4-mini", &request);
        assert_eq!(body["model"], "gpt-5.4-mini");
        assert_eq!(body["instructions"], "Use strict JSON.");
        assert_eq!(body["max_output_tokens"], 1024);
        assert!(body["input"].as_str().unwrap().contains("user: Distill"));
        assert!(
            body["input"]
                .as_str()
                .unwrap()
                .contains("assistant: Previous")
        );
        assert!(body.get("temperature").is_none());
    }

    #[test]
    fn response_maps_output_text_and_usage() {
        let response = parse_response(
            r#"{
                "status": "completed",
                "output": [
                    {
                        "type": "message",
                        "role": "assistant",
                        "content": [
                            {"type": "output_text", "text": "[{\"lesson\":\"Pin the linker\"}]"}
                        ]
                    }
                ],
                "usage": {
                    "input_tokens": 10,
                    "input_tokens_details": {"cached_tokens": 3},
                    "output_tokens": 5,
                    "total_tokens": 15
                }
            }"#,
        )
        .expect("parse response");

        assert_eq!(
            response.text.as_deref(),
            Some("[{\"lesson\":\"Pin the linker\"}]")
        );
        assert!(matches!(response.stop_reason, StopReason::EndTurn));
        assert_eq!(response.usage.input_tokens, 10);
        assert_eq!(response.usage.output_tokens, 5);
        assert_eq!(response.usage.cache_read_input_tokens, 3);
    }

    #[test]
    fn response_maps_top_level_output_text() {
        let response = parse_response(
            r#"{
                "status": "completed",
                "output_text": "[]",
                "usage": {"input_tokens": 1, "output_tokens": 1}
            }"#,
        )
        .expect("parse response");

        assert_eq!(response.text.as_deref(), Some("[]"));
        assert!(matches!(response.stop_reason, StopReason::EndTurn));
    }

    #[test]
    fn response_maps_incomplete_max_tokens() {
        let response = parse_response(
            r#"{
                "status": "incomplete",
                "incomplete_details": {"reason": "max_output_tokens"},
                "output": []
            }"#,
        )
        .expect("parse response");

        assert!(matches!(response.stop_reason, StopReason::MaxTokens));
    }
}
