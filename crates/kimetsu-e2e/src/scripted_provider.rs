//! `ScriptedProvider` — pure-Rust builder for end-to-end agent tests.
//!
//! Wraps `kimetsu_agent::model::MockProvider` with an ergonomic builder
//! API so a test author can author multi-turn scenarios inline without
//! manually managing tool-call ids, default token usage, or stop
//! reasons.
//!
//! ```rust,ignore
//! use kimetsu_e2e::prelude::*;
//!
//! let mut provider = ScriptedProvider::new()
//!     .turn(|t| t.call_tool("read_file", json!({"path": "src/main.rs"})))
//!     .turn(|t| t.call_tool("cite_memory", json!({"memory_id": "m_xyz"})))
//!     .turn(|t| t.done("done"))
//!     .build();
//!
//! run_model_agent("the task", &mut runtime, &mut provider, opts, None);
//! ```
//!
//! Each call to `.turn(f)` appends one scripted response. The closure
//! receives a `TurnBuilder` that supports either `call_tool` (assistant
//! tool call) or `done` (assistant text + EndTurn stop reason).
//!
//! Tool-call ids are auto-generated as `call_1`, `call_2`, ... so the
//! test author doesn't have to thread them manually. Mismatch between
//! what the agent actually asks for vs. what was scripted is detected
//! by the surrounding test's assertions, not by ScriptedProvider — the
//! point of this layer is *response scripting*, not *call expectation*.

use kimetsu_agent::model::{MockProvider, ModelResponse, StopReason, TokenUsage, ToolCall};
use serde_json::Value;

/// Builder for a multi-turn scripted model.
pub struct ScriptedProvider {
    responses: Vec<ModelResponse>,
    next_call_id: usize,
}

impl ScriptedProvider {
    pub fn new() -> Self {
        Self {
            responses: Vec::new(),
            next_call_id: 1,
        }
    }

    /// Append one scripted turn. The closure receives a `TurnBuilder`
    /// that lets the test express either `call_tool(name, input)` or
    /// `done(text)`.
    pub fn turn<F>(mut self, build: F) -> Self
    where
        F: FnOnce(TurnBuilder) -> ModelResponse,
    {
        let builder = TurnBuilder {
            call_id: format!("call_{}", self.next_call_id),
        };
        self.next_call_id += 1;
        self.responses.push(build(builder));
        self
    }

    /// Finalize into a ready-to-use `MockProvider`. The agent loop will
    /// pop responses one per `complete()` call in script order.
    pub fn build(self) -> MockProvider {
        MockProvider::new(self.responses)
    }
}

impl Default for ScriptedProvider {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-turn helper. Construct a single `ModelResponse` from
/// caller-friendly inputs (no manual id management, no manual usage
/// struct).
pub struct TurnBuilder {
    call_id: String,
}

impl TurnBuilder {
    /// Emit one tool call this turn. Use this for the assistant's
    /// intermediate turns where it wants the runtime to do work.
    pub fn call_tool(self, name: &str, input: Value) -> ModelResponse {
        ModelResponse {
            text: None,
            tool_calls: vec![ToolCall {
                id: self.call_id,
                name: name.to_string(),
                input,
            }],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage::default(),
        }
    }

    /// Emit multiple tool calls in one turn (parallel-call shape).
    /// Each call gets its own auto-generated id (`<base>_a`, `<base>_b`, ...).
    pub fn call_tools(self, calls: Vec<(&str, Value)>) -> ModelResponse {
        let tool_calls = calls
            .into_iter()
            .enumerate()
            .map(|(i, (name, input))| {
                let suffix = (b'a' + (i as u8)) as char;
                ToolCall {
                    id: format!("{}_{suffix}", self.call_id),
                    name: name.to_string(),
                    input,
                }
            })
            .collect();
        ModelResponse {
            text: None,
            tool_calls,
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage::default(),
        }
    }

    /// Emit an assistant text response with `EndTurn` — terminates the
    /// agent loop. Every scripted scenario MUST end on a `done` turn
    /// or the loop will hang waiting for the next response.
    pub fn done(self, text: &str) -> ModelResponse {
        ModelResponse::text(text)
    }
}
