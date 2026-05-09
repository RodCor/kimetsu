use kimetsu_core::KimetsuResult;
use kimetsu_core::event::Event;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::model::{
    ModelMessage, ModelProvider, ModelRequest, ModelResponse, StopReason, TokenUsage, ToolChoice,
    ToolDefinition, default_tool_definitions,
};
use crate::tools::ToolRuntime;

#[derive(Debug, Clone)]
pub struct AgentLoopConfig {
    pub stage: String,
    pub tools: Vec<ToolDefinition>,
    pub max_model_turns: u32,
    pub max_tool_calls: u32,
    pub max_cost_usd: f32,
    pub max_output_tokens: u32,
    pub temperature: f32,
}

impl Default for AgentLoopConfig {
    fn default() -> Self {
        Self {
            stage: "tool_loop".to_string(),
            tools: default_tool_definitions(),
            max_model_turns: 30,
            max_tool_calls: 60,
            max_cost_usd: 5.0,
            max_output_tokens: 4096,
            temperature: 0.2,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentLoopOutcome {
    pub final_text: String,
    pub messages: Vec<ModelMessage>,
    pub model_turns: u32,
    pub tool_calls: u32,
    pub usage: TokenUsage,
}

pub struct AgentLoop<P: ModelProvider> {
    provider: P,
    runtime: ToolRuntime,
    config: AgentLoopConfig,
}

impl<P: ModelProvider> AgentLoop<P> {
    pub fn new(provider: P, runtime: ToolRuntime, config: AgentLoopConfig) -> Self {
        Self {
            provider,
            runtime,
            config,
        }
    }

    pub fn run(&mut self, mut messages: Vec<ModelMessage>) -> KimetsuResult<AgentLoopOutcome> {
        let mut usage = TokenUsage::default();
        let mut model_turns = 0u32;
        let mut tool_calls = 0u32;

        loop {
            if model_turns >= self.config.max_model_turns {
                return Err("budget exceeded: model turns".into());
            }

            let request = ModelRequest {
                messages: messages.clone(),
                tools: self.config.tools.clone(),
                tool_choice: ToolChoice::Auto,
                max_output_tokens: self.config.max_output_tokens,
                temperature: self.config.temperature,
                metadata: serde_json::json!({
                    "stage": self.config.stage,
                    "turn": model_turns + 1,
                }),
            };

            self.write_model_requested(&request)?;
            let response = self.provider.complete(request)?;
            model_turns += 1;
            usage.input_tokens = usage
                .input_tokens
                .saturating_add(response.usage.input_tokens);
            usage.output_tokens = usage
                .output_tokens
                .saturating_add(response.usage.output_tokens);
            usage.cost_usd += response.usage.cost_usd;
            self.write_model_responded(&response)?;

            if usage.cost_usd > self.config.max_cost_usd {
                return Err("budget exceeded: cost".into());
            }

            if !response.tool_calls.is_empty() {
                let calls = response.tool_calls.clone();
                messages.push(ModelMessage::assistant_tool_calls(calls.clone()));
                for call in calls {
                    if tool_calls >= self.config.max_tool_calls {
                        return Err("budget exceeded: tool calls".into());
                    }
                    let result = self.runtime.call_tool(&call.name, call.input.clone());
                    tool_calls += 1;
                    let output = match result {
                        Ok(output) => serde_json::json!({
                            "ok": true,
                            "output": output,
                        }),
                        Err(err) => serde_json::json!({
                            "ok": false,
                            "error": err.to_string(),
                        }),
                    };
                    messages.push(ModelMessage::tool_result(call.id, call.name, output));
                }
                continue;
            }

            let final_text = response.text.unwrap_or_default();
            messages.push(ModelMessage::assistant_text(final_text.clone()));
            if matches!(response.stop_reason, StopReason::MaxTokens) {
                return Err("model stopped because max tokens were reached".into());
            }

            return Ok(AgentLoopOutcome {
                final_text,
                messages,
                model_turns,
                tool_calls,
                usage,
            });
        }
    }

    pub fn into_runtime(self) -> ToolRuntime {
        self.runtime
    }

    fn write_model_requested(&mut self, request: &ModelRequest) -> KimetsuResult<()> {
        let event = Event::new(
            self.runtime.run_id(),
            "model.requested",
            serde_json::json!({
                "stage": self.config.stage,
                "provider": "mock-or-adapter",
                "model": "kimetsu-owned-request",
                "request_artifact": null,
                "tool_names": request.tools.iter().map(|tool| tool.name.clone()).collect::<Vec<_>>(),
                "estimated_input_tokens": estimate_request_tokens(request),
            }),
        );
        let artifact = format!("{}.model_request.json", event.event_id);
        let bytes = serde_json::to_vec_pretty(request)?;
        let artifact_ref = self.runtime.write_artifact(&artifact, &bytes)?;
        let mut event = event;
        event.payload["request_artifact"] = artifact_ref.into();
        self.runtime.append_trace_event(&event, true)
    }

    fn write_model_responded(&mut self, response: &ModelResponse) -> KimetsuResult<()> {
        let event = Event::new(
            self.runtime.run_id(),
            "model.responded",
            serde_json::json!({
                "response_artifact": null,
                "stop_reason": response.stop_reason,
                "usage": response.usage,
            }),
        );
        let artifact = format!("{}.model_response.json", event.event_id);
        let bytes = serde_json::to_vec_pretty(response)?;
        let artifact_ref = self.runtime.write_artifact(&artifact, &bytes)?;
        let mut event = event;
        event.payload["response_artifact"] = artifact_ref.into();
        self.runtime.append_trace_event(&event, true)
    }
}

pub fn parse_structured_json<T: DeserializeOwned>(text: &str) -> KimetsuResult<T> {
    let json = extract_json_object(text).ok_or("structured output missing JSON object")?;
    Ok(serde_json::from_str(json)?)
}

fn extract_json_object(text: &str) -> Option<&str> {
    let trimmed = text.trim();
    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        return Some(trimmed);
    }

    let start = trimmed.find('{')?;
    let end = trimmed.rfind('}')?;
    if start >= end {
        return None;
    }
    Some(&trimmed[start..=end])
}

fn estimate_request_tokens(request: &ModelRequest) -> u32 {
    let text = serde_json::to_string(request).unwrap_or_default();
    ((text.split_whitespace().count() as f32) * 1.33).ceil() as u32
}

#[cfg(test)]
mod tests {
    use std::fs;

    use kimetsu_brain::project::init_project;
    use kimetsu_brain::trace::{TraceWriter, read_trace};
    use kimetsu_core::ids::{RunId, new_id};
    use kimetsu_core::paths::ProjectPaths;
    use serde::Deserialize;

    use super::*;
    use crate::model::{MockProvider, ModelResponse, ToolCall};
    use crate::tools::{PatchChange, PatchOp, ToolPatchPlan, ToolRuntime};

    #[test]
    fn loop_executes_tool_calls_and_writes_model_artifacts() {
        let root = temp_project();
        fs::write(root.join("note.txt"), "kimetsu brain\n").expect("write note");
        init_project(&root, false).expect("init project");
        let paths = ProjectPaths::discover(&root).expect("paths");
        let run_id = RunId::new();
        let (writer, run_paths) = TraceWriter::create(&paths, run_id).expect("trace writer");
        let runtime = ToolRuntime::new(&root, run_id)
            .expect("runtime")
            .with_trace(writer, run_paths.clone());
        let provider = MockProvider::new([
            ModelResponse {
                text: None,
                tool_calls: vec![ToolCall {
                    id: "call_1".to_string(),
                    name: "read_file".to_string(),
                    input: serde_json::json!({ "path": "note.txt", "max_bytes": 1000 }),
                }],
                stop_reason: StopReason::ToolUse,
                usage: TokenUsage {
                    input_tokens: 10,
                    output_tokens: 5,
                    cost_usd: 0.01,
                },
            },
            ModelResponse::text(r#"{"status":"ok"}"#),
        ]);
        let mut loop_runner = AgentLoop::new(provider, runtime, AgentLoopConfig::default());
        let outcome = loop_runner
            .run(vec![ModelMessage::user_text("Read note.txt")])
            .expect("agent loop");

        assert_eq!(outcome.tool_calls, 1);
        assert_eq!(outcome.model_turns, 2);
        assert_eq!(outcome.final_text, r#"{"status":"ok"}"#);

        let events = read_trace(&run_paths.trace_jsonl).expect("trace");
        assert!(events.iter().any(|event| event.kind == "model.requested"));
        assert!(events.iter().any(|event| event.kind == "model.responded"));
        assert!(events.iter().any(|event| event.kind == "tool.called"));
        assert!(events.iter().any(|event| event.kind == "tool.completed"));
        assert!(
            fs::read_dir(&run_paths.artifacts_dir)
                .expect("artifacts")
                .filter_map(Result::ok)
                .any(|entry| entry
                    .file_name()
                    .to_string_lossy()
                    .contains("model_request"))
        );

        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn structured_json_parser_extracts_object_from_text() {
        #[derive(Debug, Deserialize)]
        struct Output {
            status: String,
        }

        let output: Output = parse_structured_json("result:\n```json\n{\"status\":\"ok\"}\n```")
            .expect("parse structured json");
        assert_eq!(output.status, "ok");
    }

    #[test]
    fn loop_enforces_tool_budget() {
        let root = temp_project();
        fs::write(root.join("note.txt"), "kimetsu brain\n").expect("write note");
        init_project(&root, false).expect("init project");
        let run_id = RunId::new();
        let mut runtime = ToolRuntime::new(&root, run_id).expect("runtime");
        runtime.set_patch_plan(ToolPatchPlan::default());
        let provider = MockProvider::new([ModelResponse::tool_call(
            "call_1",
            "read_file",
            serde_json::json!({ "path": "note.txt" }),
        )]);
        let mut loop_runner = AgentLoop::new(
            provider,
            runtime,
            AgentLoopConfig {
                max_tool_calls: 0,
                ..AgentLoopConfig::default()
            },
        );

        let result = loop_runner.run(vec![ModelMessage::user_text("Read note.txt")]);
        assert!(result.is_err());

        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn loop_applies_patch_under_active_plan() {
        let root = temp_project();
        fs::write(root.join("src.txt"), "hello\nworld\n").expect("write source");
        init_project(&root, false).expect("init project");
        let paths = ProjectPaths::discover(&root).expect("paths");
        let run_id = RunId::new();
        let (writer, run_paths) = TraceWriter::create(&paths, run_id).expect("trace writer");
        let mut runtime = ToolRuntime::new(&root, run_id)
            .expect("runtime")
            .with_stage("implementation")
            .with_trace(writer, run_paths.clone());
        runtime.set_patch_plan(ToolPatchPlan::allow_modify(["src.txt"]));

        let expected_hash = blake3::hash(b"hello\nworld\n").to_hex().to_string();
        let provider = MockProvider::new([
            ModelResponse::tool_call(
                "call_read",
                "read_file",
                serde_json::json!({ "path": "src.txt" }),
            ),
            ModelResponse::tool_call(
                "call_patch",
                "apply_patch",
                serde_json::to_value(crate::tools::ApplyPatchInput {
                    changes: vec![PatchChange {
                        path: "src.txt".to_string(),
                        op: PatchOp::Modify,
                        content: Some("hello\nkimetsu\n".to_string()),
                        expected_hash: Some(expected_hash),
                    }],
                })
                .expect("patch input"),
            ),
            ModelResponse::text("patched src.txt"),
        ]);
        let mut loop_runner = AgentLoop::new(provider, runtime, AgentLoopConfig::default());
        let outcome = loop_runner
            .run(vec![ModelMessage::user_text("Patch src.txt")])
            .expect("agent loop");

        assert_eq!(outcome.tool_calls, 2);
        assert_eq!(
            fs::read_to_string(root.join("src.txt")).expect("read patched"),
            "hello\nkimetsu\n"
        );
        let events = read_trace(&run_paths.trace_jsonl).expect("trace");
        assert!(
            events.iter().any(|event| {
                event.kind == "tool.called"
                    && event.payload.get("stage").and_then(|value| value.as_str())
                        == Some("implementation")
            }),
            "tool events should carry implementation stage"
        );

        fs::remove_dir_all(root).expect("cleanup");
    }

    fn temp_project() -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("kimetsu-agent-loop-test-{}", new_id()));
        fs::create_dir_all(&root).expect("create temp root");
        root
    }
}
