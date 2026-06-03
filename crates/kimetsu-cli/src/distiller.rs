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
use kimetsu_core::config::ProjectConfig;
use kimetsu_core::env_file::resolve_env_value;
use kimetsu_core::memory::{MemoryKind, MemoryScope};
use kimetsu_core::paths::{ProjectPaths, user_brain_enabled, user_kimetsu_dir};
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

/// Extract the first complete top-level JSON array from `text`, ignoring
/// brackets that appear inside JSON strings and any prose after the array.
/// Byte-scan is safe: the structural bytes (`"` `[` `]` `\`) are all ASCII,
/// and UTF-8 continuation bytes are >= 0x80 so they never match.
fn find_json_array(text: &str) -> Option<&str> {
    let start = text.find('[')?;
    let bytes = text.as_bytes();
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for i in start..bytes.len() {
        let b = bytes[i];
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
        } else {
            match b {
                b'"' => in_string = true,
                b'[' => depth += 1,
                b']' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(&text[start..=i]);
                    }
                }
                _ => {}
            }
        }
    }
    None
}

/// Extract the first JSON array from `text` and parse it into lessons.
/// Tolerant: returns empty on any parse failure; drops empty lessons;
/// caps at 3.
pub fn parse_lessons(text: &str) -> Vec<Lesson> {
    let Some(array) = find_json_array(text) else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<Lesson>>(array)
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
            if out.chars().count() > max_chars.saturating_mul(2) {
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

/// Distill lessons from `view` and record each. `Project` scope uses the
/// confidence-gated `propose_or_merge_memory` (workspace brain); `GlobalUser`
/// uses `add_memory`, which routes to `~/.kimetsu/brain.db` (the user brain
/// has no proposal queue, so this is add-or-dedup). Returns the count recorded.
/// For `GlobalUser`, `start` is ignored (the user brain is global).
pub fn distill_and_record(
    start: &Path,
    view: &str,
    provider: &mut dyn ModelProvider,
    scope: MemoryScope,
) -> usize {
    let mut recorded = 0;
    for lesson in distill_lessons(view, provider) {
        // Mirror kimetsu_brain_record's MCP kind mapping; semantic_operator + default store as Fact.
        let kind = match lesson.kind.as_str() {
            "anti_pattern" => MemoryKind::FailurePattern,
            "convention" => MemoryKind::Convention,
            _ => MemoryKind::Fact,
        };
        let text = lesson.lesson.trim();
        let ok = match scope {
            MemoryScope::GlobalUser => {
                project::add_memory(start, MemoryScope::GlobalUser, kind, text).is_ok()
            }
            _ => project::propose_or_merge_memory(
                start,
                scope,
                kind,
                text,
                lesson.confidence.clamp(0.0, 1.0),
                "auto-harvested at session end",
            )
            .is_ok(),
        };
        if ok {
            recorded += 1;
        }
    }
    recorded
}

/// The distiller selected for this session: which model/key/endpoint to use,
/// and how to record (project vs the global user brain).
pub struct ResolvedDistiller {
    pub model: String,
    pub key: String,
    pub base_url: Option<String>,
    pub timeout_secs: u64,
    pub scope: MemoryScope,
    pub record_start: std::path::PathBuf,
}

/// Resolve the distiller for `workspace`, preferring the workspace distiller
/// over the global one (`~/.kimetsu`). `None` when neither is enabled +
/// credentialed.
pub fn resolve_distiller(workspace: &Path) -> Option<ResolvedDistiller> {
    let global_dir = if user_brain_enabled() {
        user_kimetsu_dir()
    } else {
        None
    };
    resolve_distiller_with(workspace, global_dir)
}

/// Testable core: `global_dir` is injected (the `~/.kimetsu` dir, or `None`).
fn resolve_distiller_with(
    workspace: &Path,
    global_dir: Option<std::path::PathBuf>,
) -> Option<ResolvedDistiller> {
    // 1. Workspace distiller (Project scope).
    if let Ok(paths) = ProjectPaths::discover(workspace)
        && let Ok(config) = project::load_config(&paths)
    {
        let d = &config.learning.distiller;
        if d.enabled
            && d.provider == "anthropic"
            && let Some(key) = resolve_env_value(&paths.repo_root, &d.api_key_env)
        {
            return Some(ResolvedDistiller {
                model: d.model.clone(),
                key,
                base_url: resolve_env_value(&paths.repo_root, &d.base_url_env),
                timeout_secs: config.model.request_timeout_secs,
                scope: MemoryScope::Project,
                record_start: paths.repo_root.clone(),
            });
        }
    }
    // 2. Global distiller (GlobalUser scope).
    if let Some(dir) = global_dir
        && let Ok(text) = std::fs::read_to_string(dir.join("project.toml"))
        && let Ok(config) = ProjectConfig::from_toml(&text)
    {
        let d = &config.learning.distiller;
        if d.enabled
            && d.provider == "anthropic"
            && let Some(key) = resolve_env_value(&dir, &d.api_key_env)
        {
            return Some(ResolvedDistiller {
                model: d.model.clone(),
                key,
                base_url: resolve_env_value(&dir, &d.base_url_env),
                timeout_secs: config.model.request_timeout_secs,
                scope: MemoryScope::GlobalUser,
                record_start: workspace.to_path_buf(),
            });
        }
    }
    None
}

/// `kimetsu brain session-end-hook` entry. Reads the SessionEnd payload
/// from stdin, and if the distiller is enabled + credentialed, distills
/// the transcript and records lessons. Silent no-op otherwise.
pub fn run_session_end_hook(workspace: &Path) {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).ok();
    let payload: serde_json::Value =
        serde_json::from_str(input.trim()).unwrap_or(serde_json::Value::Null);

    let Some(transcript_path) = payload
        .get("transcript_path")
        .and_then(|v| v.as_str())
        .filter(|p| !p.trim().is_empty())
    else {
        return;
    };
    let Some(resolved) = resolve_distiller(workspace) else {
        return;
    };
    let view = build_transcript_view(transcript_path, MAX_VIEW_CHARS);
    if view.trim().is_empty() {
        return;
    }
    let Ok(mut provider) = AnthropicProvider::for_distiller(
        &resolved.model,
        resolved.key,
        resolved.base_url,
        resolved.timeout_secs,
    ) else {
        return;
    };
    let recorded = distill_and_record(&resolved.record_start, &view, &mut provider, resolved.scope);
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
    fn parse_lessons_ignores_trailing_prose_and_brackets() {
        let lessons = parse_lessons(
            "[{\"lesson\":\"Pin the linker\",\"confidence\":0.9}], also see [1] and [2].",
        );
        assert_eq!(lessons.len(), 1);
        assert_eq!(lessons[0].lesson, "Pin the linker");
    }

    #[test]
    fn parse_lessons_handles_brackets_inside_strings() {
        let lessons = parse_lessons("[{\"lesson\":\"use arr[0] not arr.first\"}]");
        assert_eq!(lessons.len(), 1);
        assert_eq!(lessons[0].lesson, "use arr[0] not arr.first");
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
        std::fs::create_dir_all(&root).unwrap();
        kimetsu_core::paths::git_init_boundary(&root);

        kimetsu_brain::user_brain::with_user_brain_disabled(|| {
            kimetsu_brain::project::init_project(&root, true).expect("init brain");
            let mut provider = MockProvider::new([text_response(
                "[{\"lesson\":\"Set USERPROFILE for global installs\",\"tags\":[\"cargo\",\"windows\"],\"confidence\":0.9}]",
            )]);
            let n = distill_and_record(
                &root,
                "user: a\nuser: b",
                &mut provider,
                MemoryScope::Project,
            );
            assert_eq!(n, 1);
            let memories = kimetsu_brain::project::list_memories(&root).expect("list");
            assert!(
                memories.iter().any(|m| m.text.contains("USERPROFILE")),
                "distilled lesson was recorded"
            );
        });

        std::fs::remove_dir_all(root).ok();
    }

    /// Run `f` with the user brain pointed at a temp dir (enabled), under
    /// the process-wide env lock, restoring the previous env afterward.
    fn with_user_brain_dir<R>(dir: &std::path::Path, f: impl FnOnce() -> R) -> R {
        let _g = kimetsu_brain::user_brain::test_env_lock()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let prev_dir = std::env::var("KIMETSU_USER_BRAIN_DIR").ok();
        let prev_en = std::env::var("KIMETSU_USER_BRAIN").ok();
        // SAFETY: scoped by the shared lock.
        unsafe {
            std::env::set_var("KIMETSU_USER_BRAIN_DIR", dir);
            std::env::remove_var("KIMETSU_USER_BRAIN");
        }
        let out = f();
        unsafe {
            match prev_dir {
                Some(v) => std::env::set_var("KIMETSU_USER_BRAIN_DIR", v),
                None => std::env::remove_var("KIMETSU_USER_BRAIN_DIR"),
            }
            match prev_en {
                Some(v) => std::env::set_var("KIMETSU_USER_BRAIN", v),
                None => std::env::remove_var("KIMETSU_USER_BRAIN"),
            }
        }
        out
    }

    /// Write a full `project.toml` to `dir` with the distiller section configured.
    /// Uses `ProjectConfig::default_for_project` + `to_toml()` because a partial
    /// TOML with only `[learning.distiller]` fails to parse — the `kimetsu` and
    /// `model` sections are required by serde (no `#[serde(default)]` on those
    /// `ProjectConfig` fields).
    fn write_distiller_toml(dir: &std::path::Path, enabled: bool, model: &str) {
        std::fs::create_dir_all(dir).unwrap();
        let mut config = ProjectConfig::default_for_project("test");
        config.learning.distiller.enabled = enabled;
        config.learning.distiller.provider = "anthropic".to_string();
        config.learning.distiller.model = model.to_string();
        config.learning.distiller.api_key_env = "ANTHROPIC_API_KEY".to_string();
        config.learning.distiller.base_url_env = "ANTHROPIC_BASE_URL".to_string();
        let toml = config.to_toml().unwrap();
        std::fs::write(dir.join("project.toml"), toml).unwrap();
    }

    #[test]
    fn resolve_distiller_global_when_no_workspace() {
        let ws = std::env::temp_dir().join(format!(
            "km_rd_ws_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        // Intentionally NOT a git repo: exercises discover's non-repo fallback
        // so the workspace tier finds no config and we fall through to global.
        std::fs::create_dir_all(&ws).unwrap();
        let gdir = std::env::temp_dir().join(format!(
            "km_rd_g_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        write_distiller_toml(&gdir, true, "claude-haiku-4-5");
        std::fs::write(gdir.join(".env"), "ANTHROPIC_API_KEY=sk-global\n").unwrap();

        let r = resolve_distiller_with(&ws, Some(gdir.clone())).expect("global resolved");
        assert_eq!(r.scope, MemoryScope::GlobalUser);
        assert_eq!(r.model, "claude-haiku-4-5");
        assert_eq!(r.key, "sk-global");

        write_distiller_toml(&gdir, false, "claude-haiku-4-5");
        assert!(resolve_distiller_with(&ws, Some(gdir.clone())).is_none());

        std::fs::remove_dir_all(ws).ok();
        std::fs::remove_dir_all(gdir).ok();
    }

    #[test]
    fn resolve_distiller_workspace_wins() {
        let ws = std::env::temp_dir().join(format!(
            "km_rd_wsw_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(ws.join(".kimetsu")).unwrap();
        assert!(
            kimetsu_core::paths::git_init_boundary(&ws),
            "git_init_boundary failed — git needed for workspace isolation"
        );
        // Use default_for_project + set distiller fields + to_toml() because
        // a partial toml with only [learning.distiller] fails to parse
        // (kimetsu/model sections are required by serde).
        {
            let mut config = ProjectConfig::default_for_project("ws");
            config.learning.distiller.enabled = true;
            config.learning.distiller.provider = "anthropic".to_string();
            config.learning.distiller.model = "ws-model".to_string();
            config.learning.distiller.api_key_env = "ANTHROPIC_API_KEY".to_string();
            config.learning.distiller.base_url_env = "ANTHROPIC_BASE_URL".to_string();
            let toml = config.to_toml().unwrap();
            std::fs::write(ws.join(".kimetsu").join("project.toml"), toml).unwrap();
        }
        std::fs::write(ws.join(".env"), "ANTHROPIC_API_KEY=sk-ws\n").unwrap();

        let gdir = std::env::temp_dir().join(format!(
            "km_rd_gw_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        write_distiller_toml(&gdir, true, "g-model");
        std::fs::write(gdir.join(".env"), "ANTHROPIC_API_KEY=sk-global\n").unwrap();

        let r = resolve_distiller_with(&ws, Some(gdir.clone())).expect("workspace resolved");
        assert_eq!(r.scope, MemoryScope::Project);
        assert_eq!(r.model, "ws-model");
        assert_eq!(r.key, "sk-ws");

        std::fs::remove_dir_all(ws).ok();
        std::fs::remove_dir_all(gdir).ok();
    }

    #[test]
    fn distill_and_record_global_writes_to_user_brain() {
        let dir = std::env::temp_dir().join(format!(
            "kimetsu_userbrain_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        with_user_brain_dir(&dir, || {
            let mut provider = MockProvider::new([text_response(
                "[{\"lesson\":\"Global lesson kept everywhere\",\"tags\":[\"x\"],\"confidence\":0.9}]",
            )]);
            // `start` is ignored on the GlobalUser path; pass the temp dir.
            let n = distill_and_record(&dir, "user: a", &mut provider, MemoryScope::GlobalUser);
            assert_eq!(n, 1);
            let conn = kimetsu_brain::user_brain::open_user_brain_readonly()
                .unwrap()
                .expect("user brain exists");
            let mems = kimetsu_brain::user_brain::list_user_memories(&conn).unwrap();
            assert!(
                mems.iter()
                    .any(|m| m.text.contains("Global lesson kept everywhere"))
            );
        });
        std::fs::remove_dir_all(dir).ok();
    }
}
