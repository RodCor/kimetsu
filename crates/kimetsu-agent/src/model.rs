use std::collections::VecDeque;

use kimetsu_core::KimetsuResult;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRequest {
    pub messages: Vec<ModelMessage>,
    pub tools: Vec<ToolDefinition>,
    pub tool_choice: ToolChoice,
    pub max_output_tokens: u32,
    pub temperature: f32,
    pub metadata: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelMessage {
    pub role: MessageRole,
    pub content: Vec<MessageContent>,
}

impl ModelMessage {
    pub fn user_text(text: impl Into<String>) -> Self {
        Self {
            role: MessageRole::User,
            content: vec![MessageContent::Text { text: text.into() }],
        }
    }

    pub fn assistant_text(text: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Assistant,
            content: vec![MessageContent::Text { text: text.into() }],
        }
    }

    pub fn assistant_tool_calls(tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: MessageRole::Assistant,
            content: tool_calls
                .into_iter()
                .map(|call| MessageContent::ToolCall {
                    id: call.id,
                    name: call.name,
                    input: call.input,
                })
                .collect(),
        }
    }

    pub fn tool_result(
        tool_call_id: impl Into<String>,
        name: impl Into<String>,
        output: Value,
    ) -> Self {
        Self {
            role: MessageRole::Tool,
            content: vec![MessageContent::ToolResult {
                tool_call_id: tool_call_id.into(),
                name: name.into(),
                output,
            }],
        }
    }

    pub fn text_content(&self) -> String {
        self.content
            .iter()
            .filter_map(|content| match content {
                MessageContent::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessageContent {
    Text {
        text: String,
    },
    ToolCall {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_call_id: String,
        name: String,
        output: Value,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
    Auto,
    None,
    Required,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelResponse {
    pub text: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub stop_reason: StopReason,
    pub usage: TokenUsage,
}

impl ModelResponse {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: Some(text.into()),
            tool_calls: Vec::new(),
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        }
    }

    pub fn tool_call(id: impl Into<String>, name: impl Into<String>, input: Value) -> Self {
        Self {
            text: None,
            tool_calls: vec![ToolCall {
                id: id.into(),
                name: name.into(),
                input,
            }],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    Refusal,
    Error,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cost_usd: f32,
    /// v0.3.4a: Anthropic prompt-caching write counter. Tokens whose
    /// ephemeral cache entry was created during this request. High
    /// cache_creation alongside low cache_read means we're paying full
    /// freight on every call — see the per-call TempCommandDir + missing
    /// `--continue` story documented in V0.3-PLAN.md. Defaulted to 0 for
    /// providers that don't surface it (so existing tests keep passing).
    #[serde(default)]
    pub cache_creation_input_tokens: u32,
    /// v0.3.4a: Anthropic prompt-caching read counter. Tokens charged at
    /// the discounted "cache hit" rate because they matched an ephemeral
    /// cache entry from a recent prior request. This is the number we
    /// want to grow over a chat session; if it stays at 0 across turns
    /// the prompt cache is not landing.
    #[serde(default)]
    pub cache_read_input_tokens: u32,
}

pub trait ModelProvider {
    fn complete(&mut self, request: ModelRequest) -> KimetsuResult<ModelResponse>;

    fn estimate_tokens(&self, text: &str) -> u32 {
        ((text.split_whitespace().count() as f32) * 1.33).ceil() as u32
    }
}

impl ModelProvider for Box<dyn ModelProvider> {
    fn complete(&mut self, request: ModelRequest) -> KimetsuResult<ModelResponse> {
        (**self).complete(request)
    }

    fn estimate_tokens(&self, text: &str) -> u32 {
        (**self).estimate_tokens(text)
    }
}

#[derive(Debug, Default)]
pub struct MockProvider {
    responses: VecDeque<ModelResponse>,
    pub requests: Vec<ModelRequest>,
}

impl MockProvider {
    pub fn new(responses: impl IntoIterator<Item = ModelResponse>) -> Self {
        Self {
            responses: responses.into_iter().collect(),
            requests: Vec::new(),
        }
    }
}

impl ModelProvider for MockProvider {
    fn complete(&mut self, request: ModelRequest) -> KimetsuResult<ModelResponse> {
        self.requests.push(request);
        self.responses
            .pop_front()
            .ok_or_else(|| "mock provider has no response queued".into())
    }
}

pub fn default_tool_definitions() -> Vec<ToolDefinition> {
    vec![
        tool("read_file", "Read a UTF-8 text file inside the repo."),
        tool("list_files", "List files inside the repo."),
        tool("search_files", "Search repo files by regex."),
        tool("git_status", "Read git status."),
        tool("git_diff", "Read git diff."),
        tool("shell_command", "Run a policy-checked direct command."),
        tool(
            "apply_patch",
            "Apply whole-file repo changes under the active patch plan.",
        ),
    ]
}

fn tool(name: &str, description: &str) -> ToolDefinition {
    ToolDefinition {
        name: name.to_string(),
        description: description.to_string(),
        input_schema: serde_json::json!({ "type": "object" }),
    }
}
