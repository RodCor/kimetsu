//! REPL loop for `kimetsu chat`.
//!
//! Reads user messages from stdin, drives the kimetsu agent core, prints
//! assistant responses to stdout. Slash commands are handled inline
//! before any model round-trip.
//!
//! v0.3.1 — agent round-trip live:
//!   - public surface (`ChatConfig`, `run_repl`) is defined here
//!   - per-message agent invocations call `kimetsu_agent::harbor::run_model_agent`
//!     directly. The agent loop became transport-agnostic in Phase-2 of
//!     the v0.3 split (no HarborSession dependency), so chat reuses
//!     the same 20-tool surface + MP-18 verify the gauntlet validated.
//!   - tool runtime uses host-side `LocalShellExecutor` (the default for
//!     `ToolRuntime::new`) — commands execute against the user's actual
//!     filesystem under `config.workspace_root`.
//!   - slash commands handled by [`crate::commands`]
//!   - cost meter wires through [`crate::cost::CostMeter`]
//!   - claude_code provider construction reuses the same `CLAUDE_CODE_OAUTH_TOKEN`
//!     environment variable kimetsu's harbor mode uses; budget tracked
//!     against `config.max_cost_usd`.
//!
//! What's deferred to v0.3.2:
//!   - session resume / persistent transcripts
//!   - approve-each-write mode (currently always-bypass like harbor)
//!   - streaming partial responses (today: blocking per-turn)
//!   - per-tool cost breakdown (rolled-up cost works)
//!   - rendering tool-call results inline with diff syntax

use std::io::{BufRead, Write};
use std::path::PathBuf;

use kimetsu_agent::claude_code::ClaudeCodeProvider;
use kimetsu_agent::harbor::{HarborAgentOpts, run_model_agent};
use kimetsu_agent::tools::{ToolRuntime, ToolRuntimeConfig};
use kimetsu_brain::project as brain_project;
use kimetsu_core::config::ProjectConfig;
use kimetsu_core::ids::RunId;

use crate::commands::SlashCommand;
use crate::cost::CostMeter;

/// Configuration for one chat session.
#[derive(Debug, Clone)]
pub struct ChatConfig {
    /// Workspace root. All file/shell tools operate relative to this path.
    /// Canonicalized at session start; must exist.
    pub workspace_root: PathBuf,
    /// Optional kimetsu project directory (contains `.kimetsu/`). When set,
    /// brain context is loaded for retrieval and any record_deviation calls
    /// surface as memory proposals at session end. Same shape as harbor
    /// mode's `KIMETSU_HARBOR_PROJECT`.
    pub brain_project: Option<PathBuf>,
    /// Model identifier (defaults to "claude-opus-4-7").
    pub model: String,
    /// Maximum cost ceiling per session in USD. The cost meter halts the
    /// session when the model's reported `total_cost_usd` crosses this.
    pub max_cost_usd: f32,
    /// Initial goal statement. When set, MP-18's iterative verify loop
    /// has a concrete target on every finish attempt. Can also be set
    /// later via `/goal <text>`.
    pub goal: Option<String>,
    /// Whether MP-18's strict verify mode is active for this session
    /// (model MUST call `record_deviation` on every fix-up cycle). Default
    /// false; user toggles via `/strict` slash command.
    pub strict_verify: bool,
}

impl ChatConfig {
    pub fn new(workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            workspace_root: workspace_root.into(),
            brain_project: None,
            model: "claude-opus-4-7".to_string(),
            max_cost_usd: 10.0,
            goal: None,
            strict_verify: false,
        }
    }
}

pub type ChatResult<T> = Result<T, ChatError>;

#[derive(Debug)]
pub enum ChatError {
    Io(std::io::Error),
    NoOauthToken,
    BrainProject(String),
    Provider(String),
    Quit,
}

impl std::fmt::Display for ChatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::NoOauthToken => write!(
                f,
                "CLAUDE_CODE_OAUTH_TOKEN is not set. Run `claude auth` or export the token."
            ),
            Self::BrainProject(e) => write!(f, "brain project: {e}"),
            Self::Provider(e) => write!(f, "model provider: {e}"),
            Self::Quit => write!(f, "session ended by user"),
        }
    }
}

impl From<std::io::Error> for ChatError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// Entry point: run an interactive chat session against `stdin`/`stdout`.
///
/// v0.3.0-alpha: this is a scaffold. The body wires the slash-command
/// handler + cost meter + greeting + quit handling so callers can verify
/// the dependency direction (chat → kimetsu-agent only, no harbor). The
/// model round-trip itself is plumbed in [`run_repl_with_agent`] which
/// lands in the v0.3.0 commit that wires the provider.
pub fn run_repl<R: BufRead, W: Write>(
    mut reader: R,
    mut writer: W,
    config: ChatConfig,
) -> ChatResult<()> {
    let workspace = config
        .workspace_root
        .canonicalize()
        .map_err(ChatError::Io)?;
    writeln!(writer, "kimetsu chat v{}", env!("CARGO_PKG_VERSION"))?;
    writeln!(writer, "workspace: {}", workspace.display())?;
    if let Some(p) = config.brain_project.as_ref() {
        writeln!(writer, "brain project: {}", p.display())?;
    } else {
        writeln!(writer, "brain project: <none>  (no memory retrieval / curation)")?;
    }
    writeln!(writer, "model: {}", config.model)?;
    writeln!(writer, "budget: ${:.2}  strict_verify: {}", config.max_cost_usd, config.strict_verify)?;
    if let Some(g) = config.goal.as_ref() {
        writeln!(writer, "goal: {g}")?;
    }
    writeln!(
        writer,
        "type `/help` for slash commands. `/quit` to exit."
    )?;

    // Validate brain project up-front so we don't surprise the user
    // mid-session with a stale path error.
    if let Some(ref p) = config.brain_project {
        validate_brain_project(p)?;
    }

    // Per-session state.
    let _run_id = RunId::new();
    let mut cost = CostMeter::new(config.max_cost_usd);
    let mut goal = config.goal.clone();
    let mut strict_verify = config.strict_verify;

    // v0.3.1: provider is constructed lazily on first user message so
    // /help / /quit / /cost don't fail if CLAUDE_CODE_OAUTH_TOKEN is
    // unset. Same env handshake harbor mode uses.
    let mut provider: Option<Box<dyn kimetsu_agent::model::ModelProvider>> = None;

    // v0.3.3: multi-turn conversation state. Each TurnRecord captures
    // the user's message and the model's final response. On each new
    // turn we render the accumulated transcript into the brain-context
    // block so the model sees its own past responses; the agent loop
    // itself is still stateless per call. Cheap, no agent-loop
    // refactor required.
    let mut transcript: Vec<TurnRecord> = Vec::new();

    // v0.3.3: accumulated MP-18 record_deviation calls across all
    // turns this session. On `/quit` we walk this list and write each
    // `lesson_for_next_time` to the brain as a failure_pattern memory
    // — closes the MP-18 feedback loop.
    let mut session_deviations: Vec<DeviationRecord> = Vec::new();

    loop {
        write!(writer, "\nyou> ")?;
        writer.flush()?;
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            // EOF
            writeln!(writer, "\n(stdin closed; ending session)")?;
            return Ok(());
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // Slash commands run inline (no model round-trip).
        if let Some(cmd) = SlashCommand::parse(line) {
            match cmd {
                SlashCommand::Help => {
                    SlashCommand::print_help(&mut writer)?;
                }
                SlashCommand::Quit => {
                    // v0.3.3: auto-propose any accumulated record_deviation
                    // lessons to the brain pool as failure_pattern memories.
                    // The user can prune later via `kimetsu brain memory
                    // prune`; for now we trust the model's self-reflection
                    // enough to land them as accepted memories.
                    if let Some(ref project_dir) = config.brain_project {
                        if !session_deviations.is_empty() {
                            writeln!(
                                writer,
                                "\n[brain] adding {} deviation lesson(s) to {}",
                                session_deviations.len(),
                                project_dir.display(),
                            )?;
                            for d in &session_deviations {
                                match brain_project::add_memory(
                                    project_dir,
                                    kimetsu_core::memory::MemoryScope::GlobalUser,
                                    kimetsu_core::memory::MemoryKind::FailurePattern,
                                    &d.lesson_for_next_time,
                                ) {
                                    Ok(id) => writeln!(
                                        writer,
                                        "  + {} ({})",
                                        preview(&d.lesson_for_next_time, 70),
                                        id,
                                    )?,
                                    Err(e) => writeln!(
                                        writer,
                                        "  ! failed to add: {e} (lesson: {})",
                                        preview(&d.lesson_for_next_time, 70),
                                    )?,
                                }
                            }
                        }
                    } else if !session_deviations.is_empty() {
                        writeln!(
                            writer,
                            "\n[brain] {} deviation lesson(s) captured but not persisted (no --project set):",
                            session_deviations.len()
                        )?;
                        for d in &session_deviations {
                            writeln!(writer, "  - {}", preview(&d.lesson_for_next_time, 80))?;
                        }
                    }
                    writeln!(writer, "bye.")?;
                    return Ok(());
                }
                SlashCommand::Cost => {
                    writeln!(
                        writer,
                        "cost so far: ${:.4} (budget ${:.2}, remaining ${:.4})",
                        cost.spent(),
                        cost.budget(),
                        cost.remaining()
                    )?;
                }
                SlashCommand::Goal(g) => {
                    if g.is_empty() {
                        match goal.as_ref() {
                            Some(g) => writeln!(writer, "goal: {g}")?,
                            None => writeln!(writer, "goal: <none>")?,
                        }
                    } else {
                        goal = Some(g);
                        writeln!(writer, "goal set.")?;
                    }
                }
                SlashCommand::Strict(on) => {
                    strict_verify = on;
                    writeln!(writer, "strict verify: {on}")?;
                }
                SlashCommand::Memory(arg) => {
                    // v0.3.3: minimal in-REPL memory ops. For `list` we
                    // call brain_project::list_memories; everything else
                    // points the user at the full CLI so we don't
                    // duplicate the kimetsu-brain command surface here.
                    if let Some(ref project_dir) = config.brain_project {
                        match arg.split_whitespace().next().unwrap_or("") {
                            "list" => match brain_project::list_memories(project_dir) {
                                Ok(rows) => {
                                    writeln!(writer, "{} active memories:", rows.len())?;
                                    for r in rows.iter().take(20) {
                                        writeln!(
                                            writer,
                                            "  {} [{}:{}]  uses={} usefulness={:+.2}  {}",
                                            r.memory_id,
                                            r.scope,
                                            r.kind,
                                            r.use_count,
                                            r.usefulness_score,
                                            preview(&r.text, 80)
                                        )?;
                                    }
                                    if rows.len() > 20 {
                                        writeln!(writer, "  ... ({} more; use `kimetsu brain memory list` for full)", rows.len() - 20)?;
                                    }
                                }
                                Err(e) => writeln!(writer, "[error] {e}")?,
                            },
                            "" => writeln!(
                                writer,
                                "usage: /memory list  (use `kimetsu brain memory ...` for add/review/prune)"
                            )?,
                            other => writeln!(
                                writer,
                                "/memory {other}: not handled in REPL. Use `kimetsu brain memory {other} ...` from your shell."
                            )?,
                        }
                    } else {
                        writeln!(
                            writer,
                            "memory commands require --project pointing at a kimetsu project"
                        )?;
                    }
                }
            }
            continue;
        }

        // v0.3.1: real agent round-trip.

        // Budget gate before spending any tokens.
        if cost.over_budget() {
            writeln!(
                writer,
                "[budget] $${:.4} of $${:.2} spent — refusing further model calls. Raise --max-cost-usd or /quit.",
                cost.spent(),
                cost.budget()
            )?;
            continue;
        }

        // Construct provider lazily on first user message. Failure is
        // non-fatal: print the error, let the user fix env vars (or
        // /quit) without exiting the REPL outright.
        if provider.is_none() {
            match build_chat_provider(&config, &workspace) {
                Ok(p) => provider = Some(p),
                Err(e) => {
                    writeln!(writer, "[error] {e}")?;
                    continue;
                }
            }
        }

        // Re-create the runtime per message so the run-id rotates and
        // tool side-effects don't leak across messages. (Tool runtime
        // doesn't need to live across messages since the brain pool
        // and conversation context are external state.)
        let mut runtime = match ToolRuntime::new(&workspace, RunId::new()) {
            Ok(r) => r.with_config(ToolRuntimeConfig {
                redact_secrets: false,
                ..ToolRuntimeConfig::default()
            }),
            Err(e) => {
                writeln!(writer, "[error] tool runtime: {e}")?;
                continue;
            }
        };

        // v0.3.3: build the task + brain-context for this turn.
        //
        // task:      the user's new message (with goal preamble if set)
        // context:   accumulated transcript of prior turns + retrieved
        //            brain capsules (if --project is set). The agent
        //            loop renders this as a "Prior knowledge" block
        //            before the user message — giving the model
        //            conversational memory + memory-pool retrieval in
        //            one channel.
        let task = match goal.as_ref() {
            Some(g) if !g.is_empty() => format!("Goal: {g}\nMessage: {line}"),
            _ => line.to_string(),
        };
        let brain_context = build_chat_brain_context(
            config.brain_project.as_deref(),
            &transcript,
            &task,
        );

        // Strict verify mode is wired through the env var the agent
        // loop already reads (MP-18). Setting it for this turn only.
        // SAFETY (Rust 2024): set_var/remove_var are flagged unsafe
        // because env mutation isn't thread-safe. We're single-threaded
        // here (REPL loop, no spawned threads racing on env), so this
        // is sound.
        let prev_strict = std::env::var("KIMETSU_HARBOR_VERIFY_STRICT").ok();
        unsafe {
            std::env::set_var(
                "KIMETSU_HARBOR_VERIFY_STRICT",
                if strict_verify { "1" } else { "0" },
            );
        }

        let opts = HarborAgentOpts::default();
        let result = run_model_agent(
            &task,
            &mut runtime,
            provider.as_mut().expect("provider built above").as_mut(),
            opts,
            brain_context.as_deref(),
        );

        // Restore env so we don't leak state. SAFETY: single-threaded
        // REPL — see note above.
        unsafe {
            match prev_strict {
                Some(v) => std::env::set_var("KIMETSU_HARBOR_VERIFY_STRICT", v),
                None => std::env::remove_var("KIMETSU_HARBOR_VERIFY_STRICT"),
            }
        }

        match result {
            Ok(report) => {
                let turn_cost = report.usage.cost_usd;
                let still_within = cost.record_turn(turn_cost);
                writeln!(writer, "\nassistant>")?;
                let assistant_text = report
                    .final_text
                    .clone()
                    .unwrap_or_else(|| report.summary.clone());
                writeln!(writer, "{}", assistant_text)?;

                // v0.3.3: append this turn to the conversation transcript
                // so subsequent turns see prior context. Truncate the
                // user/assistant text to keep the brain_context block
                // bounded across long sessions.
                transcript.push(TurnRecord {
                    user: line.to_string(),
                    assistant: assistant_text.clone(),
                });

                // v0.3.3: pluck recorded_deviations out of the report
                // context and accumulate for the session. On /quit we
                // write each lesson_for_next_time as a memory.
                if let Some(devs) = report
                    .context
                    .get("recorded_deviations")
                    .and_then(|v| v.as_array())
                {
                    for d in devs {
                        let missing = d
                            .get("missing")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let where_deviated = d
                            .get("where_deviated")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let lesson = d
                            .get("lesson_for_next_time")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        if !lesson.is_empty() {
                            session_deviations.push(DeviationRecord {
                                missing,
                                where_deviated,
                                lesson_for_next_time: lesson,
                            });
                        }
                    }
                }
                writeln!(
                    writer,
                    "\n[turn] cost=$${:.4}  total=$${:.4}/$${:.2}  turns={}  tool_calls={}",
                    turn_cost,
                    cost.spent(),
                    cost.budget(),
                    report.turns,
                    report.tool_calls,
                )?;
                if !still_within {
                    writeln!(
                        writer,
                        "[budget] crossed budget; next message will be refused."
                    )?;
                }
                // v0.3.3: deviations are now accumulated into
                // session_deviations above; we tell the user only how
                // many fired so the output stays concise.
                let dev_count = report
                    .context
                    .get("recorded_deviations")
                    .and_then(|v| v.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);
                if dev_count > 0 {
                    writeln!(
                        writer,
                        "[deviations] {dev_count} recorded this turn (will be added to brain on /quit)"
                    )?;
                }
            }
            Err(e) => {
                writeln!(writer, "[error] {e}")?;
            }
        }
    }
}

/// v0.3.3: one user/assistant exchange in the chat transcript. Used to
/// render the conversational history into the brain-context block on
/// the next turn so the model sees its own previous responses.
#[derive(Debug, Clone)]
struct TurnRecord {
    user: String,
    assistant: String,
}

/// v0.3.3: a single MP-18 record_deviation call captured from a turn.
/// Accumulated for the session; on /quit we walk these and write each
/// `lesson_for_next_time` to the brain pool as a failure_pattern
/// memory — closes the MP-18 feedback loop in chat mode.
#[derive(Debug, Clone)]
struct DeviationRecord {
    #[allow(dead_code)]
    missing: String,
    #[allow(dead_code)]
    where_deviated: String,
    lesson_for_next_time: String,
}

/// v0.3.3: build the brain-context string for one chat turn.
///
/// Two concatenated blocks (either may be empty):
///   1. Retrieved brain capsules (if --project is set) — surfaced from
///      `kimetsu_brain::project::retrieve_context` with the current
///      task as the query, 2000-token budget, stage="chat".
///   2. Conversation transcript so far (if any prior turns) — last
///      ~10 turns of user/assistant exchanges, each capped so a long
///      session doesn't blow the prompt budget.
///
/// The agent loop renders this whole string as a "Prior knowledge"
/// block before the new user message. The model gets memory-pool
/// retrieval AND conversational continuity through a single channel.
fn build_chat_brain_context(
    project_dir: Option<&std::path::Path>,
    transcript: &[TurnRecord],
    task: &str,
) -> Option<String> {
    const TURN_CAP_BYTES: usize = 1200;
    const TRANSCRIPT_TAIL: usize = 10;

    let mut out = String::new();

    // Block 1: retrieved capsules.
    if let Some(dir) = project_dir {
        if dir.is_dir() {
            match brain_project::retrieve_context(dir, "chat", task, 2000) {
                Ok(bundle) if !bundle.capsules.is_empty() => {
                    out.push_str(&format!(
                        "Retrieved {} brain capsule(s) for this turn ({}/{} tokens used):\n",
                        bundle.capsules.len(),
                        bundle.used_tokens,
                        bundle.budget_tokens,
                    ));
                    for (i, c) in bundle.capsules.iter().enumerate() {
                        out.push_str(&format!(
                            "  [{}] {} (score {:.2}, scope_weight {:.2})\n      {}\n",
                            i + 1,
                            c.kind,
                            c.score,
                            c.scope_weight,
                            preview(&c.summary, 240),
                        ));
                    }
                }
                Ok(_) => {
                    // Brain is configured but had nothing relevant.
                    // We deliberately stay silent in the prompt — no
                    // need to tell the model the brain was empty.
                }
                Err(_) => {
                    // Silent failure: brain retrieval is best-effort.
                    // If the DB is missing or corrupt the user will
                    // see it on the brain CLI; don't pollute the prompt.
                }
            }
        }
    }

    // Block 2: conversation transcript (last N turns, each capped).
    if !transcript.is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        let start = transcript.len().saturating_sub(TRANSCRIPT_TAIL);
        let recent = &transcript[start..];
        out.push_str(&format!(
            "Conversation so far (last {} turn(s) of this session):\n",
            recent.len()
        ));
        for (i, t) in recent.iter().enumerate() {
            out.push_str(&format!(
                "  turn {}\n  > user: {}\n  < assistant: {}\n",
                start + i + 1,
                preview(&t.user, TURN_CAP_BYTES / 2),
                preview(&t.assistant, TURN_CAP_BYTES / 2),
            ));
        }
    }

    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Build the model provider for chat. Today: claude_code via
/// CLAUDE_CODE_OAUTH_TOKEN, same handshake harbor mode uses. Future:
/// switch on `config.model` prefix (anthropic API key vs CC OAuth).
fn build_chat_provider(
    config: &ChatConfig,
    workspace: &std::path::Path,
) -> ChatResult<Box<dyn kimetsu_agent::model::ModelProvider>> {
    let oauth = std::env::var("CLAUDE_CODE_OAUTH_TOKEN")
        .map_err(|_| ChatError::NoOauthToken)?;
    let mut project_cfg = ProjectConfig::default_for_project("kimetsu-chat");
    project_cfg.model.provider = "claude_code".to_string();
    project_cfg.model.model = config.model.clone();
    project_cfg.model.api_key_env = "CLAUDE_CODE_OAUTH_TOKEN".to_string();
    project_cfg.model.request_timeout_secs = std::env::var("KIMETSU_HARBOR_PROVIDER_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(1500);
    project_cfg.run.max_total_cost_usd = config.max_cost_usd;

    match ClaudeCodeProvider::from_config_with_key(workspace, &project_cfg, Some(&oauth)) {
        Ok(Some(p)) => Ok(Box::new(p)),
        Ok(None) => Err(ChatError::Provider(
            "ClaudeCodeProvider returned None (no API key resolved)".into(),
        )),
        Err(e) => Err(ChatError::Provider(format!("{e}"))),
    }
}

/// Truncate a string for display in error / status messages. Used by
/// future code paths (deviation rendering, etc.); kept available so we
/// don't lose the helper between releases.
#[allow(dead_code)]
fn preview(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}…")
    }
}

fn validate_brain_project(p: &std::path::Path) -> ChatResult<()> {
    let kimetsu_dir = p.join(".kimetsu");
    if !kimetsu_dir.exists() {
        return Err(ChatError::BrainProject(format!(
            "{} has no .kimetsu/ subdir (run `kimetsu init` there first)",
            p.display()
        )));
    }
    // Light touch: open a connection to verify the brain DB is readable.
    // Heavier validation happens inside add_memory / retrieve_context if
    // the user runs slash commands that hit the brain.
    let _ = brain_project::list_memories(p)
        .map_err(|e| ChatError::BrainProject(format!("{e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn quit_command_ends_session_cleanly() {
        let input = b"/quit\n";
        let mut output = Vec::new();
        let config = ChatConfig::new(std::env::current_dir().unwrap());
        run_repl(Cursor::new(input), &mut output, config).expect("repl");
        let out_str = String::from_utf8(output).unwrap();
        assert!(out_str.contains("bye."));
    }

    #[test]
    fn non_slash_input_attempts_agent_round_trip() {
        // v0.3.1: non-slash messages now drive the agent. Without
        // CLAUDE_CODE_OAUTH_TOKEN the provider build fails — non-fatal,
        // we print the error and continue. Test that the REPL didn't
        // crash, did print the failure message, and still accepted
        // /quit afterward.
        unsafe {
            std::env::remove_var("CLAUDE_CODE_OAUTH_TOKEN");
        }
        let input = b"do something useful\n/quit\n";
        let mut output = Vec::new();
        let config = ChatConfig::new(std::env::current_dir().unwrap());
        run_repl(Cursor::new(input), &mut output, config).expect("repl");
        let out_str = String::from_utf8(output).unwrap();
        // We tried the model path; without OAuth we surfaced an error
        // but continued instead of crashing.
        assert!(
            out_str.contains("[error]") || out_str.contains("CLAUDE_CODE_OAUTH_TOKEN"),
            "expected provider build error message; got: {out_str}"
        );
        // /quit still works after the error.
        assert!(out_str.contains("bye."));
    }

    #[test]
    fn slash_help_prints_command_list() {
        let input = b"/help\n/quit\n";
        let mut output = Vec::new();
        let config = ChatConfig::new(std::env::current_dir().unwrap());
        run_repl(Cursor::new(input), &mut output, config).expect("repl");
        let out_str = String::from_utf8(output).unwrap();
        assert!(out_str.contains("/help"));
        assert!(out_str.contains("/quit"));
        assert!(out_str.contains("/cost"));
        assert!(out_str.contains("/goal"));
    }

    #[test]
    fn slash_goal_sets_and_recalls() {
        let input = b"/goal\n/goal refactor errors to thiserror\n/goal\n/quit\n";
        let mut output = Vec::new();
        let config = ChatConfig::new(std::env::current_dir().unwrap());
        run_repl(Cursor::new(input), &mut output, config).expect("repl");
        let out_str = String::from_utf8(output).unwrap();
        // First /goal: no goal set.
        assert!(out_str.contains("goal: <none>"));
        // Then /goal <text>: confirmation.
        assert!(out_str.contains("goal set."));
        // Then /goal: echoes the set goal.
        assert!(out_str.contains("goal: refactor errors to thiserror"));
    }

    #[test]
    fn slash_strict_toggles() {
        let input = b"/strict on\n/strict off\n/quit\n";
        let mut output = Vec::new();
        let config = ChatConfig::new(std::env::current_dir().unwrap());
        run_repl(Cursor::new(input), &mut output, config).expect("repl");
        let out_str = String::from_utf8(output).unwrap();
        assert!(out_str.contains("strict verify: true"));
        assert!(out_str.contains("strict verify: false"));
    }

    #[test]
    fn slash_cost_shows_zero_at_start() {
        let input = b"/cost\n/quit\n";
        let mut output = Vec::new();
        let config = ChatConfig::new(std::env::current_dir().unwrap());
        run_repl(Cursor::new(input), &mut output, config).expect("repl");
        let out_str = String::from_utf8(output).unwrap();
        assert!(out_str.contains("cost so far: $0.0000"));
    }

    // ----- v0.3.3: multi-turn transcript + brain retrieval helpers -----

    #[test]
    fn build_chat_brain_context_returns_none_when_no_state() {
        // No project, no transcript -> nothing to surface.
        let out = build_chat_brain_context(None, &[], "do something");
        assert!(out.is_none());
    }

    #[test]
    fn build_chat_brain_context_renders_transcript_alone() {
        // Transcript present, no project -> transcript block only.
        let transcript = vec![
            TurnRecord {
                user: "list files in src/".to_string(),
                assistant: "Found 12 files. Here they are: ...".to_string(),
            },
            TurnRecord {
                user: "read the largest one".to_string(),
                assistant: "It's main.rs (4321 lines). Want me to summarize?".to_string(),
            },
        ];
        let out = build_chat_brain_context(None, &transcript, "yes please")
            .expect("transcript should produce context");
        assert!(out.contains("Conversation so far"));
        assert!(out.contains("last 2 turn(s)"));
        assert!(out.contains("list files in src/"));
        assert!(out.contains("It's main.rs (4321 lines)"));
        // Each turn should be tagged with its 1-based index.
        assert!(out.contains("turn 1"));
        assert!(out.contains("turn 2"));
    }

    #[test]
    fn build_chat_brain_context_truncates_long_turns() {
        let huge_user = "a".repeat(5_000);
        let huge_asst = "b".repeat(5_000);
        let transcript = vec![TurnRecord {
            user: huge_user,
            assistant: huge_asst,
        }];
        let out =
            build_chat_brain_context(None, &transcript, "next").expect("context");
        // 1200/2 = 600-char cap per side; should be << 5000 chars.
        // Plus the cap appends "…" only when actually truncated, and
        // preview uses char count not bytes. Verify clipping happened.
        assert!(out.contains("aaa"));
        assert!(out.contains("bbb"));
        assert!(out.contains("…"), "expected the … truncation marker");
    }

    #[test]
    fn build_chat_brain_context_keeps_only_tail_when_history_is_long() {
        // 25 turns -> only the last 10 should appear in the rendered
        // block to keep the prompt budget bounded.
        let transcript: Vec<TurnRecord> = (1..=25)
            .map(|i| TurnRecord {
                user: format!("user message {i}"),
                assistant: format!("assistant reply {i}"),
            })
            .collect();
        let out = build_chat_brain_context(None, &transcript, "next")
            .expect("context");
        // Last 10 means turns 16..=25 are present, 1..=15 are gone.
        assert!(out.contains("user message 25"));
        assert!(out.contains("user message 16"));
        assert!(!out.contains("user message 15"));
        assert!(!out.contains("user message 1\n"));
        assert!(out.contains("last 10 turn(s)"));
    }
}
