//! Credentialed SessionEnd distiller: at session end, ask a cheap
//! configured model to distill the transcript into 0-3 generalizable
//! lessons and record them via the confidence-gated brain memory API.
//! Best-effort throughout — a hook must never break session shutdown.

use std::io::{BufRead, Read};
use std::path::Path;

use kimetsu_agent::anthropic::AnthropicProvider;
use kimetsu_agent::model::{
    MessageContent, MessageRole, ModelMessage, ModelProvider, ModelRequest, ToolChoice,
};
use kimetsu_brain::project;
use kimetsu_core::env_file::resolve_env_value;
use kimetsu_core::memory::{MemoryKind, MemoryScope};
use kimetsu_core::paths::ProjectPaths;
use serde::Deserialize;

/// Max characters of transcript view fed to the distiller (keeps the
/// model call cheap and bounded).
const MAX_VIEW_CHARS: usize = 12_000;

const DISTILL_SYSTEM: &str = "You are Kimetsu's memory distiller. From the session transcript, extract durable, \
generalizable lessons worth remembering across future sessions — favoring non-obvious fixes for \
commands/tools that failed and were resolved, hard-won environment quirks, and confirmed conventions or \
anti-patterns. Ignore trivia, one-liners, and anything specific to a single throwaway value.\n\n\
Reply with ONLY a JSON array (no prose, no markdown) of at most 3 objects:\n\
[{\"lesson\": \"concrete, actionable, generalized\", \"tags\": [\"2-5\", \"domain\", \"tags\"], \
\"kind\": \"semantic_operator|anti_pattern|convention\", \"confidence\": 0.0-1.0}]\n\
Use confidence 0.8 when you're sure it generalizes, lower when unsure. If nothing qualifies, reply [].";

#[derive(Debug, Deserialize, PartialEq)]
pub struct Lesson {
    pub lesson: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default = "default_kind")]
    pub kind: String,
    #[serde(default = "default_confidence")]
    pub confidence: f32,
}

fn default_kind() -> String {
    "semantic_operator".to_string()
}
fn default_confidence() -> f32 {
    0.7
}

/// Extract the first JSON array from `text` and parse it into lessons.
/// Tolerant: returns empty on any parse failure; drops empty lessons;
/// caps at 3.
pub fn parse_lessons(text: &str) -> Vec<Lesson> {
    let (Some(start), Some(end)) = (text.find('['), text.rfind(']')) else {
        return Vec::new();
    };
    if end < start {
        return Vec::new();
    }
    serde_json::from_str::<Vec<Lesson>>(&text[start..=end])
        .unwrap_or_default()
        .into_iter()
        .filter(|l| !l.lesson.trim().is_empty())
        .take(3)
        .collect()
}

/// Ask the model to distill lessons from a transcript view. Returns empty
/// on any model/parse error.
pub fn distill_lessons(transcript_view: &str, provider: &mut dyn ModelProvider) -> Vec<Lesson> {
    let request = ModelRequest {
        messages: vec![
            ModelMessage {
                role: MessageRole::System,
                content: vec![MessageContent::Text {
                    text: DISTILL_SYSTEM.to_string(),
                }],
            },
            ModelMessage::user_text(transcript_view),
        ],
        tools: Vec::new(),
        tool_choice: ToolChoice::None,
        max_output_tokens: 1024,
        temperature: 0.2,
        metadata: serde_json::Value::Null,
    };
    match provider.complete(request) {
        Ok(response) => parse_lessons(response.text.as_deref().unwrap_or("")),
        Err(_) => Vec::new(),
    }
}

/// Stream a transcript JSONL into a compact, character-bounded view of the
/// user/assistant text (most-recent tail kept). Best-effort.
pub fn build_transcript_view(path: &str, max_chars: usize) -> String {
    let Ok(file) = std::fs::File::open(path) else {
        return String::new();
    };
    let mut out = String::new();
    for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
        let line = line.trim().trim_start_matches('\u{feff}');
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if let Some(snippet) = extract_snippet(&value) {
            out.push_str(&snippet);
            out.push('\n');
            if out.chars().count() > max_chars * 2 {
                out = tail_chars(&out, max_chars);
            }
        }
    }
    tail_chars(&out, max_chars)
}

/// Pull a `role: text` snippet from one transcript message (text content
/// blocks only; tool blocks are noted briefly). Returns None when there's
/// no human-readable text.
fn extract_snippet(value: &serde_json::Value) -> Option<String> {
    let message = value.get("message").unwrap_or(value);
    let role = message
        .get("role")
        .and_then(|r| r.as_str())
        .or_else(|| value.get("type").and_then(|t| t.as_str()))
        .unwrap_or("msg");
    let content = message.get("content").or_else(|| value.get("content"))?;
    let mut parts = Vec::new();
    if let Some(text) = content.as_str() {
        parts.push(text.to_string());
    } else if let Some(blocks) = content.as_array() {
        for block in blocks {
            if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                parts.push(text.to_string());
            } else if let Some(name) = block.get("name").and_then(|n| n.as_str()) {
                parts.push(format!("[tool {name}]"));
            }
        }
    }
    let body = parts.join(" ").trim().to_string();
    if body.is_empty() {
        None
    } else {
        Some(format!("{role}: {body}"))
    }
}

/// Char-boundary-safe tail of at most `n` chars.
fn tail_chars(s: &str, n: usize) -> String {
    let count = s.chars().count();
    if count <= n {
        s.to_string()
    } else {
        s.chars().skip(count - n).collect()
    }
}

/// Distill lessons from `view` and record each via the confidence-gated
/// brain API. Returns the count recorded.
pub fn distill_and_record(start: &Path, view: &str, provider: &mut dyn ModelProvider) -> usize {
    let mut recorded = 0;
    for lesson in distill_lessons(view, provider) {
        let kind = match lesson.kind.as_str() {
            "anti_pattern" => MemoryKind::FailurePattern,
            "convention" => MemoryKind::Convention,
            _ => MemoryKind::Fact,
        };
        let confidence = lesson.confidence.clamp(0.0, 1.0);
        if project::propose_or_merge_memory(
            start,
            MemoryScope::Project,
            kind,
            lesson.lesson.trim(),
            confidence,
            "auto-harvested at session end",
        )
        .is_ok()
        {
            recorded += 1;
        }
    }
    recorded
}

/// `kimetsu brain session-end-hook` entry. Reads the SessionEnd payload
/// from stdin, and if the distiller is enabled + credentialed, distills
/// the transcript and records lessons. Silent no-op otherwise.
pub fn run_session_end_hook(workspace: &Path) {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).ok();
    let payload: serde_json::Value =
        serde_json::from_str(input.trim()).unwrap_or(serde_json::Value::Null);

    let Ok(paths) = ProjectPaths::discover(workspace) else {
        return;
    };
    let Ok(config) = project::load_config(&paths) else {
        return;
    };
    let distiller = &config.learning.distiller;
    if !distiller.enabled || distiller.provider != "anthropic" {
        return;
    }
    let Some(api_key) = resolve_env_value(&paths.repo_root, &distiller.api_key_env) else {
        return;
    };
    let base_url = resolve_env_value(&paths.repo_root, &distiller.base_url_env);
    let Some(transcript_path) = payload
        .get("transcript_path")
        .and_then(|v| v.as_str())
        .filter(|p| !p.trim().is_empty())
    else {
        return;
    };
    let view = build_transcript_view(transcript_path, MAX_VIEW_CHARS);
    if view.trim().is_empty() {
        return;
    }
    let Ok(mut provider) = AnthropicProvider::for_distiller(
        &distiller.model,
        api_key,
        base_url,
        config.model.request_timeout_secs,
    ) else {
        return;
    };
    let recorded = distill_and_record(&paths.repo_root, &view, &mut provider);
    if recorded > 0 {
        println!(
            "[Kimetsu] distilled {recorded} lesson{} at session end.",
            if recorded == 1 { "" } else { "s" }
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kimetsu_agent::model::{MockProvider, ModelResponse, StopReason, TokenUsage};

    fn text_response(text: &str) -> ModelResponse {
        ModelResponse {
            text: Some(text.to_string()),
            tool_calls: Vec::new(),
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        }
    }

    #[test]
    fn parse_lessons_extracts_array_and_defaults() {
        let lessons = parse_lessons(
            "Sure! Here you go:\n[{\"lesson\":\"Use X not Y\",\"tags\":[\"a\"]}, \
             {\"lesson\":\"  \"}]\nthanks",
        );
        assert_eq!(lessons.len(), 1, "blank lesson dropped");
        assert_eq!(lessons[0].lesson, "Use X not Y");
        assert_eq!(lessons[0].kind, "semantic_operator");
        assert_eq!(lessons[0].confidence, 0.7);
    }

    #[test]
    fn parse_lessons_tolerates_garbage() {
        assert!(parse_lessons("no json here").is_empty());
        assert!(parse_lessons("[not valid json}").is_empty());
        assert!(parse_lessons("[]").is_empty());
    }

    #[test]
    fn distill_lessons_uses_model_text() {
        let mut provider = MockProvider::new([text_response(
            "[{\"lesson\":\"Pin the linker\",\"tags\":[\"rust\",\"windows\"],\"kind\":\"convention\",\"confidence\":0.9}]",
        )]);
        let lessons = distill_lessons("user: it failed\nuser: fixed it", &mut provider);
        assert_eq!(lessons.len(), 1);
        assert_eq!(lessons[0].kind, "convention");
        assert_eq!(provider.requests.len(), 1);
        assert_eq!(provider.requests[0].messages.len(), 2);
    }

    #[test]
    fn distill_lessons_empty_on_model_error() {
        let mut provider = MockProvider::new([]);
        assert!(distill_lessons("anything", &mut provider).is_empty());
    }

    #[test]
    fn build_transcript_view_streams_text_and_bounds() {
        let dir = std::env::temp_dir().join(format!(
            "kimetsu_view_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        std::fs::write(
            &path,
            "\u{feff}{\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"hello\"}]}}\n\
             {\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"name\":\"Bash\"}]}}\n\
             garbage line\n\
             {\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"bye\"}]}}\n",
        )
        .unwrap();

        let view = build_transcript_view(path.to_str().unwrap(), 10_000);
        assert!(view.contains("user: hello"));
        assert!(view.contains("assistant: [tool Bash]"));
        assert!(view.contains("assistant: bye"));

        let tiny = build_transcript_view(path.to_str().unwrap(), 5);
        assert!(tiny.chars().count() <= 5);

        assert!(build_transcript_view("/no/such.jsonl", 100).is_empty());

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn distill_and_record_writes_to_a_temp_brain() {
        let root = std::env::temp_dir().join(format!(
            "kimetsu_distill_brain_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        // git_init_boundary makes this dir its own git toplevel so
        // ProjectPaths::discover() resolves to `root` itself instead of
        // climbing to the enclosing Kimetsu dev repo.
        kimetsu_core::paths::git_init_boundary(&root);
        // SAFETY: single-threaded test setup; isolate from any user brain.
        unsafe {
            std::env::set_var("KIMETSU_USER_BRAIN", "0");
        }
        kimetsu_brain::project::init_project(&root, true).expect("init brain");

        let mut provider = MockProvider::new([text_response(
            "[{\"lesson\":\"Set USERPROFILE for global installs\",\"tags\":[\"cargo\",\"windows\"],\"confidence\":0.9}]",
        )]);
        let n = distill_and_record(&root, "user: a\nuser: b", &mut provider);
        assert_eq!(n, 1);
        let memories = kimetsu_brain::project::list_memories(&root).expect("list");
        assert!(
            memories.iter().any(|m| m.text.contains("USERPROFILE")),
            "distilled lesson was recorded"
        );

        std::fs::remove_dir_all(root).ok();
    }
}
