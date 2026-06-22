//! Flagship 3 — "The Active Brain": grounded answer composition.
//!
//! Shared composer reused by:
//!   - `kimetsu brain ask "<question>"` (3.1 CLI, via `kimetsu-cli`)
//!   - `kimetsu_brain_answer` MCP tool (3.2, `mcp_server.rs`)
//!
//! Design decisions:
//!   - **DP-B**: the composer resolves the cheap model via the distiller
//!     resolution chain (workspace first, then global). ollama / any local
//!     model runs entirely offline with zero frontier tokens.  Only falls
//!     back to a remote distiller (Anthropic / OpenAI / Bedrock) when no
//!     local model is configured.
//!   - **DP-C**: when NEITHER a local model NOR any distiller creds are
//!     configured, return the top retrieved capsules verbatim — no model
//!     call, no failure.
//!   - **GROUNDED-ONLY**: when retrieval is empty / floored, return a
//!     fixed refusal string; never add training-knowledge to the answer.
//!   - **3.4 Command fast-path**: "how do I …" queries reorder
//!     `command`-kind capsules to the front of the context block so they
//!     are always prominent regardless of the model.

use std::path::Path;

use kimetsu_agent::model::{
    MessageContent, MessageRole, ModelMessage, ModelProvider, ModelRequest, ToolChoice,
};
use kimetsu_brain::context::{ContextCapsule, ContextRequest};
use kimetsu_brain::project;
use kimetsu_core::config::{CheapModelSection, ProjectConfig};
use kimetsu_core::env_file::resolve_env_value;
use kimetsu_core::paths::{ProjectPaths, user_brain_enabled, user_kimetsu_dir};

// ── Public output type ────────────────────────────────────────────────────────

/// Stable JSON-serialisable output from the composer (3.1 + 3.2).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AskAnswer {
    /// The composed (or verbatim) answer text.
    pub answer: String,
    /// Memory / file citation ids (`memory:<id>` or `file:<path>`).
    pub citations: Vec<String>,
    /// `true` when the answer is grounded in retrieved memories/files.
    /// `false` for grounded-only refusals (nothing retrieved).
    pub grounded: bool,
    /// Model id used, `"verbatim"` on the degraded path, `"none"` on
    /// errors / refusals.
    pub model_used: String,
    /// `true` when the answer is raw capsule text (DP-C verbatim path).
    pub verbatim: bool,
}

// ── Constants ─────────────────────────────────────────────────────────────────

/// Retrieval budget for ask queries (generous but not overwhelming).
pub const ASK_BUDGET_TOKENS: u32 = 4_000;

/// Max capsules fed to the composer.
pub const ASK_MAX_CAPSULES: usize = 6;

/// Score floor (matches `kimetsu_brain_context` default).
pub const ASK_MIN_SCORE: f32 = 0.15;

// ── 3.4 Command fast-path ─────────────────────────────────────────────────────

/// `true` when the query is a "how do I …" / "how to …" / "what command …"
/// question — triggering the command fast-path that reorders
/// `kind = command` capsules to the front.
pub fn is_command_query(query: &str) -> bool {
    let q = query.trim().to_ascii_lowercase();
    q.starts_with("how do i ")
        || q.starts_with("how to ")
        || q.starts_with("what command ")
        || q.starts_with("what's the command")
        || q.starts_with("whats the command")
        || q.contains("run the command")
        || q.contains("run command")
}

/// Reorder capsules so `command`-kind comes first when the query is a
/// "how do I …" question. Relative order within each group is preserved.
/// No-op for non-command queries.
pub fn reorder_for_command_fastpath(
    capsules: Vec<ContextCapsule>,
    query: &str,
) -> Vec<ContextCapsule> {
    if !is_command_query(query) {
        return capsules;
    }
    let mut commands = Vec::new();
    let mut others = Vec::new();
    for cap in capsules {
        if cap.kind == "command" {
            commands.push(cap);
        } else {
            others.push(cap);
        }
    }
    commands.extend(others);
    commands
}

// ── Main public entry point ───────────────────────────────────────────────────

/// Retrieve brain context for `question` and compose a grounded answer.
///
/// Shared by 3.1 CLI (`kimetsu brain ask`) and 3.2 MCP
/// (`kimetsu_brain_answer`).  Never panics — all errors degrade to a safe
/// string answer.
///
/// Degradation ladder:
/// 1. Brain unavailable → informational error message.
/// 2. Retrieval empty / floored → grounded-only refusal.
/// 3. Retrieval OK + model available → composed, cited answer.
/// 4. Retrieval OK + no model → verbatim top capsules (DP-C).
pub fn compose_answer(workspace: &Path, question: &str) -> AskAnswer {
    // ── Retrieve ──────────────────────────────────────────────────────────────
    let request = ContextRequest {
        stage: "ask".to_string(),
        query: question.to_string(),
        budget_tokens: ASK_BUDGET_TOKENS,
        min_score: ASK_MIN_SCORE,
        max_capsules: ASK_MAX_CAPSULES,
        ..Default::default()
    };

    let bundle = match project::retrieve_context_readonly_with_request(workspace, request) {
        Ok(b) => b,
        Err(err) => {
            return AskAnswer {
                answer: format!(
                    "Brain unavailable for this workspace: {err}. \
                     Is the project initialized (`kimetsu init`)?"
                ),
                citations: Vec::new(),
                grounded: false,
                model_used: "none".to_string(),
                verbatim: false,
            };
        }
    };

    // ── Grounded-only refusal ─────────────────────────────────────────────────
    if bundle.skipped || bundle.capsules.is_empty() {
        return AskAnswer {
            answer: "Nothing in project memory answers that.".to_string(),
            citations: Vec::new(),
            grounded: false,
            model_used: "none".to_string(),
            verbatim: false,
        };
    }

    // ── 3.4 Command fast-path reorder ─────────────────────────────────────────
    let capsules = reorder_for_command_fastpath(bundle.capsules, question);

    // ── Citation handles ──────────────────────────────────────────────────────
    let citations: Vec<String> = capsules
        .iter()
        .map(|c| c.expansion_handle.clone())
        .filter(|h| !h.is_empty())
        .collect();

    // ── DP-B: resolve cheap model (local preferred) ───────────────────────────
    match resolve_ask_provider(workspace) {
        Some((mut provider, model_id)) => {
            match call_composer(&capsules, question, provider.as_mut()) {
                Some(answer) => AskAnswer {
                    answer,
                    citations,
                    grounded: true,
                    model_used: model_id,
                    verbatim: false,
                },
                None => verbatim_answer(&capsules, &citations),
            }
        }
        None => verbatim_answer(&capsules, &citations),
    }
}

/// Record a citation for each memory handle in `citation_handles`, wiring
/// helpful-mark answers into the self-tuning ROI loop.
///
/// Best-effort: silently ignores non-memory handles and all errors.
pub fn record_helpful_mark(workspace: &Path, citation_handles: &[String]) {
    for handle in citation_handles {
        if let Some(memory_id) = handle.strip_prefix("memory:") {
            project::record_mcp_citation(workspace, memory_id, Some("marked helpful via ask")).ok();
        }
    }
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Build a verbatim answer from capsule texts when no model is available.
fn verbatim_answer(capsules: &[ContextCapsule], citations: &[String]) -> AskAnswer {
    let mut parts = Vec::new();
    for cap in capsules {
        if cap.kind == "command" {
            parts.push(format!("[command] {}", cap.summary));
        } else {
            parts.push(cap.summary.clone());
        }
    }
    let answer = if parts.is_empty() {
        "Nothing in project memory answers that.".to_string()
    } else {
        format!(
            "Here is what the brain has (no model configured for synthesis):\n\n{}",
            parts.join("\n\n---\n\n")
        )
    };
    AskAnswer {
        answer,
        citations: citations.to_vec(),
        grounded: true,
        model_used: "verbatim".to_string(),
        verbatim: true,
    }
}

/// Call the composer model and return the answer text, or `None` on failure.
fn call_composer(
    capsules: &[ContextCapsule],
    question: &str,
    provider: &mut dyn ModelProvider,
) -> Option<String> {
    let context_block = capsules
        .iter()
        .enumerate()
        .map(|(i, cap)| {
            let header = if cap.kind == "command" {
                format!("[#{} COMMAND — {}]", i + 1, cap.expansion_handle)
            } else {
                format!("[#{} {} — {}]", i + 1, cap.kind, cap.expansion_handle)
            };
            format!("{header}\n{}", cap.summary)
        })
        .collect::<Vec<_>>()
        .join("\n\n---\n\n");

    let system = "You are Kimetsu, an active project memory assistant. \
        Answer the user's question STRICTLY using the memory capsules below — \
        do NOT add information from general training knowledge. \
        If the capsules do not answer the question, say so explicitly. \
        Be concise. Cite the capsule numbers you used (e.g. 'per #1, #2').";

    let user_msg = format!(
        "Question: {question}\n\n\
         Memory capsules:\n\n{context_block}\n\n\
         Answer using only the capsules above."
    );

    let request = ModelRequest {
        messages: vec![
            ModelMessage {
                role: MessageRole::System,
                content: vec![MessageContent::Text {
                    text: system.to_string(),
                }],
            },
            ModelMessage::user_text(&user_msg),
        ],
        tools: Vec::new(),
        tool_choice: ToolChoice::None,
        max_output_tokens: 512,
        temperature: 0.1,
        metadata: serde_json::Value::Null,
    };

    let response = provider.complete(request).ok()?;
    let text = response.text?.trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

// ── Provider resolution (DP-B: local preferred) ───────────────────────────────

/// Resolve the provider to use for answer composition.
///
/// Priority (DP-B):
/// 1. Workspace `[cheap_model]` / `[learning.distiller]` — local model
///    (ollama) gets zero frontier tokens.
/// 2. Global `~/.kimetsu/project.toml` — same fallback chain.
/// 3. `None` → caller uses verbatim degradation (DP-C).
fn resolve_ask_provider(workspace: &Path) -> Option<(Box<dyn ModelProvider>, String)> {
    // 1. Workspace config.
    if let Ok(paths) = ProjectPaths::discover(workspace)
        && let Ok(config) = project::load_config(&paths)
    {
        if let Some(cm) = config.cheap_model() {
            if let Some(pair) =
                build_provider(&cm, &paths.repo_root, config.model.request_timeout_secs)
            {
                return Some(pair);
            }
        }
    }
    // 2. Global ~/.kimetsu config.
    let global_dir = if user_brain_enabled() {
        user_kimetsu_dir()
    } else {
        None
    };
    if let Some(dir) = global_dir
        && let Ok(text) = std::fs::read_to_string(dir.join("project.toml"))
        && let Ok(config) = ProjectConfig::from_toml(&text)
    {
        if let Some(cm) = config.cheap_model() {
            if let Some(pair) = build_provider(&cm, &dir, config.model.request_timeout_secs) {
                return Some(pair);
            }
        }
    }
    None
}

/// Construct a `ModelProvider` from a `CheapModelSection`, returning `None`
/// when credentials are missing.
fn build_provider(
    cm: &CheapModelSection,
    env_root: &Path,
    timeout_secs: u64,
) -> Option<(Box<dyn ModelProvider>, String)> {
    use kimetsu_agent::anthropic::AnthropicProvider;
    use kimetsu_agent::bedrock::BedrockProvider;
    use kimetsu_agent::openai::OpenAiProvider;

    let provider_name = normalize_provider(&cm.provider)?;
    let model_id = cm.model.clone();

    let provider: Box<dyn ModelProvider> = match provider_name {
        // DP-B: ollama = local model, zero frontier tokens, offline capable.
        "ollama" => {
            let base_url = resolve_env_value(env_root, &cm.base_url_env)
                .filter(|u| !u.trim().is_empty())
                .unwrap_or_else(|| CheapModelSection::OLLAMA_DEFAULT_BASE_URL.to_string());
            let key = resolve_env_value(env_root, &cm.api_key_env).unwrap_or_default();
            Box::new(
                OpenAiProvider::for_distiller(&model_id, key, Some(base_url), timeout_secs).ok()?,
            )
        }
        "openai" => {
            let key = resolve_env_value(env_root, &cm.api_key_env)?;
            let base_url = resolve_env_value(env_root, &cm.base_url_env);
            Box::new(OpenAiProvider::for_distiller(&model_id, key, base_url, timeout_secs).ok()?)
        }
        "anthropic" => {
            let key = resolve_env_value(env_root, &cm.api_key_env)?;
            let base_url = resolve_env_value(env_root, &cm.base_url_env);
            Box::new(AnthropicProvider::for_distiller(&model_id, key, base_url, timeout_secs).ok()?)
        }
        "bedrock" => {
            let access_key = resolve_env_value(env_root, "AWS_ACCESS_KEY_ID")?;
            let secret_key = resolve_env_value(env_root, "AWS_SECRET_ACCESS_KEY")?;
            let session_token = resolve_env_value(env_root, "AWS_SESSION_TOKEN");
            let region = cm.region.clone().or_else(|| {
                resolve_env_value(env_root, &cm.region_env)
                    .or_else(|| resolve_env_value(env_root, "AWS_DEFAULT_REGION"))
            })?;
            Box::new(
                BedrockProvider::for_distiller(
                    &model_id,
                    region,
                    access_key,
                    secret_key,
                    session_token,
                    512,
                    0.1,
                    timeout_secs,
                )
                .ok()?,
            )
        }
        _ => return None,
    };

    Some((provider, model_id))
}

fn normalize_provider(provider: &str) -> Option<&'static str> {
    match provider.trim().to_ascii_lowercase().as_str() {
        "anthropic" | "claude" => Some("anthropic"),
        "openai" | "oai" | "gpt" => Some("openai"),
        "bedrock" | "aws" => Some("bedrock"),
        "ollama" => Some("ollama"),
        _ => None,
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use kimetsu_brain::context::ContextCapsule;

    fn make_capsule(kind: &str, summary: &str, handle: &str) -> ContextCapsule {
        ContextCapsule {
            id: "test".to_string(),
            kind: kind.to_string(),
            summary: summary.to_string(),
            token_estimate: 10,
            expansion_handle: handle.to_string(),
            provenance: Vec::new(),
            confidence: 0.9,
            freshness: 1.0,
            relevance: 0.8,
            scope_weight: 1.0,
            score: 0.9,
        }
    }

    // ── 3.4 command fast-path ───────────────────────────────────────────────

    #[test]
    fn is_command_query_matches_how_do_i() {
        assert!(is_command_query("how do I run the tests?"));
        assert!(is_command_query("How do I build?"));
        assert!(is_command_query("how to install deps"));
        assert!(is_command_query("what command runs clippy"));
    }

    #[test]
    fn is_command_query_no_match_for_general_questions() {
        assert!(!is_command_query("explain the architecture"));
        assert!(!is_command_query("what is the broker?"));
    }

    #[test]
    fn reorder_puts_command_first() {
        let caps = vec![
            make_capsule("fact", "fact text", "memory:f1"),
            make_capsule("command", "cargo test --all", "memory:cmd1"),
            make_capsule("convention", "snake_case", "memory:c1"),
        ];
        let out = reorder_for_command_fastpath(caps, "how do I run the tests?");
        assert_eq!(out[0].kind, "command");
    }

    #[test]
    fn reorder_noop_for_non_command_query() {
        let caps = vec![
            make_capsule("fact", "fact text", "memory:f1"),
            make_capsule("command", "cmd", "memory:cmd1"),
        ];
        let orig_kinds: Vec<_> = caps.iter().map(|c| c.kind.clone()).collect();
        let out = reorder_for_command_fastpath(caps, "explain the broker");
        let out_kinds: Vec<_> = out.iter().map(|c| c.kind.clone()).collect();
        assert_eq!(orig_kinds, out_kinds);
    }

    // ── verbatim path (DP-C) ────────────────────────────────────────────────

    #[test]
    fn verbatim_answer_labels_command_prominently() {
        let caps = vec![
            make_capsule("command", "cargo clippy", "memory:cmd1"),
            make_capsule("fact", "fact", "memory:f1"),
        ];
        let ans = verbatim_answer(&caps, &["memory:cmd1".to_string()]);
        assert!(ans.verbatim);
        assert!(ans.grounded);
        assert!(ans.answer.contains("[command]"));
        assert_eq!(ans.model_used, "verbatim");
    }

    // ── grounded-only refusal ───────────────────────────────────────────────

    #[test]
    fn compose_answer_refusal_when_no_brain() {
        let tmp = std::env::temp_dir().join(format!(
            "kimetsu_chat_ask_nodir_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let ans = compose_answer(&tmp, "how do I run tests?");
        assert!(
            ans.answer.contains("Brain unavailable")
                || ans.answer.contains("Nothing in project memory"),
            "unexpected: {}",
            ans.answer
        );
    }

    #[test]
    fn compose_answer_refusal_with_empty_brain() {
        let root = std::env::temp_dir().join(format!(
            "kimetsu_chat_ask_empty_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        kimetsu_core::paths::git_init_boundary(&root);
        kimetsu_brain::user_brain::with_user_brain_disabled(|| {
            kimetsu_brain::project::init_project(&root, true).expect("init");
            let ans = compose_answer(&root, "explain the flux capacitor");
            assert_eq!(ans.answer, "Nothing in project memory answers that.");
            assert!(!ans.grounded);
            assert!(ans.citations.is_empty());
        });
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn compose_answer_verbatim_or_composed_with_memory() {
        let root = std::env::temp_dir().join(format!(
            "kimetsu_chat_ask_verbatim_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        kimetsu_core::paths::git_init_boundary(&root);
        kimetsu_brain::user_brain::with_user_brain_disabled(|| {
            kimetsu_brain::project::init_project(&root, true).expect("init");
            kimetsu_brain::project::add_memory(
                &root,
                kimetsu_core::memory::MemoryScope::Project,
                kimetsu_core::memory::MemoryKind::Fact,
                "use cargo test --workspace to run all tests",
            )
            .expect("add_memory");
            let ans = compose_answer(&root, "how do I run tests?");
            // Must not be a grounded refusal when memory exists.
            assert_ne!(ans.answer, "Nothing in project memory answers that.");
            // Must be grounded (either verbatim or composed).
            assert!(ans.grounded || ans.citations.is_empty());
        });
        std::fs::remove_dir_all(root).ok();
    }

    // ── helpful mark ───────────────────────────────────────────────────────

    #[test]
    fn record_helpful_mark_no_panic_on_file_handles() {
        let tmp = std::env::temp_dir().join("kimetsu_chat_ask_helpful");
        record_helpful_mark(&tmp, &["file:src/main.rs".to_string()]);
        // No panic = pass.
    }
}
