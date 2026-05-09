use std::path::Path;
use std::time::Duration;

use kimetsu_core::KimetsuResult;
use kimetsu_core::config::ProjectConfig;
use kimetsu_core::env_file::resolve_env_value;
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
    api_key: String,
    model: String,
}

impl AnthropicProvider {
    pub fn from_config(repo_root: &Path, config: &ProjectConfig) -> KimetsuResult<Option<Self>> {
        if config.model.provider != "anthropic" {
            return Err(format!("unsupported model provider: {}", config.model.provider).into());
        }

        let Some(api_key) = resolve_env_value(repo_root, &config.model.api_key_env) else {
            return Ok(None);
        };

        let client = Client::builder()
            .timeout(Duration::from_secs(config.model.request_timeout_secs))
            .build()?;

        Ok(Some(Self {
            client,
            api_key,
            model: config.model.model.clone(),
        }))
    }

    pub fn model_name(&self) -> &str {
        &self.model
    }
}

impl ModelProvider for AnthropicProvider {
    fn complete(&mut self, request: ModelRequest) -> KimetsuResult<ModelResponse> {
        let body = build_request_body(&self.model, &request);
        let response = self
            .client
            .post(MESSAGES_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()?;

        let status = response.status();
        let response_text = response.text()?;
        if !status.is_success() {
            return Err(format!(
                "anthropic request failed ({status}): {}",
                response_error_summary(&response_text)
            )
            .into());
        }

        parse_response(&response_text)
    }
}

fn build_request_body(model: &str, request: &ModelRequest) -> Value {
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
        "model": model,
        "max_tokens": request.max_output_tokens,
        "temperature": request.temperature,
        "messages": messages,
    });

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

fn parse_response(response_text: &str) -> KimetsuResult<ModelResponse> {
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
}

impl From<AnthropicUsage> for TokenUsage {
    fn from(value: AnthropicUsage) -> Self {
        Self {
            input_tokens: value.input_tokens,
            output_tokens: value.output_tokens,
            cost_usd: 0.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ModelMessage, ToolDefinition};

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
        let response = parse_response(
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
