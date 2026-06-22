//! Credentialed SessionEnd distiller: at session end, ask a cheap
//! configured model to distill the transcript into 0-3 generalizable
//! lessons and record them via the confidence-gated brain memory API.
//! Best-effort throughout — a hook must never break session shutdown.

use std::io::{BufRead, Read};
use std::path::Path;

use kimetsu_agent::anthropic::AnthropicProvider;
use kimetsu_agent::bedrock::BedrockProvider;
use kimetsu_agent::model::{
    MessageContent, MessageRole, ModelMessage, ModelProvider, ModelRequest, ToolChoice,
};
use kimetsu_agent::openai::OpenAiProvider;
use kimetsu_brain::project;
use kimetsu_core::config::{CheapModelSection, ProjectConfig};
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
    pub provider: String,
    pub model: String,
    /// For Anthropic/OpenAI: the API key. For Bedrock: the AWS access key ID.
    pub key: String,
    pub base_url: Option<String>,
    pub timeout_secs: u64,
    pub scope: MemoryScope,
    pub record_start: std::path::PathBuf,
    /// Bedrock only: AWS secret access key.
    pub secret_key: Option<String>,
    /// Bedrock only: AWS session token (optional for long-term credentials).
    pub session_token: Option<String>,
    /// Bedrock only: resolved AWS region.
    pub region: Option<String>,
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
    // 1. Workspace cheap model (Project scope).
    // S1.2: use config.cheap_model() which resolves [cheap_model] → [learning.distiller]
    // in priority order, providing back-compat for existing configs.
    if let Ok(paths) = ProjectPaths::discover(workspace)
        && let Ok(config) = project::load_config(&paths)
    {
        if let Some(cm) = config.cheap_model() {
            if let Some(resolved) = resolve_from_cheap_model(
                &cm,
                &paths.repo_root,
                config.model.request_timeout_secs,
                MemoryScope::Project,
                paths.repo_root.clone(),
            ) {
                return Some(resolved);
            }
        }
    }
    // 2. Global cheap model (GlobalUser scope).
    if let Some(dir) = global_dir
        && let Ok(text) = std::fs::read_to_string(dir.join("project.toml"))
        && let Ok(config) = ProjectConfig::from_toml(&text)
    {
        if let Some(cm) = config.cheap_model() {
            if let Some(resolved) = resolve_from_cheap_model(
                &cm,
                &dir,
                config.model.request_timeout_secs,
                MemoryScope::GlobalUser,
                workspace.to_path_buf(),
            ) {
                return Some(resolved);
            }
        }
    }
    None
}

/// Attempt to build a `ResolvedDistiller` from a `CheapModelSection`.
/// Returns `None` when credentials are missing (e.g. the required env var
/// is not set) — callers try the next tier or return `None` (graceful
/// degradation, no panic).
fn resolve_from_cheap_model(
    cm: &CheapModelSection,
    env_root: &Path,
    timeout_secs: u64,
    scope: MemoryScope,
    record_start: std::path::PathBuf,
) -> Option<ResolvedDistiller> {
    let provider = normalize_distiller_provider(&cm.provider)?;
    match provider {
        "bedrock" => {
            let access_key = resolve_env_value(env_root, "AWS_ACCESS_KEY_ID");
            let secret_key = resolve_env_value(env_root, "AWS_SECRET_ACCESS_KEY");
            let session_token = resolve_env_value(env_root, "AWS_SESSION_TOKEN");
            let region = cm.region.clone().or_else(|| {
                resolve_env_value(env_root, &cm.region_env)
                    .or_else(|| resolve_env_value(env_root, "AWS_DEFAULT_REGION"))
            });
            if let (Some(ak), Some(sk), Some(rg)) = (access_key, secret_key, region) {
                Some(ResolvedDistiller {
                    provider: provider.to_string(),
                    model: cm.model.clone(),
                    key: ak,
                    base_url: None,
                    timeout_secs,
                    scope,
                    record_start,
                    secret_key: Some(sk),
                    session_token,
                    region: Some(rg),
                })
            } else {
                None
            }
        }
        // S1.1: Ollama uses the OpenAI-compatible request path; API key is
        // optional (empty string is fine). Base URL defaults to
        // http://localhost:11434/v1 when no env override is present.
        "ollama" => {
            let base_url = resolve_env_value(env_root, &cm.base_url_env)
                .filter(|u| !u.trim().is_empty())
                .unwrap_or_else(|| CheapModelSection::OLLAMA_DEFAULT_BASE_URL.to_string());
            // API key optional for Ollama — use an empty placeholder so the
            // OpenAI-compatible client path doesn't reject a missing key.
            let key = resolve_env_value(env_root, &cm.api_key_env).unwrap_or_default();
            Some(ResolvedDistiller {
                provider: provider.to_string(),
                model: cm.model.clone(),
                key,
                base_url: Some(base_url),
                timeout_secs,
                scope,
                record_start,
                secret_key: None,
                session_token: None,
                region: None,
            })
        }
        // anthropic / openai: require an API key from env.
        _ => {
            let key = resolve_env_value(env_root, &cm.api_key_env)?;
            Some(ResolvedDistiller {
                provider: provider.to_string(),
                model: cm.model.clone(),
                key,
                base_url: resolve_env_value(env_root, &cm.base_url_env),
                timeout_secs,
                scope,
                record_start,
                secret_key: None,
                session_token: None,
                region: None,
            })
        }
    }
}

fn normalize_distiller_provider(provider: &str) -> Option<&'static str> {
    match provider.trim().to_ascii_lowercase().as_str() {
        "anthropic" | "claude" => Some("anthropic"),
        "openai" | "oai" | "gpt" => Some("openai"),
        "bedrock" | "aws" => Some("bedrock"),
        // S1.1: Ollama exposes an OpenAI-compatible API at localhost:11434/v1;
        // no API key required.
        "ollama" => Some("ollama"),
        _ => None,
    }
}

/// `kimetsu brain session-end-hook` entry. Reads the SessionEnd payload
/// from stdin, and if the distiller is enabled + credentialed, distills
/// the transcript and records lessons. Silent no-op otherwise.
///
/// Also auto-captures a work episode (Story 1.3): whether or not a cheap
/// model is configured, an episode is written.  With a cheap model the
/// episode fields are model-distilled; without one, the rule-based fallback
/// assembles the episode from the transcript view.
pub fn run_session_end_hook(workspace: &Path) {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).ok();
    let payload: serde_json::Value =
        serde_json::from_str(input.trim()).unwrap_or(serde_json::Value::Null);

    let transcript_path = payload
        .get("transcript_path")
        .and_then(|v| v.as_str())
        .filter(|p| !p.trim().is_empty());

    if let Some(tp) = transcript_path {
        run_distiller_for_transcript(workspace, tp);
    }

    // Story 1.3: auto-capture episode at SessionEnd (best-effort, never fails
    // the hook).
    capture_episode_at_session_end(workspace, transcript_path.unwrap_or(""));
}

/// Capture a work episode at SessionEnd.  Tries the cheap model first;
/// degrades gracefully to the rule-based fallback if none is configured or
/// if the model call fails.  Best-effort — silently swallows all errors so
/// the session shutdown is never blocked.
pub fn capture_episode_at_session_end(workspace: &Path, transcript_path: &str) {
    capture_episode_now(workspace, transcript_path, "");
}

/// Capture an episode now (manual checkpoint or auto-capture).
///
/// `note` is an optional annotation from the user.
/// Returns `true` if the episode was written successfully.
pub fn capture_episode_now(workspace: &Path, transcript_path: &str, note: &str) -> bool {
    use kimetsu_brain::episode::{capture_episode, rule_based_episode};
    use kimetsu_core::paths::ProjectPaths;

    // Resolve the repo_root from the workspace.  If the project can't be
    // found this is a non-project dir and we skip silently.
    let repo_root = match ProjectPaths::discover(workspace) {
        Ok(p) => p.repo_root.to_string_lossy().to_string(),
        Err(_) => return false,
    };

    let view = if transcript_path.is_empty() {
        String::new()
    } else {
        build_transcript_view(transcript_path, MAX_VIEW_CHARS)
    };

    // Try cheap model first; fall back to rule-based.
    let episode_payload = if let Some(resolved) = resolve_distiller(workspace) {
        distill_episode_with_model(&view, &resolved, &repo_root, note)
            .unwrap_or_else(|| kimetsu_brain::episode::rule_based_episode(&view, &repo_root, note))
    } else {
        rule_based_episode(&view, &repo_root, note)
    };

    // Write the episode event.  Best-effort.
    match capture_episode(workspace, episode_payload) {
        Ok(_id) => true,
        Err(_) => false,
    }
}

/// Prompt the cheap model to distill an episode from the transcript view.
/// Returns `None` on any error (caller falls back to rule-based).
fn distill_episode_with_model(
    view: &str,
    resolved: &ResolvedDistiller,
    repo_root: &str,
    note: &str,
) -> Option<kimetsu_brain::episode::EpisodePayload> {
    const EPISODE_SYSTEM: &str = "You are Kimetsu's session recorder. From the transcript below, extract a concise \
        work episode in JSON with these EXACT keys (all strings/arrays of strings):\n\
        {\"task\": \"the main goal\", \"summary\": \"what was done\", \
        \"open_threads\": [\"what still needs doing\"], \
        \"dead_ends\": [\"what failed and why\"], \
        \"hypothesis\": \"current best theory or approach\"}\n\
        Be concrete. Keep each field to one sentence max. If a field is empty use \"\" or []. \
        Reply with ONLY the JSON object, no prose.";

    if view.trim().is_empty() {
        return None;
    }

    let mut provider = make_provider_for_resolved(resolved)?;

    let request = kimetsu_agent::model::ModelRequest {
        messages: vec![
            kimetsu_agent::model::ModelMessage {
                role: kimetsu_agent::model::MessageRole::System,
                content: vec![kimetsu_agent::model::MessageContent::Text {
                    text: EPISODE_SYSTEM.to_string(),
                }],
            },
            kimetsu_agent::model::ModelMessage::user_text(view),
        ],
        tools: Vec::new(),
        tool_choice: kimetsu_agent::model::ToolChoice::None,
        max_output_tokens: 512,
        temperature: 0.1,
        metadata: serde_json::Value::Null,
    };

    let response = provider.complete(request).ok()?;
    let text = response.text.as_deref().unwrap_or("");

    // Parse the JSON object from the model output.
    parse_episode_json(text, repo_root, note)
}

/// Extract and parse the episode JSON object from model output.
fn parse_episode_json(
    text: &str,
    repo_root: &str,
    note: &str,
) -> Option<kimetsu_brain::episode::EpisodePayload> {
    // Find the first `{...}` top-level object.
    let start = text.find('{')?;
    let bytes = text.as_bytes();
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    let mut end = None;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
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
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }
    }
    let json_str = &text[start..=end?];
    let val: serde_json::Value = serde_json::from_str(json_str).ok()?;

    let task = val
        .get("task")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let summary = val
        .get("summary")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let open_threads = val
        .get("open_threads")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str())
                .map(|s| s.to_string())
                .collect()
        })
        .unwrap_or_default();
    let dead_ends = val
        .get("dead_ends")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str())
                .map(|s| s.to_string())
                .collect()
        })
        .unwrap_or_default();
    let hypothesis = val
        .get("hypothesis")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    Some(kimetsu_brain::episode::EpisodePayload {
        task,
        summary,
        open_threads,
        dead_ends,
        hypothesis,
        note: note.to_string(),
        repo_root: repo_root.to_string(),
        memory_ids: Vec::new(),
    })
}

/// Construct a boxed `ModelProvider` from a `ResolvedDistiller`, consuming
/// the credential fields. Returns `None` when the provider variant is unknown
/// or credential construction fails.
pub fn make_provider_for_resolved(resolved: &ResolvedDistiller) -> Option<Box<dyn ModelProvider>> {
    match resolved.provider.as_str() {
        "anthropic" => AnthropicProvider::for_distiller(
            &resolved.model,
            resolved.key.clone(),
            resolved.base_url.clone(),
            resolved.timeout_secs,
        )
        .ok()
        .map(|p| Box::new(p) as Box<dyn ModelProvider>),
        // S1.1: ollama reuses the OpenAI-compatible path; base_url is
        // always set (defaults to http://localhost:11434/v1).
        "openai" | "ollama" => OpenAiProvider::for_distiller(
            &resolved.model,
            resolved.key.clone(),
            resolved.base_url.clone(),
            resolved.timeout_secs,
        )
        .ok()
        .map(|p| Box::new(p) as Box<dyn ModelProvider>),
        "bedrock" => {
            let region = resolved.region.clone()?;
            let secret_key = resolved.secret_key.clone()?;
            BedrockProvider::for_distiller(
                &resolved.model,
                region,
                resolved.key.clone(),
                secret_key,
                resolved.session_token.clone(),
                1024,
                0.2,
                resolved.timeout_secs,
            )
            .ok()
            .map(|p| Box::new(p) as Box<dyn ModelProvider>)
        }
        _ => None,
    }
}

/// Run the configured distiller against a known transcript path. Used by
/// SessionEnd hooks where available, and by Codex Stop hooks because current
/// Codex releases expose Stop but not SessionEnd.
pub fn run_distiller_for_transcript(workspace: &Path, transcript_path: &str) -> Option<usize> {
    let resolved = resolve_distiller(workspace)?;
    let view = build_transcript_view(transcript_path, MAX_VIEW_CHARS);
    if view.trim().is_empty() {
        return Some(0);
    }
    let mut provider: Box<dyn ModelProvider> = match resolved.provider.as_str() {
        "anthropic" => match AnthropicProvider::for_distiller(
            &resolved.model,
            resolved.key,
            resolved.base_url,
            resolved.timeout_secs,
        ) {
            Ok(provider) => Box::new(provider),
            Err(_) => return None,
        },
        // S1.1: ollama reuses the OpenAI-compatible path; base_url is always
        // set (defaults to http://localhost:11434/v1 at resolve time).
        "openai" | "ollama" => match OpenAiProvider::for_distiller(
            &resolved.model,
            resolved.key,
            resolved.base_url,
            resolved.timeout_secs,
        ) {
            Ok(provider) => Box::new(provider),
            Err(_) => return None,
        },
        "bedrock" => {
            let region = resolved.region?;
            let secret_key = resolved.secret_key?;
            match BedrockProvider::for_distiller(
                &resolved.model,
                region,
                resolved.key,
                secret_key,
                resolved.session_token,
                1024,
                0.2,
                resolved.timeout_secs,
            ) {
                Ok(provider) => Box::new(provider),
                Err(_) => return None,
            }
        }
        _ => return None,
    };
    let recorded = distill_and_record(
        &resolved.record_start,
        &view,
        provider.as_mut(),
        resolved.scope,
    );
    if recorded > 0 {
        println!(
            "[Kimetsu] distilled {recorded} lesson{} at session end.",
            if recorded == 1 { "" } else { "s" }
        );
    }
    Some(recorded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kimetsu_agent::model::{MockProvider, ModelResponse, StopReason, TokenUsage};

    // ── A7: normalize_distiller_provider bedrock/aws aliases ─────────────

    #[test]
    fn normalize_distiller_provider_bedrock_alias() {
        assert_eq!(normalize_distiller_provider("bedrock"), Some("bedrock"));
        assert_eq!(normalize_distiller_provider("Bedrock"), Some("bedrock"));
        assert_eq!(normalize_distiller_provider("BEDROCK"), Some("bedrock"));
    }

    #[test]
    fn normalize_distiller_provider_aws_alias() {
        assert_eq!(normalize_distiller_provider("aws"), Some("bedrock"));
        assert_eq!(normalize_distiller_provider("AWS"), Some("bedrock"));
    }

    #[test]
    fn normalize_distiller_provider_existing_aliases_unchanged() {
        assert_eq!(normalize_distiller_provider("anthropic"), Some("anthropic"));
        assert_eq!(normalize_distiller_provider("claude"), Some("anthropic"));
        assert_eq!(normalize_distiller_provider("openai"), Some("openai"));
        assert_eq!(normalize_distiller_provider("oai"), Some("openai"));
        assert_eq!(normalize_distiller_provider("gpt"), Some("openai"));
        assert_eq!(normalize_distiller_provider("unknown"), None);
    }

    // S1.1: ollama provider
    #[test]
    fn normalize_distiller_provider_ollama() {
        assert_eq!(normalize_distiller_provider("ollama"), Some("ollama"));
        assert_eq!(normalize_distiller_provider("Ollama"), Some("ollama"));
        assert_eq!(normalize_distiller_provider("OLLAMA"), Some("ollama"));
    }

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
        write_distiller_toml_with_provider(
            dir,
            enabled,
            "anthropic",
            model,
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_BASE_URL",
        );
    }

    fn write_distiller_toml_with_provider(
        dir: &std::path::Path,
        enabled: bool,
        provider: &str,
        model: &str,
        api_key_env: &str,
        base_url_env: &str,
    ) {
        std::fs::create_dir_all(dir).unwrap();
        let mut config = ProjectConfig::default_for_project("test");
        config.learning.distiller.enabled = enabled;
        config.learning.distiller.provider = provider.to_string();
        config.learning.distiller.model = model.to_string();
        config.learning.distiller.api_key_env = api_key_env.to_string();
        config.learning.distiller.base_url_env = base_url_env.to_string();
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
        assert_eq!(r.provider, "anthropic");
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
        assert_eq!(r.provider, "anthropic");
        assert_eq!(r.model, "ws-model");
        assert_eq!(r.key, "sk-ws");

        std::fs::remove_dir_all(ws).ok();
        std::fs::remove_dir_all(gdir).ok();
    }

    #[test]
    fn resolve_distiller_openai_workspace() {
        let ws = std::env::temp_dir().join(format!(
            "km_rd_oai_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(ws.join(".kimetsu")).unwrap();
        assert!(
            kimetsu_core::paths::git_init_boundary(&ws),
            "git_init_boundary failed - git needed for workspace isolation"
        );
        {
            let mut config = ProjectConfig::default_for_project("ws");
            config.learning.distiller.enabled = true;
            config.learning.distiller.provider = "openai".to_string();
            config.learning.distiller.model = "gpt-5.4-mini".to_string();
            config.learning.distiller.api_key_env = "OPENAI_API_KEY".to_string();
            config.learning.distiller.base_url_env = "OPENAI_BASE_URL".to_string();
            let toml = config.to_toml().unwrap();
            std::fs::write(ws.join(".kimetsu").join("project.toml"), toml).unwrap();
        }
        std::fs::write(
            ws.join(".env"),
            "OPENAI_API_KEY=sk-openai\nOPENAI_BASE_URL=http://localhost:4000/v1\n",
        )
        .unwrap();

        let r = resolve_distiller_with(&ws, None).expect("workspace resolved");
        assert_eq!(r.scope, MemoryScope::Project);
        assert_eq!(r.provider, "openai");
        assert_eq!(r.model, "gpt-5.4-mini");
        assert_eq!(r.key, "sk-openai");
        assert_eq!(r.base_url.as_deref(), Some("http://localhost:4000/v1"));

        std::fs::remove_dir_all(ws).ok();
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
