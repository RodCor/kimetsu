//! REPL loop for `kimetsu chat`.
//!
//! Reads user messages from stdin, drives the kimetsu agent core, prints
//! assistant responses to stdout. Slash commands are handled inline
//! before any model round-trip.
//!
//! v0.3.1 - agent round-trip live:
//!   - public surface (`ChatConfig`, `run_repl`) is defined here
//!   - per-message agent invocations call `kimetsu_agent::harness::run_model_agent`
//!     directly. The agent loop became transport-agnostic in Phase-2 of
//!     the v0.3 split (no HarborSession dependency), so chat reuses
//!     the same 20-tool surface + MP-18 verify the gauntlet validated.
//!   - tool runtime uses host-side `LocalShellExecutor` (the default for
//!     `ToolRuntime::new`) - commands execute against the user's actual
//!     filesystem under `config.workspace_root`.
//!   - slash commands handled by [`crate::commands`]
//!   - cost meter wires through [`crate::cost::CostMeter`]
//!   - claude_code provider construction uses `CLAUDE_CODE_OAUTH_TOKEN`;
//!     budget tracked against `config.max_cost_usd`.
//!
//! What's deferred to v0.3.2:
//!   - session resume / persistent transcripts
//!   - approve-each-write mode
//!   - streaming partial responses (today: blocking per-turn)
//!   - per-tool cost breakdown (rolled-up cost works)
//!   - rendering tool-call results inline with diff syntax

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::{Engine as _, engine::general_purpose};
use crossterm::cursor::{MoveDown, MoveToColumn, MoveUp, RestorePosition, SavePosition};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, read};
use crossterm::queue;
use crossterm::terminal::{Clear, ClearType, disable_raw_mode, enable_raw_mode, size};
use kimetsu_agent::claude_code::ClaudeCodeProvider;
use kimetsu_agent::harness::{KimetsuAgentOpts, ToolLoadout, run_model_agent};
use kimetsu_agent::model::{
    MessageContent, MessageRole, ModelMessage, ModelRequest, StopReason, TokenUsage, ToolChoice,
};
use kimetsu_agent::tools::{CommandSpec, ToolRuntime, ToolRuntimeConfig};
use kimetsu_brain::project as brain_project;
use kimetsu_core::config::ProjectConfig;
use kimetsu_core::env_file::resolve_env_value;
use kimetsu_core::ids::RunId;
use serde::{Deserialize, Serialize};

use crate::bridge::{
    BridgeTarget, bridge_export_skill, bridge_import_skill, bridge_scan, bridge_sync,
};
use crate::commands::{SLASH_COMMANDS, SlashCommand, SlashCommandSpec};
use crate::cost::CostMeter;
use crate::skills::{
    LoadedSkill, SkillConfig, SkillManifest, SkillRegistry, loaded_skill_names,
    render_loaded_skills, skill_origin_label,
};
use crate::ui::{CacheStats, ChatBanner, ChatUi, TurnStats};

/// Configuration for one chat session.
#[derive(Debug, Clone)]
pub struct ChatConfig {
    /// Workspace root. All file/shell tools operate relative to this path.
    /// Canonicalized at session start; must exist.
    pub workspace_root: PathBuf,
    /// Optional kimetsu project directory (contains `.kimetsu/`). When set,
    /// brain context is loaded for retrieval and any record_deviation calls
    /// surface as memory proposals at session end.
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
    /// Terminal renderer. CLI enables the rich renderer only when stdout is
    /// a terminal; tests and redirected output stay plain by default.
    pub ui: ChatUi,
    /// Agent Skills / Codex / Claude Code compatible skill loading.
    pub skills: SkillConfig,
    /// Use raw terminal key events for the input line. CLI enables this only
    /// when stdin/stdout are both terminals; tests and piped input stay
    /// line-buffered.
    pub raw_terminal_input: bool,
    /// Persist chat transcripts/checkpoints under `.kimetsu/chat/sessions`.
    /// CLI enables this; library tests default off to avoid workspace writes.
    pub persist_sessions: bool,
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
            ui: ChatUi::plain(),
            skills: SkillConfig::default(),
            raw_terminal_input: false,
            persist_sessions: false,
        }
    }
}

pub type ChatResult<T> = Result<T, ChatError>;

#[derive(Debug)]
pub enum ChatError {
    Io(std::io::Error),
    NoOauthToken,
    BrainProject(String),
    Skill(String),
    Provider(String),
    Quit,
}

impl std::fmt::Display for ChatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::NoOauthToken => write!(
                f,
                "CLAUDE_CODE_OAUTH_TOKEN is not set. Put it in the workspace .env or export it."
            ),
            Self::BrainProject(e) => write!(f, "brain project: {e}"),
            Self::Skill(e) => write!(f, "skills: {e}"),
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
/// the dependency direction (chat -> kimetsu-agent only, no benchmark adapter). The
/// model round-trip itself is plumbed in [`run_repl_with_agent`] which
/// lands in the v0.3.0 commit that wires the provider.
pub fn run_repl<R: BufRead, W: Write>(
    mut reader: R,
    mut writer: W,
    mut config: ChatConfig,
) -> ChatResult<()> {
    let workspace = config
        .workspace_root
        .canonicalize()
        .map_err(ChatError::Io)?;
    let mut skill_registry = SkillRegistry::discover(&workspace, &config.skills)
        .map_err(|err| ChatError::Skill(err.to_string()))?;
    let mut loaded_skills = skill_registry
        .load_selected(&config.skills.selected, &config.skills)
        .map_err(|err| ChatError::Skill(err.to_string()))?;
    let mut ui = config.ui;
    write_session_banner(
        &mut writer,
        ui,
        ChatBanner {
            version: env!("CARGO_PKG_VERSION"),
            workspace: &workspace,
            brain_project: config.brain_project.as_deref(),
            model: &config.model,
            budget: config.max_cost_usd,
            strict_verify: config.strict_verify,
            goal: config.goal.as_deref(),
            skills_loaded: loaded_skills.len(),
            skills_available: skill_registry.skills().len(),
        },
    )?;

    // Validate brain project up-front so we don't surprise the user
    // mid-session with a stale path error.
    if let Some(ref p) = config.brain_project {
        validate_brain_project(p)?;
    }
    let brain_session = match config.brain_project.as_ref() {
        Some(path) => Some(
            brain_project::BrainSession::open(path)
                .map_err(|err| ChatError::BrainProject(format!("{err}")))?,
        ),
        None => None,
    };

    // Per-session state.
    let _run_id = RunId::new();
    let mut cost = CostMeter::new(config.max_cost_usd);
    let mut goal = config.goal.clone();
    let mut strict_verify = config.strict_verify;
    let mut plan_mode = false;
    let mut raw_output_mode = false;
    let mut statusline_enabled = true;
    let mut input_state = ChatInputState::default();
    let mut permission_mode = PermissionMode::Auto;
    let mut reasoning_effort = ReasoningEffort::Medium;
    let session_dir = chat_session_dir(&workspace);
    let mut session = ChatSession::new(&config.model, permission_mode, goal.as_deref());
    let mut checkpoints: Vec<CheckpointRecord> = Vec::new();
    let hooks = HookRegistry::discover(&workspace);
    let mut task_manager = TaskManager::new(workspace.clone());
    let mut agent_manager = AgentTaskManager::new();
    if let Err(err) = hooks.run(HookEvent::SessionStart, &workspace, &session, None) {
        ui.write_error(&mut writer, &format!("hook session-start: {err}"))?;
    }

    // v0.3.1: provider is constructed lazily on first user message so
    // /help / /quit / /cost don't fail if CLAUDE_CODE_OAUTH_TOKEN is
    // unset. Same env handshake the benchmark adapter uses.
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
    // - closes the MP-18 feedback loop.
    let mut session_deviations: Vec<DeviationRecord> = Vec::new();

    loop {
        let reserved_palette_rows = if config.raw_terminal_input {
            if statusline_enabled {
                write_statusline(
                    &mut writer,
                    StatuslineView {
                        ui,
                        model: &config.model,
                        permission_mode,
                        effort: reasoning_effort,
                        spent: cost.spent(),
                        budget: cost.budget(),
                        turns: transcript.len(),
                        plan_mode,
                    },
                )?;
            }
            let rows = slash_palette_rows();
            write_interactive_prompt(&mut writer, ui, rows)?;
            rows
        } else {
            ui.write_prompt(&mut writer)?;
            0
        };
        writer.flush()?;
        let line = if config.raw_terminal_input {
            match read_terminal_line(
                &mut writer,
                ui,
                reserved_palette_rows,
                &mut input_state,
                &workspace,
                &skill_registry,
            )? {
                Some(line) => line,
                None => {
                    writeln!(writer, "\n(stdin closed; ending session)")?;
                    return Ok(());
                }
            }
        } else {
            let mut line = String::new();
            let n = reader.read_line(&mut line)?;
            if n == 0 {
                // EOF
                writeln!(writer, "\n(stdin closed; ending session)")?;
                return Ok(());
            }
            line
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        input_state.record(line);

        // Slash commands run inline (no model round-trip).
        if let Some(cmd) = SlashCommand::parse(line) {
            match cmd {
                SlashCommand::Help => {
                    SlashCommand::print_help(&mut writer)?;
                }
                SlashCommand::Clear => {
                    transcript.clear();
                    session.transcript.clear();
                    session.updated_at = now_unix();
                    persist_session(config.persist_sessions, &session_dir, &session, &transcript)?;
                    ui.clear_screen(&mut writer)?;
                    write_session_banner(
                        &mut writer,
                        ui,
                        ChatBanner {
                            version: env!("CARGO_PKG_VERSION"),
                            workspace: &workspace,
                            brain_project: config.brain_project.as_deref(),
                            model: &config.model,
                            budget: config.max_cost_usd,
                            strict_verify,
                            goal: goal.as_deref(),
                            skills_loaded: loaded_skills.len(),
                            skills_available: skill_registry.skills().len(),
                        },
                    )?;
                    ui.write_note(
                        &mut writer,
                        "[clear]",
                        "conversation context cleared; goal and cost meter kept.",
                    )?;
                }
                SlashCommand::Logo => {
                    write_session_banner(
                        &mut writer,
                        ui,
                        ChatBanner {
                            version: env!("CARGO_PKG_VERSION"),
                            workspace: &workspace,
                            brain_project: config.brain_project.as_deref(),
                            model: &config.model,
                            budget: config.max_cost_usd,
                            strict_verify,
                            goal: goal.as_deref(),
                            skills_loaded: loaded_skills.len(),
                            skills_available: skill_registry.skills().len(),
                        },
                    )?;
                }
                SlashCommand::Quit => {
                    let stopped_tasks = task_manager.stop_all();
                    if stopped_tasks > 0 {
                        ui.write_note(
                            &mut writer,
                            "[tasks]",
                            &format!("stopped {stopped_tasks} running task(s)"),
                        )?;
                    }
                    if let Err(err) = hooks.run(HookEvent::SessionEnd, &workspace, &session, None) {
                        ui.write_error(&mut writer, &format!("hook session-end: {err}"))?;
                    }
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
                SlashCommand::Status => {
                    handle_status_command(
                        &mut writer,
                        StatusView {
                            workspace: &workspace,
                            session: &session,
                            config: &config,
                            permission_mode,
                            reasoning_effort,
                            cost: &cost,
                            transcript: &transcript,
                            loaded_skills: &loaded_skills,
                        },
                    )?;
                }
                SlashCommand::Context => {
                    handle_context_command(
                        &mut writer,
                        brain_session.as_ref(),
                        &transcript,
                        &loaded_skills,
                        &checkpoints,
                    )?;
                }
                SlashCommand::Compact(arg) => {
                    match handle_compact_command(
                        &mut writer,
                        &workspace,
                        &config,
                        &arg,
                        &mut provider,
                        &mut cost,
                        &mut transcript,
                    ) {
                        Ok(true) => {
                            session.transcript = transcript.clone();
                            session.updated_at = now_unix();
                            persist_session(
                                config.persist_sessions,
                                &session_dir,
                                &session,
                                &transcript,
                            )?;
                        }
                        Ok(false) => {}
                        Err(err) => ui.write_error(&mut writer, &err.to_string())?,
                    }
                }
                SlashCommand::Plan(arg) => {
                    plan_mode = !matches!(
                        arg.trim().to_ascii_lowercase().as_str(),
                        "off" | "stop" | "clear" | "none" | "cancel"
                    );
                    if plan_mode {
                        let msg = if arg.trim().is_empty() {
                            "plan mode on; the next task will use read-only planning"
                        } else {
                            "plan mode on; send or refine the task to plan"
                        };
                        ui.write_note(&mut writer, "[plan]", msg)?;
                        if !arg.trim().is_empty() {
                            writeln!(writer, "planned task: {}", arg.trim())?;
                        }
                    } else {
                        ui.write_note(&mut writer, "[plan]", "plan mode off")?;
                    }
                }
                SlashCommand::Transcript(arg) => {
                    handle_transcript_command(&mut writer, &transcript, &arg)?;
                }
                SlashCommand::Copy(arg) => {
                    match handle_copy_command(&mut writer, &transcript, &arg) {
                        Ok(()) => {}
                        Err(err) => ui.write_error(&mut writer, &err.to_string())?,
                    }
                }
                SlashCommand::Diff(arg) => {
                    handle_diff_command(&mut writer, &workspace, &arg)?;
                }
                SlashCommand::Checkpoint(name) => {
                    match create_checkpoint(&workspace, &transcript, &name, "manual") {
                        Ok(checkpoint) => {
                            checkpoints.push(checkpoint.clone());
                            session.checkpoints = checkpoints.clone();
                            persist_session(
                                config.persist_sessions,
                                &session_dir,
                                &session,
                                &transcript,
                            )?;
                            ui.write_note(
                                &mut writer,
                                "[checkpoint]",
                                &format!("saved {}", checkpoint.id),
                            )?;
                        }
                        Err(err) => ui.write_error(&mut writer, &err.to_string())?,
                    }
                }
                SlashCommand::Undo => match undo_last_checkpoint(&workspace, &mut checkpoints) {
                    Ok(Some(checkpoint)) => {
                        let target_len = checkpoint.transcript_len.min(transcript.len());
                        transcript.truncate(target_len);
                        session.checkpoints = checkpoints.clone();
                        persist_session(
                            config.persist_sessions,
                            &session_dir,
                            &session,
                            &transcript,
                        )?;
                        ui.write_note(
                            &mut writer,
                            "[undo]",
                            &format!("restored {}", checkpoint.id),
                        )?;
                    }
                    Ok(None) => writeln!(writer, "no checkpoint available")?,
                    Err(err) => ui.write_error(&mut writer, &err.to_string())?,
                },
                SlashCommand::New(name) => {
                    persist_session(config.persist_sessions, &session_dir, &session, &transcript)?;
                    transcript.clear();
                    checkpoints.clear();
                    session = ChatSession::new(&config.model, permission_mode, goal.as_deref());
                    if !name.trim().is_empty() {
                        session.title = Some(name.trim().to_string());
                    }
                    persist_session(config.persist_sessions, &session_dir, &session, &transcript)?;
                    ui.write_note(&mut writer, "[session]", &format!("started {}", session.id))?;
                }
                SlashCommand::Resume(arg) => {
                    match handle_resume_command(&mut writer, &session_dir, &arg) {
                        Ok(Some(loaded)) => {
                            session = loaded;
                            transcript = session.transcript.clone();
                            checkpoints = session.checkpoints.clone();
                            config.model = session.model.clone();
                            permission_mode = session.permission_mode;
                            goal = session.goal.clone();
                            provider = None;
                            ui.write_note(
                                &mut writer,
                                "[session]",
                                &format!("loaded {}", session.id),
                            )?;
                        }
                        Ok(None) => {}
                        Err(err) => ui.write_error(&mut writer, &err.to_string())?,
                    }
                }
                SlashCommand::Export(path) => {
                    match export_transcript(&workspace, &session, &transcript, &path) {
                        Ok(path) => {
                            ui.write_note(&mut writer, "[export]", &path.display().to_string())?
                        }
                        Err(err) => ui.write_error(&mut writer, &err.to_string())?,
                    }
                }
                SlashCommand::Permissions(arg) => {
                    let selected = if arg.trim().is_empty() && config.raw_terminal_input {
                        pick_option(
                            &mut writer,
                            ui,
                            "permissions",
                            &[
                                ("read-only", "inspect files only"),
                                ("auto", "load edit/shell tools only when needed"),
                                ("full", "full tool surface from the first turn"),
                            ],
                        )?
                    } else {
                        None
                    };
                    let arg = selected.as_deref().unwrap_or(arg.trim());
                    if arg.is_empty() {
                        writeln!(
                            writer,
                            "permissions: {} ({})",
                            permission_mode.as_str(),
                            permission_mode.description()
                        )?;
                    } else {
                        match PermissionMode::parse(arg) {
                            Some(mode) => {
                                permission_mode = mode;
                                session.permission_mode = mode;
                                persist_session(
                                    config.persist_sessions,
                                    &session_dir,
                                    &session,
                                    &transcript,
                                )?;
                                ui.write_note(
                                    &mut writer,
                                    "[permissions]",
                                    &format!("{} - {}", mode.as_str(), mode.description()),
                                )?;
                            }
                            None => {
                                writeln!(writer, "usage: /permissions read-only | auto | full")?
                            }
                        }
                    }
                }
                SlashCommand::Model(arg) => {
                    let selected = if arg.trim().is_empty() && config.raw_terminal_input {
                        pick_option(
                            &mut writer,
                            ui,
                            "model",
                            &[
                                ("claude-opus-4-7", "Claude Code default provider"),
                                ("gpt-5.5", "OpenAI frontier model"),
                                ("gpt-5.4", "OpenAI coding/general model"),
                                ("gpt-5.3-codex", "Codex optimized model"),
                                ("gpt-5.3-codex-spark", "fast Codex iteration model"),
                                ("gpt-image-2", "image-capable OpenAI model"),
                            ],
                        )?
                    } else {
                        None
                    };
                    let arg = selected.as_deref().unwrap_or(arg.trim());
                    if arg.is_empty() {
                        writeln!(
                            writer,
                            "model: {}  image_generation={}",
                            config.model,
                            model_supports_image_generation(&config.model)
                        )?;
                    } else {
                        config.model = arg.to_string();
                        session.model = config.model.clone();
                        provider = None;
                        persist_session(
                            config.persist_sessions,
                            &session_dir,
                            &session,
                            &transcript,
                        )?;
                        ui.write_note(
                            &mut writer,
                            "[model]",
                            &format!(
                                "{} (image_generation={})",
                                config.model,
                                model_supports_image_generation(&config.model)
                            ),
                        )?;
                    }
                }
                SlashCommand::Effort(arg) => {
                    let selected = if arg.trim().is_empty() && config.raw_terminal_input {
                        pick_option(
                            &mut writer,
                            ui,
                            "effort",
                            &[
                                ("low", "fastest responses"),
                                ("medium", "balanced default"),
                                ("high", "more deliberate work"),
                                ("xhigh", "maximum reasoning depth"),
                            ],
                        )?
                    } else {
                        None
                    };
                    let arg = selected.as_deref().unwrap_or(arg.trim());
                    if arg.is_empty() {
                        writeln!(writer, "effort: {}", reasoning_effort.as_str())?;
                    } else {
                        match ReasoningEffort::parse(arg) {
                            Some(effort) => {
                                reasoning_effort = effort;
                                session.reasoning_effort = effort;
                                persist_session(
                                    config.persist_sessions,
                                    &session_dir,
                                    &session,
                                    &transcript,
                                )?;
                                ui.write_note(&mut writer, "[effort]", reasoning_effort.as_str())?;
                            }
                            None => writeln!(writer, "usage: /effort low | medium | high | xhigh")?,
                        }
                    }
                }
                SlashCommand::Theme(arg) => match arg.trim().to_ascii_lowercase().as_str() {
                    "" => writeln!(
                        writer,
                        "theme: {}",
                        if ui.is_rich() { "rich" } else { "plain" }
                    )?,
                    "rich" | "dragon" => {
                        ui = ChatUi::rich().with_logo(ui.show_logo);
                        config.ui = ui;
                        ui.write_note(&mut writer, "[theme]", "rich")?;
                    }
                    "plain" | "raw" => {
                        ui = ChatUi::plain().with_logo(ui.show_logo);
                        config.ui = ui;
                        ui.write_note(&mut writer, "[theme]", "plain")?;
                    }
                    other => writeln!(writer, "unknown theme `{other}`. Use /theme rich | plain")?,
                },
                SlashCommand::StatusLine(arg) => {
                    if let Some(value) = parse_toggle(arg.trim()) {
                        statusline_enabled = value;
                    }
                    writeln!(
                        writer,
                        "statusline: {}",
                        if statusline_enabled { "on" } else { "off" }
                    )?;
                }
                SlashCommand::Raw(arg) => {
                    if let Some(value) = parse_toggle(arg.trim()) {
                        raw_output_mode = value;
                    } else if !arg.trim().is_empty() {
                        writeln!(writer, "usage: /raw on | off")?;
                        continue;
                    } else {
                        raw_output_mode = !raw_output_mode;
                    }
                    writeln!(
                        writer,
                        "raw scrollback mode: {}",
                        if raw_output_mode { "on" } else { "off" }
                    )?;
                }
                SlashCommand::Keybindings => {
                    handle_keybindings_command(&mut writer)?;
                }
                SlashCommand::Goal(g) => {
                    if g.is_empty() {
                        match goal.as_ref() {
                            Some(g) => writeln!(writer, "goal: {g}")?,
                            None => writeln!(writer, "goal: <none>")?,
                        }
                    } else {
                        goal = Some(g);
                        session.goal = goal.clone();
                        persist_session(
                            config.persist_sessions,
                            &session_dir,
                            &session,
                            &transcript,
                        )?;
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
                                        writeln!(
                                            writer,
                                            "  ... ({} more; use `kimetsu brain memory list` for full)",
                                            rows.len() - 20
                                        )?;
                                    }
                                }
                                Err(e) => ui.write_error(&mut writer, &e.to_string())?,
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
                SlashCommand::Skills(arg) => {
                    handle_skills_command(
                        &arg,
                        &mut skill_registry,
                        &config.skills,
                        &mut loaded_skills,
                        ui,
                        config.raw_terminal_input,
                        &mut writer,
                    )?;
                }
                SlashCommand::Run(arg) => {
                    handle_run_command(
                        &mut writer,
                        &workspace,
                        permission_mode,
                        config.raw_terminal_input,
                        &arg,
                        false,
                    )?;
                }
                SlashCommand::Verify(arg) => {
                    handle_run_command(
                        &mut writer,
                        &workspace,
                        permission_mode,
                        config.raw_terminal_input,
                        &arg,
                        true,
                    )?;
                }
                SlashCommand::Image(prompt) => {
                    match handle_image_command(&mut writer, &workspace, &config.model, &prompt) {
                        Ok(()) => {}
                        Err(err) => ui.write_error(&mut writer, &err.to_string())?,
                    }
                }
                SlashCommand::Hooks(arg) => {
                    handle_hooks_command(&mut writer, &workspace, &session, &hooks, &arg)?;
                }
                SlashCommand::Mcp(arg) => {
                    handle_mcp_command(&mut writer, &workspace, &arg)?;
                }
                SlashCommand::Bridge(arg) => {
                    handle_bridge_command(&mut writer, &workspace, &config.skills, &arg)?;
                }
                SlashCommand::Agents(arg) => {
                    let mut agents_ctx = AgentCommandContext {
                        workspace: &workspace,
                        config: &config,
                        provider: &mut provider,
                        cost: &mut cost,
                        transcript: &mut transcript,
                        agent_manager: &mut agent_manager,
                    };
                    handle_agents_command(&mut writer, &mut agents_ctx, &arg)?;
                    session.transcript = transcript.clone();
                    persist_session(config.persist_sessions, &session_dir, &session, &transcript)?;
                }
                SlashCommand::Tasks(arg) => {
                    handle_tasks_command(
                        &mut writer,
                        permission_mode,
                        config.raw_terminal_input,
                        &mut task_manager,
                        &arg,
                    )?;
                }
                SlashCommand::Review(arg) => {
                    let mut review_ctx = ReviewCommandContext {
                        workspace: &workspace,
                        config: &config,
                        provider: &mut provider,
                        cost: &mut cost,
                        transcript: &mut transcript,
                    };
                    handle_review_command(&mut writer, &mut review_ctx, &arg, "review")?;
                    session.transcript = transcript.clone();
                    session.updated_at = now_unix();
                    persist_session(config.persist_sessions, &session_dir, &session, &transcript)?;
                }
                SlashCommand::SecurityReview(arg) => {
                    let mut review_ctx = ReviewCommandContext {
                        workspace: &workspace,
                        config: &config,
                        provider: &mut provider,
                        cost: &mut cost,
                        transcript: &mut transcript,
                    };
                    handle_review_command(&mut writer, &mut review_ctx, &arg, "security-review")?;
                    session.transcript = transcript.clone();
                    session.updated_at = now_unix();
                    persist_session(config.persist_sessions, &session_dir, &session, &transcript)?;
                }
                SlashCommand::Simplify(arg) => {
                    let mut review_ctx = ReviewCommandContext {
                        workspace: &workspace,
                        config: &config,
                        provider: &mut provider,
                        cost: &mut cost,
                        transcript: &mut transcript,
                    };
                    handle_review_command(&mut writer, &mut review_ctx, &arg, "simplify")?;
                    session.transcript = transcript.clone();
                    session.updated_at = now_unix();
                    persist_session(config.persist_sessions, &session_dir, &session, &transcript)?;
                }
                SlashCommand::Doctor => {
                    handle_doctor_command(&mut writer, &workspace, &config, &session_dir)?;
                }
                SlashCommand::DebugConfig => {
                    handle_debug_config_command(&mut writer, &workspace, &config, &session_dir)?;
                }
            }
            continue;
        }

        if let Some(shell_command) = line.strip_prefix('!') {
            match handle_shell_prefix_command(
                &mut writer,
                &workspace,
                permission_mode,
                shell_command,
                &mut transcript,
            ) {
                Ok(Some(reply)) => {
                    session.transcript = transcript.clone();
                    session.updated_at = now_unix();
                    persist_session(config.persist_sessions, &session_dir, &session, &transcript)?;
                    ui.write_assistant(&mut writer, &reply)?;
                }
                Ok(None) => {}
                Err(err) => ui.write_error(&mut writer, &err.to_string())?,
            }
            continue;
        }

        let mut user_line = line.to_string();
        let mut model_line = line.to_string();
        if let Some((skill_name, prompt)) = parse_skill_prefix(line) {
            match skill_registry.load_into(&skill_name, &mut loaded_skills, &config.skills) {
                Ok(true) => {
                    ui.write_note(&mut writer, "[skills]", &format!("loaded `{skill_name}`"))?
                }
                Ok(false) => {}
                Err(err) => {
                    ui.write_error(&mut writer, &err)?;
                    continue;
                }
            }
            if prompt.trim().is_empty() {
                continue;
            }
            user_line = line.to_string();
            model_line = format!(
                "Use skill `{skill_name}` for this request:\n{}",
                prompt.trim()
            );
        } else if let Some(expanded) = expand_file_mentions(line, &workspace) {
            model_line = expanded;
        }

        let route = if plan_mode {
            ChatRoute::WorkspaceRead
        } else {
            classify_chat_route(&model_line, goal.as_deref(), !loaded_skills.is_empty())
        };
        if let ChatRoute::Local(reply) = &route {
            if let Err(err) = hooks.run(HookEvent::PreTurn, &workspace, &session, Some(&user_line))
            {
                ui.write_error(&mut writer, &format!("hook pre-turn: {err}"))?;
            }
            ui.write_assistant(&mut writer, reply)?;
            transcript.push(TurnRecord {
                user: user_line.clone(),
                assistant: reply.clone(),
            });
            session.transcript = transcript.clone();
            session.updated_at = now_unix();
            persist_session(config.persist_sessions, &session_dir, &session, &transcript)?;
            if let Err(err) = hooks.run(HookEvent::PostTurn, &workspace, &session, Some(&user_line))
            {
                ui.write_error(&mut writer, &format!("hook post-turn: {err}"))?;
            }
            continue;
        }

        if route == ChatRoute::WorkspaceAgent && permission_mode == PermissionMode::ReadOnly {
            ui.write_error(
                &mut writer,
                "read-only permission mode blocks editing and shell-agent turns. Use `/permissions auto` or `/permissions full` for this task.",
            )?;
            continue;
        }

        // Budget gate before spending any tokens.
        if cost.over_budget() {
            ui.write_budget_stop(&mut writer, cost.spent(), cost.budget())?;
            continue;
        }

        // Construct provider lazily on first user message. Failure is
        // non-fatal: print the error, let the user fix env vars (or
        // /quit) without exiting the REPL outright.
        if provider.is_none() {
            match build_chat_provider(&config, &workspace) {
                Ok(p) => provider = Some(p),
                Err(e) => {
                    ui.write_error(&mut writer, &e.to_string())?;
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
                trace_fsync: false,
                ..ToolRuntimeConfig::default()
            }),
            Err(e) => {
                ui.write_error(&mut writer, &format!("tool runtime: {e}"))?;
                continue;
            }
        };

        // v0.3.3: build the task + brain-context for this turn.
        //
        // task:      the user's new message (with goal preamble if set)
        // context:   accumulated transcript of prior turns + retrieved
        //            brain capsules (if --project is set). The agent
        //            loop renders this as a "Prior knowledge" block
        //            before the user message - giving the model
        //            conversational memory + memory-pool retrieval in
        //            one channel.
        let mut task = match goal.as_ref() {
            Some(g) if !g.is_empty() => format!("Goal: {g}\nMessage: {model_line}"),
            _ => model_line.clone(),
        };
        if plan_mode {
            task = format!(
                "PLAN MODE: inspect and reason in read-only mode. Produce a concrete plan, risks, and verification steps. Do not edit files or run write commands.\n{task}"
            );
        }
        if reasoning_effort != ReasoningEffort::Medium {
            task = format!(
                "Reasoning effort hint: {}\n{task}",
                reasoning_effort.as_str()
            );
        }
        let brain_context =
            build_chat_brain_context(brain_session.as_ref(), &transcript, &loaded_skills, &task);

        if route == ChatRoute::TextOnly {
            if let Err(err) = hooks.run(HookEvent::PreTurn, &workspace, &session, Some(&user_line))
            {
                ui.write_error(&mut writer, &format!("hook pre-turn: {err}"))?;
            }
            ui.write_thinking_text(&mut writer)?;
            writer.flush()?;
            match run_text_only_chat(
                provider.as_mut().expect("provider built above").as_mut(),
                &task,
                &transcript,
            ) {
                Ok(report) => {
                    let still_within = cost.record_turn(report.usage.cost_usd);
                    write_assistant_output(&mut writer, ui, raw_output_mode, &report.text)?;
                    transcript.push(TurnRecord {
                        user: user_line.clone(),
                        assistant: report.text.clone(),
                    });
                    session.transcript = transcript.clone();
                    session.updated_at = now_unix();
                    persist_session(config.persist_sessions, &session_dir, &session, &transcript)?;
                    if let Err(err) =
                        hooks.run(HookEvent::PostTurn, &workspace, &session, Some(&user_line))
                    {
                        ui.write_error(&mut writer, &format!("hook post-turn: {err}"))?;
                    }
                    ui.write_turn_stats(
                        &mut writer,
                        TurnStats {
                            turn_cost: report.usage.cost_usd,
                            total_cost: cost.spent(),
                            budget: cost.budget(),
                            turns: 1,
                            tool_calls: 0,
                        },
                    )?;
                    if report.usage.cache_creation_input_tokens > 0
                        || report.usage.cache_read_input_tokens > 0
                    {
                        ui.write_cache_stats(
                            &mut writer,
                            CacheStats {
                                input_tokens: report.usage.input_tokens,
                                output_tokens: report.usage.output_tokens,
                                cache_write: report.usage.cache_creation_input_tokens,
                                cache_read: report.usage.cache_read_input_tokens,
                            },
                        )?;
                    }
                    if !still_within {
                        ui.write_note(
                            &mut writer,
                            "[budget]",
                            "crossed budget; next message will be refused.",
                        )?;
                    }
                }
                Err(err) => ui.write_error(&mut writer, &err.to_string())?,
            }
            continue;
        }

        let mut turn_checkpoint = if route == ChatRoute::WorkspaceAgent {
            match create_checkpoint(&workspace, &transcript, "", "turn") {
                Ok(checkpoint) => Some(checkpoint),
                Err(err) => {
                    ui.write_error(&mut writer, &format!("checkpoint: {err}"))?;
                    None
                }
            }
        } else {
            None
        };

        if let Err(err) = hooks.run(HookEvent::PreTurn, &workspace, &session, Some(&user_line)) {
            ui.write_error(&mut writer, &format!("hook pre-turn: {err}"))?;
        }

        let opts = KimetsuAgentOpts {
            turn_budget: match route {
                ChatRoute::WorkspaceRead => 12,
                ChatRoute::WorkspaceAgent => match permission_mode {
                    PermissionMode::Full => 80,
                    PermissionMode::Auto | PermissionMode::ReadOnly => 40,
                },
                ChatRoute::Local(_) | ChatRoute::TextOnly => 1,
            },
            mode: "chat",
            task_source: "user",
            auto_orient: transcript.is_empty(),
            min_actions_before_finish: match route {
                ChatRoute::WorkspaceAgent => 1,
                _ => 0,
            },
            tool_loadout: match route {
                ChatRoute::WorkspaceRead => ToolLoadout::ReadOnly,
                ChatRoute::WorkspaceAgent => match permission_mode {
                    PermissionMode::Full => ToolLoadout::Full,
                    PermissionMode::Auto => ToolLoadout::Dynamic,
                    PermissionMode::ReadOnly => ToolLoadout::ReadOnly,
                },
                ChatRoute::Local(_) | ChatRoute::TextOnly => ToolLoadout::ReadOnly,
            },
            strict_verify: Some(strict_verify),
            self_verify_nudge_enabled: route == ChatRoute::WorkspaceAgent,
        };
        ui.write_thinking(&mut writer)?;
        writer.flush()?;
        let result = run_model_agent(
            &task,
            &mut runtime,
            provider.as_mut().expect("provider built above").as_mut(),
            opts,
            brain_context.as_deref(),
        );

        match result {
            Ok(report) => {
                let turn_cost = report.usage.cost_usd;
                let still_within = cost.record_turn(turn_cost);
                let assistant_text = report
                    .final_text
                    .clone()
                    .unwrap_or_else(|| report.summary.clone());
                write_assistant_output(&mut writer, ui, raw_output_mode, &assistant_text)?;

                // v0.3.3: append this turn to the conversation transcript
                // so subsequent turns see prior context. Truncate the
                // user/assistant text to keep the brain_context block
                // bounded across long sessions.
                transcript.push(TurnRecord {
                    user: user_line.clone(),
                    assistant: assistant_text.clone(),
                });
                if let Some(mut checkpoint) = turn_checkpoint.take() {
                    if let Ok(diff) = current_git_diff(&workspace) {
                        checkpoint.post_diff = diff;
                    }
                    checkpoints.push(checkpoint);
                }
                session.transcript = transcript.clone();
                session.checkpoints = checkpoints.clone();
                session.updated_at = now_unix();
                persist_session(config.persist_sessions, &session_dir, &session, &transcript)?;
                if let Err(err) =
                    hooks.run(HookEvent::PostTurn, &workspace, &session, Some(&user_line))
                {
                    ui.write_error(&mut writer, &format!("hook post-turn: {err}"))?;
                }

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
                ui.write_turn_stats(
                    &mut writer,
                    TurnStats {
                        turn_cost,
                        total_cost: cost.spent(),
                        budget: cost.budget(),
                        turns: report.turns,
                        tool_calls: report.tool_calls,
                    },
                )?;
                // v0.3.4a: print Anthropic prompt-cache stats so the
                // user can see whether the prompt cache is landing
                // across turns. cache_read should grow over the session
                // once persistent-subprocess (v0.3.4e) lands; today
                // most calls show cache_creation > 0, cache_read == 0.
                let cc = report.usage.cache_creation_input_tokens;
                let cr = report.usage.cache_read_input_tokens;
                if cc > 0 || cr > 0 {
                    ui.write_cache_stats(
                        &mut writer,
                        CacheStats {
                            input_tokens: report.usage.input_tokens,
                            output_tokens: report.usage.output_tokens,
                            cache_write: cc,
                            cache_read: cr,
                        },
                    )?;
                }
                if !still_within {
                    ui.write_note(
                        &mut writer,
                        "[budget]",
                        "crossed budget; next message will be refused.",
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
                    ui.write_note(
                        &mut writer,
                        "[deviations]",
                        &format!(
                            "{dev_count} recorded this turn (will be added to brain on /quit)"
                        ),
                    )?;
                }
            }
            Err(e) => {
                ui.write_error(&mut writer, &e.to_string())?;
            }
        }
    }
}

fn write_session_banner<W: Write>(
    writer: &mut W,
    ui: ChatUi,
    banner: ChatBanner<'_>,
) -> ChatResult<()> {
    ui.write_banner(writer, banner)?;
    Ok(())
}

fn write_assistant_output<W: Write>(
    writer: &mut W,
    ui: ChatUi,
    raw_output_mode: bool,
    text: &str,
) -> ChatResult<()> {
    if raw_output_mode {
        writeln!(writer, "\n{}", text.trim_end())?;
    } else {
        ui.write_assistant(writer, text)?;
    }
    Ok(())
}

const SLASH_PALETTE_MAX_ROWS: u16 = 8;

fn slash_palette_rows() -> u16 {
    size()
        .map(|(_, rows)| rows.saturating_sub(3).clamp(4, SLASH_PALETTE_MAX_ROWS))
        .unwrap_or(SLASH_PALETTE_MAX_ROWS)
}

fn write_interactive_prompt<W: Write>(
    writer: &mut W,
    ui: ChatUi,
    palette_rows: u16,
) -> ChatResult<()> {
    writeln!(writer)?;
    for _ in 0..palette_rows {
        queue!(writer, MoveToColumn(0), Clear(ClearType::CurrentLine)).map_err(ChatError::Io)?;
        writeln!(writer)?;
    }
    ui.write_prompt_inline(writer)?;
    Ok(())
}

#[derive(Debug, Default)]
struct ChatInputState {
    history: Vec<String>,
}

impl ChatInputState {
    fn record(&mut self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        if self.history.last().map(String::as_str) == Some(line) {
            return;
        }
        self.history.push(line.to_string());
        const MAX_INPUT_HISTORY: usize = 500;
        if self.history.len() > MAX_INPUT_HISTORY {
            self.history.remove(0);
        }
    }

    fn previous(&self, cursor: Option<usize>) -> Option<(usize, String)> {
        if self.history.is_empty() {
            return None;
        }
        let idx = cursor
            .map(|idx| idx.saturating_sub(1))
            .unwrap_or_else(|| self.history.len().saturating_sub(1));
        self.history.get(idx).cloned().map(|line| (idx, line))
    }

    fn next(&self, cursor: Option<usize>) -> Option<(Option<usize>, String)> {
        let idx = cursor?;
        if idx + 1 < self.history.len() {
            Some((Some(idx + 1), self.history[idx + 1].clone()))
        } else {
            Some((None, String::new()))
        }
    }

    fn reverse_match(&self, query: &str, cursor: Option<usize>) -> Option<(usize, String)> {
        if self.history.is_empty() {
            return None;
        }
        let end = cursor.unwrap_or(self.history.len()).min(self.history.len());
        let query = query.trim().trim_start_matches('!');
        self.history[..end]
            .iter()
            .enumerate()
            .rev()
            .find(|(_, item)| query.is_empty() || item.contains(query))
            .map(|(idx, item)| (idx, item.clone()))
    }
}

fn read_terminal_line<W: Write>(
    writer: &mut W,
    ui: ChatUi,
    palette_rows: u16,
    input_state: &mut ChatInputState,
    workspace: &Path,
    skill_registry: &SkillRegistry,
) -> ChatResult<Option<String>> {
    let _raw = RawModeGuard::enable()?;
    let mut buffer: Vec<char> = Vec::new();
    let mut cursor = 0usize;
    let mut slash_palette_open = false;
    let mut slash_selection = 0usize;
    let mut history_cursor: Option<usize> = None;
    let mut reverse_cursor: Option<usize> = None;

    loop {
        let event = read().map_err(ChatError::Io)?;
        let Event::Key(key) = event else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }

        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if slash_palette_open {
                    clear_transient_input_area(writer, palette_rows)?;
                }
                if !buffer.is_empty() {
                    buffer.clear();
                    cursor = 0;
                    slash_palette_open = false;
                    redraw_prompt_chars(writer, ui, &buffer, cursor)?;
                    continue;
                }
                write!(writer, "^C\r\n")?;
                writer.flush()?;
                return Ok(Some("/quit".to_string()));
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if buffer.is_empty() {
                    write!(writer, "\r\n")?;
                    writer.flush()?;
                    return Ok(Some("/quit".to_string()));
                }
                if cursor < buffer.len() {
                    buffer.remove(cursor);
                    redraw_prompt_chars(writer, ui, &buffer, cursor)?;
                }
            }
            KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if slash_palette_open {
                    clear_transient_input_area(writer, palette_rows)?;
                    slash_palette_open = false;
                }
                ui.clear_screen(writer)?;
                redraw_prompt_chars(writer, ui, &buffer, cursor)?;
            }
            KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let query = buffer_chars_to_string(&buffer);
                if let Some((idx, line)) = input_state.reverse_match(&query, reverse_cursor) {
                    buffer = line.chars().collect();
                    cursor = buffer.len();
                    history_cursor = Some(idx);
                    reverse_cursor = Some(idx);
                    slash_palette_open = buffer.first() == Some(&'/');
                    redraw_prompt_chars(writer, ui, &buffer, cursor)?;
                    if slash_palette_open {
                        slash_selection = clamp_slash_selection(
                            &buffer_chars_to_string(&buffer),
                            slash_selection,
                        );
                        render_slash_palette(
                            writer,
                            ui,
                            &buffer_chars_to_string(&buffer),
                            slash_selection,
                            palette_rows,
                        )?;
                    }
                }
            }
            KeyCode::Char('g') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if slash_palette_open {
                    clear_transient_input_area(writer, palette_rows)?;
                    slash_palette_open = false;
                }
                match edit_prompt_in_editor(workspace, &buffer_chars_to_string(&buffer)) {
                    Ok(Some(edited)) => {
                        buffer = edited.chars().collect();
                        cursor = buffer.len();
                        redraw_prompt_chars(writer, ui, &buffer, cursor)?;
                    }
                    Ok(None) => redraw_prompt_chars(writer, ui, &buffer, cursor)?,
                    Err(err) => {
                        redraw_prompt_chars(writer, ui, &buffer, cursor)?;
                        writeln!(writer, "\r\n[editor] {err}")?;
                        redraw_prompt_chars(writer, ui, &buffer, cursor)?;
                    }
                }
            }
            KeyCode::Enter
                if key.modifiers.contains(KeyModifiers::SHIFT)
                    || key.modifiers.contains(KeyModifiers::ALT)
                    || key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                buffer.insert(cursor, '\n');
                cursor += 1;
                redraw_prompt_chars(writer, ui, &buffer, cursor)?;
            }
            KeyCode::Char(ch)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                let was_empty = buffer.is_empty();
                buffer.insert(cursor, ch);
                cursor += 1;
                let rendered = buffer_chars_to_string(&buffer);
                if was_empty && ch == '/' {
                    slash_palette_open = true;
                    slash_selection = 0;
                    redraw_prompt_chars(writer, ui, &buffer, cursor)?;
                    render_slash_palette(writer, ui, &rendered, slash_selection, palette_rows)?;
                } else if slash_palette_open {
                    slash_selection = clamp_slash_selection(&rendered, slash_selection);
                    redraw_prompt_chars(writer, ui, &buffer, cursor)?;
                    render_slash_palette(writer, ui, &rendered, slash_selection, palette_rows)?;
                } else {
                    redraw_prompt_chars(writer, ui, &buffer, cursor)?;
                }
                history_cursor = None;
                reverse_cursor = None;
            }
            KeyCode::Backspace if cursor > 0 => {
                cursor -= 1;
                buffer.remove(cursor);
                if slash_palette_open {
                    let rendered = buffer_chars_to_string(&buffer);
                    if rendered.starts_with('/') {
                        slash_selection = clamp_slash_selection(&rendered, slash_selection);
                        redraw_prompt_chars(writer, ui, &buffer, cursor)?;
                        render_slash_palette(writer, ui, &rendered, slash_selection, palette_rows)?;
                    } else {
                        slash_palette_open = false;
                        clear_transient_input_area(writer, palette_rows)?;
                        redraw_prompt_chars(writer, ui, &buffer, cursor)?;
                    }
                } else {
                    redraw_prompt_chars(writer, ui, &buffer, cursor)?;
                }
            }
            KeyCode::Delete if cursor < buffer.len() => {
                buffer.remove(cursor);
                redraw_prompt_chars(writer, ui, &buffer, cursor)?;
            }
            KeyCode::Left if cursor > 0 => {
                cursor -= 1;
                redraw_prompt_chars(writer, ui, &buffer, cursor)?;
            }
            KeyCode::Right if cursor < buffer.len() => {
                cursor += 1;
                redraw_prompt_chars(writer, ui, &buffer, cursor)?;
            }
            KeyCode::Home => {
                cursor = 0;
                redraw_prompt_chars(writer, ui, &buffer, cursor)?;
            }
            KeyCode::End => {
                cursor = buffer.len();
                redraw_prompt_chars(writer, ui, &buffer, cursor)?;
            }
            KeyCode::Up if slash_palette_open => {
                let rendered = buffer_chars_to_string(&buffer);
                let count = matching_slash_commands(&rendered).len();
                if count > 0 {
                    slash_selection = if slash_selection == 0 {
                        count - 1
                    } else {
                        slash_selection - 1
                    };
                    render_slash_palette(writer, ui, &rendered, slash_selection, palette_rows)?;
                }
            }
            KeyCode::Down if slash_palette_open => {
                let rendered = buffer_chars_to_string(&buffer);
                let count = matching_slash_commands(&rendered).len();
                if count > 0 {
                    slash_selection = (slash_selection + 1) % count;
                    render_slash_palette(writer, ui, &rendered, slash_selection, palette_rows)?;
                }
            }
            KeyCode::Up => {
                if let Some((idx, line)) = input_state.previous(history_cursor) {
                    history_cursor = Some(idx);
                    reverse_cursor = None;
                    buffer = line.chars().collect();
                    cursor = buffer.len();
                    slash_palette_open = buffer.first() == Some(&'/');
                    redraw_prompt_chars(writer, ui, &buffer, cursor)?;
                }
            }
            KeyCode::Down => {
                if let Some((next, line)) = input_state.next(history_cursor) {
                    history_cursor = next;
                    reverse_cursor = None;
                    buffer = line.chars().collect();
                    cursor = buffer.len();
                    slash_palette_open = buffer.first() == Some(&'/');
                    redraw_prompt_chars(writer, ui, &buffer, cursor)?;
                }
            }
            KeyCode::Tab if slash_palette_open => {
                let rendered = buffer_chars_to_string(&buffer);
                if let Some(completion) = selected_slash_completion(&rendered, slash_selection) {
                    buffer = completion.chars().collect();
                    cursor = buffer.len();
                    let rendered = buffer_chars_to_string(&buffer);
                    slash_selection = clamp_slash_selection(&rendered, slash_selection);
                    redraw_prompt_chars(writer, ui, &buffer, cursor)?;
                    render_slash_palette(writer, ui, &rendered, slash_selection, palette_rows)?;
                }
            }
            KeyCode::Tab => {
                if let Some(completed) =
                    complete_inline_mention(&buffer, cursor, workspace, skill_registry)
                {
                    buffer = completed.chars().collect();
                    cursor = buffer.len();
                    redraw_prompt_chars(writer, ui, &buffer, cursor)?;
                } else if buffer.first() == Some(&'!') {
                    let query = buffer_chars_to_string(&buffer);
                    if let Some((idx, line)) = input_state.reverse_match(&query, None) {
                        history_cursor = Some(idx);
                        buffer = line.chars().collect();
                        cursor = buffer.len();
                        redraw_prompt_chars(writer, ui, &buffer, cursor)?;
                    }
                }
            }
            KeyCode::Enter => {
                if slash_palette_open {
                    let rendered = buffer_chars_to_string(&buffer);
                    if let Some(completion) =
                        selected_slash_completion_for_enter(&rendered, slash_selection)
                    {
                        buffer = completion.chars().collect();
                        cursor = buffer.len();
                        if completion.ends_with(' ') {
                            let rendered = buffer_chars_to_string(&buffer);
                            slash_selection = clamp_slash_selection(&rendered, slash_selection);
                            redraw_prompt_chars(writer, ui, &buffer, cursor)?;
                            render_slash_palette(
                                writer,
                                ui,
                                &rendered,
                                slash_selection,
                                palette_rows,
                            )?;
                            continue;
                        }
                    }
                    clear_transient_input_area(writer, palette_rows)?;
                }
                write!(writer, "\r\n")?;
                writer.flush()?;
                let rendered = buffer_chars_to_string(&buffer);
                if slash_palette_open && rendered.trim() == "/" {
                    return Ok(Some("/help".to_string()));
                }
                return Ok(Some(rendered));
            }
            KeyCode::Esc => {
                if slash_palette_open {
                    clear_transient_input_area(writer, palette_rows)?;
                    slash_palette_open = false;
                    redraw_prompt_chars(writer, ui, &buffer, cursor)?;
                    continue;
                }
                if !buffer.is_empty() {
                    buffer.clear();
                    cursor = 0;
                    redraw_prompt_chars(writer, ui, &buffer, cursor)?;
                } else {
                    write!(writer, "\r\n")?;
                    writer.flush()?;
                    return Ok(Some(String::new()));
                }
            }
            _ => {}
        }
    }
}

fn buffer_chars_to_string(buffer: &[char]) -> String {
    buffer.iter().collect()
}

fn display_buffer(buffer: &[char]) -> String {
    buffer
        .iter()
        .flat_map(|ch| {
            if *ch == '\n' {
                ['\\', 'n']
            } else {
                [*ch, '\0']
            }
        })
        .filter(|ch| *ch != '\0')
        .collect()
}

fn display_cursor_column(ui: ChatUi, buffer: &[char], cursor: usize) -> u16 {
    let prompt = prompt_visible_width(ui);
    let body = buffer
        .iter()
        .take(cursor)
        .map(|ch| if *ch == '\n' { 2 } else { 1 })
        .sum::<usize>();
    prompt.saturating_add(body).min(u16::MAX as usize) as u16
}

fn prompt_visible_width(ui: ChatUi) -> usize {
    if ui.is_rich() { 6 } else { 5 }
}

fn redraw_prompt_chars<W: Write>(
    writer: &mut W,
    ui: ChatUi,
    buffer: &[char],
    cursor: usize,
) -> ChatResult<()> {
    queue!(writer, MoveToColumn(0), Clear(ClearType::CurrentLine)).map_err(ChatError::Io)?;
    ui.write_prompt_inline(writer)?;
    write!(writer, "{}", display_buffer(buffer))?;
    queue!(
        writer,
        MoveToColumn(display_cursor_column(ui, buffer, cursor))
    )
    .map_err(ChatError::Io)?;
    writer.flush()?;
    Ok(())
}

fn edit_prompt_in_editor(workspace: &Path, current: &str) -> ChatResult<Option<String>> {
    let editor = std::env::var("VISUAL")
        .ok()
        .or_else(|| std::env::var("EDITOR").ok())
        .or_else(|| {
            if cfg!(windows) {
                Some("notepad".to_string())
            } else {
                Some("vi".to_string())
            }
        })
        .unwrap_or_else(|| "vi".to_string());
    let dir = workspace.join(".kimetsu").join("chat");
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("prompt-{}.md", now_unix()));
    fs::write(&path, current)?;
    let _ = disable_raw_mode();
    let status = Command::new(editor)
        .arg(&path)
        .current_dir(workspace)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status();
    let _ = disable_raw_mode();
    match status {
        Ok(status) if status.success() => Ok(Some(fs::read_to_string(&path)?)),
        Ok(_) => Ok(None),
        Err(err) => Err(ChatError::Io(err)),
    }
}

fn complete_inline_mention(
    buffer: &[char],
    cursor: usize,
    workspace: &Path,
    skill_registry: &SkillRegistry,
) -> Option<String> {
    let text = buffer_chars_to_string(buffer);
    let before = text.chars().take(cursor).collect::<String>();
    let token_start = before
        .rfind(char::is_whitespace)
        .map(|idx| idx + 1)
        .unwrap_or(0);
    let token = &before[token_start..];
    if let Some(query) = token.strip_prefix('@') {
        let completion = complete_path_token(workspace, query)?;
        return Some(format!(
            "{}@{}{}",
            &before[..token_start],
            completion,
            &text[before.len()..]
        ));
    }
    if let Some(query) = token.strip_prefix('$') {
        let completion = complete_skill_token(skill_registry, query)?;
        return Some(format!(
            "{}${}{}",
            &before[..token_start],
            completion,
            &text[before.len()..]
        ));
    }
    None
}

fn complete_path_token(workspace: &Path, query: &str) -> Option<String> {
    let query = query.replace('\\', "/").to_ascii_lowercase();
    let mut candidates = discover_files(workspace, &["."], 4)
        .into_iter()
        .filter_map(|path| {
            path.strip_prefix(workspace)
                .ok()
                .map(|rel| rel.to_string_lossy().replace('\\', "/"))
        })
        .filter(|rel| !rel.starts_with(".git/") && rel.to_ascii_lowercase().starts_with(&query))
        .collect::<Vec<_>>();
    candidates.sort_by_key(|candidate| {
        (
            candidate.matches('/').count(),
            candidate.len(),
            candidate.clone(),
        )
    });
    candidates.first().cloned()
}

fn complete_skill_token(skill_registry: &SkillRegistry, query: &str) -> Option<String> {
    let query = query.to_ascii_lowercase();
    skill_registry
        .skills()
        .iter()
        .filter(|skill| skill.name.to_ascii_lowercase().starts_with(&query))
        .map(|skill| skill.name.clone())
        .next()
}

fn pick_option<W: Write>(
    writer: &mut W,
    ui: ChatUi,
    title: &str,
    options: &[(&str, &str)],
) -> ChatResult<Option<String>> {
    if options.is_empty() {
        return Ok(None);
    }
    let _raw = RawModeGuard::enable()?;
    let mut selected = 0usize;
    let rows = options.len() + 1;
    render_picker(writer, ui, title, options, selected, false)?;
    loop {
        let event = read().map_err(ChatError::Io)?;
        let Event::Key(key) = event else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        match key.code {
            KeyCode::Up => {
                selected = if selected == 0 {
                    options.len() - 1
                } else {
                    selected - 1
                };
                render_picker(writer, ui, title, options, selected, true)?;
            }
            KeyCode::Down | KeyCode::Tab => {
                selected = (selected + 1) % options.len();
                render_picker(writer, ui, title, options, selected, true)?;
            }
            KeyCode::Enter => {
                write!(writer, "\r\n")?;
                writer.flush()?;
                return Ok(Some(options[selected].0.to_string()));
            }
            KeyCode::Esc => {
                clear_picker(writer, rows)?;
                return Ok(None);
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                clear_picker(writer, rows)?;
                return Ok(None);
            }
            _ => {}
        }
    }
}

fn render_picker<W: Write>(
    writer: &mut W,
    ui: ChatUi,
    title: &str,
    options: &[(&str, &str)],
    selected: usize,
    move_up: bool,
) -> ChatResult<()> {
    if move_up {
        queue!(writer, MoveUp((options.len() + 1) as u16)).map_err(ChatError::Io)?;
    }
    queue!(writer, MoveToColumn(0), Clear(ClearType::CurrentLine)).map_err(ChatError::Io)?;
    writeln!(writer, "{title}")?;
    for (idx, (value, description)) in options.iter().enumerate() {
        queue!(writer, MoveToColumn(0), Clear(ClearType::CurrentLine)).map_err(ChatError::Io)?;
        let marker = if idx == selected { ">" } else { " " };
        let line = format!("  {marker} {:<24} {}", value, description);
        if ui.is_rich() && idx == selected {
            writeln!(writer, "\x1b[7m{line}\x1b[0m")?;
        } else {
            writeln!(writer, "{line}")?;
        }
    }
    writer.flush()?;
    Ok(())
}

fn clear_picker<W: Write>(writer: &mut W, rows: usize) -> ChatResult<()> {
    queue!(writer, MoveUp(rows as u16)).map_err(ChatError::Io)?;
    for idx in 0..rows {
        queue!(writer, MoveToColumn(0), Clear(ClearType::CurrentLine)).map_err(ChatError::Io)?;
        if idx + 1 < rows {
            queue!(writer, MoveDown(1)).map_err(ChatError::Io)?;
        }
    }
    queue!(writer, MoveToColumn(0)).map_err(ChatError::Io)?;
    writer.flush()?;
    Ok(())
}

fn render_slash_palette<W: Write>(
    writer: &mut W,
    ui: ChatUi,
    buffer: &str,
    selected: usize,
    rows: u16,
) -> ChatResult<()> {
    let matches = matching_slash_commands(buffer);
    let mut lines = slash_palette_lines(ui, buffer, selected, &matches, rows);
    lines.truncate(rows as usize);

    queue!(writer, SavePosition, MoveUp(rows)).map_err(ChatError::Io)?;
    clear_palette_rows(writer, rows)?;
    queue!(writer, MoveUp(rows.saturating_sub(1))).map_err(ChatError::Io)?;
    for (idx, line) in lines.iter().enumerate() {
        queue!(writer, MoveToColumn(0), Clear(ClearType::CurrentLine)).map_err(ChatError::Io)?;
        write!(writer, "{line}")?;
        if idx + 1 < rows as usize {
            queue!(writer, MoveDown(1)).map_err(ChatError::Io)?;
        }
    }
    queue!(writer, RestorePosition).map_err(ChatError::Io)?;
    writer.flush()?;
    Ok(())
}

fn clear_transient_input_area<W: Write>(writer: &mut W, rows: u16) -> ChatResult<()> {
    queue!(writer, SavePosition, MoveUp(rows)).map_err(ChatError::Io)?;
    clear_palette_rows(writer, rows)?;
    queue!(writer, RestorePosition).map_err(ChatError::Io)?;
    writer.flush()?;
    Ok(())
}

fn clear_palette_rows<W: Write>(writer: &mut W, rows: u16) -> ChatResult<()> {
    for idx in 0..rows {
        queue!(writer, MoveToColumn(0), Clear(ClearType::CurrentLine)).map_err(ChatError::Io)?;
        if idx + 1 < rows {
            queue!(writer, MoveDown(1)).map_err(ChatError::Io)?;
        }
    }
    Ok(())
}

fn slash_palette_lines(
    ui: ChatUi,
    buffer: &str,
    selected: usize,
    matches: &[SlashCommandSpec],
    rows: u16,
) -> Vec<String> {
    let command_rows = rows.saturating_sub(3).max(1) as usize;
    let columns = palette_column_limit();
    let mut lines = vec![
        fit_palette_line("  kimetsu commands", columns),
        fit_palette_line(
            "  type to filter  Up/Down select  Tab complete  Enter run",
            columns,
        ),
    ];

    if matches.is_empty() {
        lines.push(fit_palette_line(
            &format!("  no commands match `{}`", slash_query(buffer)),
            columns,
        ));
    } else {
        let selected = selected.min(matches.len() - 1);
        let start = slash_palette_window_start(matches.len(), selected, command_rows);
        let end = (start + command_rows).min(matches.len());
        for (offset, spec) in matches[start..end].iter().enumerate() {
            let idx = start + offset;
            let marker = if idx == selected { ">" } else { " " };
            let text = fit_palette_line(
                &format!("  {marker} {:<18} {}", spec.display(), spec.summary),
                columns,
            );
            if ui.is_rich() && idx == selected {
                lines.push(format!("\x1b[7m{text}\x1b[0m"));
            } else {
                lines.push(text);
            }
        }

        if matches.len() > command_rows {
            lines.push(fit_palette_line(
                &format!("  showing {}-{} of {}", start + 1, end, matches.len()),
                columns,
            ));
        }
    }

    while lines.len() < rows as usize {
        lines.push(String::new());
    }
    lines
}

fn slash_palette_window_start(total: usize, selected: usize, visible: usize) -> usize {
    if total <= visible {
        0
    } else {
        let half = visible / 2;
        selected.saturating_sub(half).min(total - visible)
    }
}

fn palette_column_limit() -> usize {
    size()
        .map(|(cols, _)| cols.saturating_sub(1).max(20) as usize)
        .unwrap_or(79)
}

fn fit_palette_line(line: &str, columns: usize) -> String {
    let len = line.chars().count();
    if len <= columns {
        return line.to_string();
    }
    if columns <= 3 {
        return line.chars().take(columns).collect();
    }
    let mut out: String = line.chars().take(columns - 3).collect();
    out.push_str("...");
    out
}

fn clamp_slash_selection(buffer: &str, selected: usize) -> usize {
    let count = matching_slash_commands(buffer).len();
    if count == 0 {
        0
    } else {
        selected.min(count - 1)
    }
}

fn selected_slash_completion(buffer: &str, selected: usize) -> Option<String> {
    matching_slash_commands(buffer)
        .get(selected)
        .map(|spec| spec.completion())
}

fn selected_slash_completion_for_enter(buffer: &str, selected: usize) -> Option<String> {
    let trimmed = buffer.trim();
    if trimmed == "/" {
        return selected_slash_completion(buffer, selected);
    }
    if trimmed[1..].chars().any(char::is_whitespace) {
        return None;
    }
    let spec = *matching_slash_commands(buffer).get(selected)?;
    let command_name = spec.command.trim_start_matches('/');
    let typed = trimmed.trim_start_matches('/');
    if command_name.starts_with(typed) && typed != command_name {
        Some(spec.completion())
    } else {
        None
    }
}

fn matching_slash_commands(buffer: &str) -> Vec<SlashCommandSpec> {
    let query = slash_query(buffer).to_ascii_lowercase();
    if query.is_empty() {
        return SLASH_COMMANDS.to_vec();
    }

    SLASH_COMMANDS
        .iter()
        .copied()
        .filter(|spec| {
            let command = spec.command.trim_start_matches('/').to_ascii_lowercase();
            command.starts_with(&query) || spec.summary.to_ascii_lowercase().contains(&query)
        })
        .collect()
}

fn slash_query(buffer: &str) -> &str {
    let Some(rest) = buffer.trim_start().strip_prefix('/') else {
        return "";
    };
    rest.split_whitespace().next().unwrap_or(rest)
}

struct RawModeGuard;

impl RawModeGuard {
    fn enable() -> ChatResult<Self> {
        enable_raw_mode().map_err(ChatError::Io)?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

fn handle_skills_command<W: Write>(
    arg: &str,
    registry: &mut SkillRegistry,
    config: &SkillConfig,
    loaded: &mut Vec<LoadedSkill>,
    ui: ChatUi,
    terminal_available: bool,
    writer: &mut W,
) -> ChatResult<()> {
    let mut parts = arg.split_whitespace();
    let command = parts
        .next()
        .unwrap_or(if terminal_available { "select" } else { "list" });
    match command {
        "" | "list" | "ls" => {
            write_skill_results(writer, registry, loaded, "", 40)?;
        }
        "select" | "pick" | "browse" => {
            if !terminal_available {
                writeln!(
                    writer,
                    "/skills select requires an interactive terminal; use `/skills search <query>` or `/skills use <name>`."
                )?;
                return Ok(());
            }
            run_skill_picker(writer, ui, registry, config, loaded)?;
        }
        "search" | "find" => {
            let query = parts.collect::<Vec<_>>().join(" ");
            write_skill_results(writer, registry, loaded, &query, 80)?;
        }
        "sources" | "marketplaces" | "roots" => {
            if registry.roots().is_empty() {
                writeln!(writer, "skill sources: none configured")?;
                return Ok(());
            }
            writeln!(writer, "{} skill source(s):", registry.roots().len())?;
            for root in registry.roots() {
                let status = if root.exists { "found" } else { "missing" };
                let login = match root.kind.as_str() {
                    "workspace" | "extra" => "local",
                    _ if root.logged_in => "login detected",
                    _ => "login unknown",
                };
                let marketplace = root
                    .marketplace
                    .as_ref()
                    .map(|marketplace| format!(" marketplace={marketplace}"))
                    .unwrap_or_default();
                writeln!(
                    writer,
                    "  {} {} [{}; {}; {}{}]",
                    root.source.as_str(),
                    root.path.display(),
                    root.kind.as_str(),
                    status,
                    login,
                    marketplace
                )?;
            }
        }
        "loaded" => {
            if loaded.is_empty() {
                writeln!(writer, "no skills loaded for this session")?;
            } else {
                writeln!(
                    writer,
                    "loaded skills: {}",
                    loaded_skill_names(loaded).join(", ")
                )?;
            }
        }
        "use" | "load" => {
            let selection = parts.collect::<Vec<_>>().join(" ");
            if selection.trim().is_empty() {
                writeln!(writer, "usage: /skills use <name-or-path>")?;
                return Ok(());
            }
            match registry.load_into(&selection, loaded, config) {
                Ok(true) => ui.write_note(
                    writer,
                    "[skills]",
                    &format!(
                        "loaded `{}`",
                        loaded
                            .last()
                            .map(|s| s.manifest.name.as_str())
                            .unwrap_or(selection.as_str())
                    ),
                )?,
                Ok(false) => {
                    ui.write_note(writer, "[skills]", &format!("`{selection}` already loaded"))?
                }
                Err(err) => ui.write_error(writer, &err)?,
            }
        }
        "install" | "import" => {
            let mut force = false;
            let selection = parts
                .filter(|part| {
                    if *part == "--force" || *part == "-f" {
                        force = true;
                        false
                    } else {
                        true
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");
            if selection.trim().is_empty() {
                writeln!(writer, "usage: /skills install [--force] <name-or-path>")?;
                return Ok(());
            }
            match registry.install_as_kimetsu(&selection, force) {
                Ok(skill) => {
                    registry.refresh(config).map_err(ChatError::Skill)?;
                    ui.write_note(
                        writer,
                        "[skills]",
                        &format!(
                            "installed `{}` as Kimetsu skill at {}",
                            skill.name,
                            skill.root.display()
                        ),
                    )?;
                }
                Err(err) => ui.write_error(writer, &err)?,
            }
        }
        "clear" | "reset" => {
            loaded.clear();
            ui.write_note(writer, "[skills]", "cleared loaded skills")?;
        }
        other => {
            writeln!(
                writer,
                "unknown /skills command `{other}`. Use: /skills select | /skills search <query> | /skills list | /skills sources | /skills use <name-or-path> | /skills install <name-or-path> | /skills loaded | /skills clear"
            )?;
        }
    }
    Ok(())
}

fn write_skill_results<W: Write>(
    writer: &mut W,
    registry: &SkillRegistry,
    loaded: &[LoadedSkill],
    query: &str,
    limit: usize,
) -> ChatResult<()> {
    let matches = registry.matching_skills(query);
    if registry.skills().is_empty() {
        writeln!(
            writer,
            "no skills found. Add skill folders under .codex/skills, .claude/skills, .kimetsu/skills, or pass --skill-dir."
        )?;
        return Ok(());
    }
    if matches.is_empty() {
        writeln!(writer, "no skills matched `{}`", query.trim())?;
        return Ok(());
    }
    if query.trim().is_empty() {
        writeln!(writer, "{} available skill(s):", matches.len())?;
    } else {
        writeln!(
            writer,
            "{} matching skill(s) for `{}`:",
            matches.len(),
            query.trim()
        )?;
    }
    for skill in matches.iter().take(limit) {
        writeln!(
            writer,
            "  {:<20} {} [{}; {}] - {}",
            skill_state_label(registry, loaded, skill),
            skill.name,
            skill_origin_label(skill),
            skill.resource_summary(),
            preview(&skill.description, 100)
        )?;
    }
    if matches.len() > limit {
        writeln!(
            writer,
            "  ... ({} more; narrow with /skills search <query>)",
            matches.len() - limit
        )?;
    }
    Ok(())
}

fn run_skill_picker<W: Write>(
    writer: &mut W,
    ui: ChatUi,
    registry: &mut SkillRegistry,
    config: &SkillConfig,
    loaded: &mut Vec<LoadedSkill>,
) -> ChatResult<()> {
    if registry.skills().is_empty() {
        writeln!(
            writer,
            "no skills found. Add skill folders under .codex/skills, .claude/skills, .kimetsu/skills, or pass --skill-dir."
        )?;
        return Ok(());
    }

    let _raw = RawModeGuard::enable()?;
    let mut query = String::new();
    let mut selected = 0usize;
    let mut rows = 0usize;
    let mut status =
        "type to search; Space/Enter toggles loaded; i imports; Esc closes".to_string();

    loop {
        let (matches_len, current) = {
            let matches = registry.matching_skills(&query);
            if selected >= matches.len() {
                selected = matches.len().saturating_sub(1);
            }
            rows = render_skill_picker(
                writer,
                SkillPickerRender {
                    ui,
                    previous_rows: rows,
                    registry,
                    loaded,
                    query: &query,
                    matches: &matches,
                    selected,
                    status: &status,
                },
            )?;
            (
                matches.len(),
                matches.get(selected).map(|skill| (*skill).clone()),
            )
        };

        let event = read().map_err(ChatError::Io)?;
        let Event::Key(key) = event else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }

        match key.code {
            KeyCode::Up if matches_len > 0 => {
                selected = if selected == 0 {
                    matches_len - 1
                } else {
                    selected - 1
                };
            }
            KeyCode::Down | KeyCode::Tab if matches_len > 0 => {
                selected = (selected + 1) % matches_len;
            }
            KeyCode::PageUp => {
                selected = selected.saturating_sub(SKILL_PICKER_VISIBLE_ROWS);
            }
            KeyCode::PageDown if matches_len > 0 => {
                selected = (selected + SKILL_PICKER_VISIBLE_ROWS).min(matches_len - 1);
            }
            KeyCode::Backspace => {
                query.pop();
                selected = 0;
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                query.clear();
                selected = 0;
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                clear_picker(writer, rows)?;
                return Ok(());
            }
            KeyCode::Esc => {
                clear_picker(writer, rows)?;
                return Ok(());
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                if let Some(skill) = current {
                    status = match toggle_skill_loaded(registry, config, loaded, &skill) {
                        Ok(message) => message,
                        Err(err) => err.to_string(),
                    };
                } else {
                    status = "no matching skill selected".to_string();
                }
            }
            KeyCode::Char('i') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(skill) = current {
                    if registry.is_installed(&skill) {
                        status = format!("`{}` already has a Kimetsu install", skill.name);
                    } else {
                        let selection = skill.root.display().to_string();
                        match registry.install_as_kimetsu(&selection, false) {
                            Ok(installed) => {
                                status = format!("installed `{}` into Kimetsu", installed.name);
                                registry.refresh(config).map_err(ChatError::Skill)?;
                            }
                            Err(err) => status = err,
                        }
                    }
                } else {
                    status = "no matching skill selected".to_string();
                }
            }
            KeyCode::Char('r') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                registry.refresh(config).map_err(ChatError::Skill)?;
                selected = 0;
                status = "refreshed skill sources".to_string();
            }
            KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                query.push(ch);
                selected = 0;
            }
            _ => {}
        }
    }
}

const SKILL_PICKER_VISIBLE_ROWS: usize = 10;

struct SkillPickerRender<'a> {
    ui: ChatUi,
    previous_rows: usize,
    registry: &'a SkillRegistry,
    loaded: &'a [LoadedSkill],
    query: &'a str,
    matches: &'a [&'a SkillManifest],
    selected: usize,
    status: &'a str,
}

fn render_skill_picker<W: Write>(writer: &mut W, view: SkillPickerRender<'_>) -> ChatResult<usize> {
    if view.previous_rows > 0 {
        clear_picker(writer, view.previous_rows)?;
    }
    let width = size().map(|(cols, _)| cols as usize).unwrap_or(100).max(60);
    let visible =
        visible_skill_window(view.matches.len(), view.selected, SKILL_PICKER_VISIBLE_ROWS);
    let mut rows = 0usize;

    writeln!(writer, "skills")?;
    rows += 1;
    writeln!(
        writer,
        "search: {}",
        if view.query.is_empty() {
            "<type>"
        } else {
            view.query
        }
    )?;
    rows += 1;
    writeln!(
        writer,
        "keys: type search | Up/Down navigate | Space/Enter load/unload | i install | r refresh | Esc close"
    )?;
    rows += 1;
    writeln!(
        writer,
        "showing {} of {} match(es) | {}",
        visible.end.saturating_sub(visible.start),
        view.matches.len(),
        view.status
    )?;
    rows += 1;

    if view.matches.is_empty() {
        writeln!(writer, "  no matches")?;
        rows += 1;
    } else {
        for (offset, skill) in view.matches[visible.clone()].iter().enumerate() {
            let idx = visible.start + offset;
            let marker = if idx == view.selected { ">" } else { " " };
            let line = format!(
                "  {marker} {:<20} {:<24} {:<28} {}",
                skill_state_label(view.registry, view.loaded, skill),
                preview(&skill.name, 22),
                preview(&skill_origin_label(skill), 26),
                preview(&skill.description, width.saturating_sub(82).max(24))
            );
            if view.ui.is_rich() && idx == view.selected {
                writeln!(writer, "\x1b[7m{}\x1b[0m", preview(&line, width))?;
            } else {
                writeln!(writer, "{}", preview(&line, width))?;
            }
            rows += 1;
        }
        if let Some(skill) = view.matches.get(view.selected) {
            writeln!(
                writer,
                "selected: {} | resources: {}",
                preview(&skill.root.display().to_string(), width.saturating_sub(24)),
                skill.resource_summary()
            )?;
            rows += 1;
        }
    }
    writer.flush()?;
    Ok(rows)
}

fn visible_skill_window(len: usize, selected: usize, window: usize) -> std::ops::Range<usize> {
    if len <= window {
        return 0..len;
    }
    let half = window / 2;
    let mut start = selected.saturating_sub(half);
    if start + window > len {
        start = len - window;
    }
    start..(start + window)
}

fn toggle_skill_loaded(
    registry: &SkillRegistry,
    config: &SkillConfig,
    loaded: &mut Vec<LoadedSkill>,
    skill: &SkillManifest,
) -> ChatResult<String> {
    if let Some(index) = loaded_skill_index(loaded, skill) {
        let removed = loaded.remove(index);
        return Ok(format!("unloaded `{}`", removed.manifest.name));
    }
    let selection = skill.root.display().to_string();
    match registry.load_into(&selection, loaded, config) {
        Ok(true) => Ok(format!("loaded `{}`", skill.name)),
        Ok(false) => Ok(format!("`{}` already loaded", skill.name)),
        Err(err) => Err(ChatError::Skill(err)),
    }
}

fn skill_state_label(
    registry: &SkillRegistry,
    loaded: &[LoadedSkill],
    skill: &SkillManifest,
) -> String {
    match (
        loaded_skill_index(loaded, skill).is_some(),
        registry.is_installed(skill),
    ) {
        (true, true) => "[loaded installed]".to_string(),
        (true, false) => "[loaded]".to_string(),
        (false, true) => "[installed]".to_string(),
        (false, false) => "[available]".to_string(),
    }
}

fn loaded_skill_index(loaded: &[LoadedSkill], skill: &SkillManifest) -> Option<usize> {
    loaded
        .iter()
        .position(|loaded| same_display_path(&loaded.manifest.path, &skill.path))
}

fn same_display_path(left: &Path, right: &Path) -> bool {
    left.canonicalize().unwrap_or_else(|_| left.to_path_buf())
        == right.canonicalize().unwrap_or_else(|_| right.to_path_buf())
}

struct StatusView<'a> {
    workspace: &'a Path,
    session: &'a ChatSession,
    config: &'a ChatConfig,
    permission_mode: PermissionMode,
    reasoning_effort: ReasoningEffort,
    cost: &'a CostMeter,
    transcript: &'a [TurnRecord],
    loaded_skills: &'a [LoadedSkill],
}

fn handle_status_command<W: Write>(writer: &mut W, view: StatusView<'_>) -> ChatResult<()> {
    writeln!(writer, "session: {}", view.session.id)?;
    if let Some(title) = &view.session.title {
        writeln!(writer, "title: {title}")?;
    }
    writeln!(writer, "workspace: {}", view.workspace.display())?;
    writeln!(writer, "model: {}", view.config.model)?;
    writeln!(
        writer,
        "image generation: {}",
        model_supports_image_generation(&view.config.model)
    )?;
    writeln!(
        writer,
        "permissions: {} - {}",
        view.permission_mode.as_str(),
        view.permission_mode.description()
    )?;
    writeln!(writer, "effort: {}", view.reasoning_effort.as_str())?;
    writeln!(writer, "turns: {}", view.transcript.len())?;
    writeln!(writer, "checkpoints: {}", view.session.checkpoints.len())?;
    writeln!(
        writer,
        "cost: ${:.4}/${:.2} remaining ${:.4}",
        view.cost.spent(),
        view.cost.budget(),
        view.cost.remaining()
    )?;
    writeln!(
        writer,
        "skills loaded: {}",
        loaded_skill_names(view.loaded_skills).join(", ")
    )?;
    match run_git(view.workspace, &["status", "--porcelain=v2", "--branch"]) {
        Ok(status) => {
            let changed = status
                .lines()
                .filter(|line| !line.starts_with("# "))
                .count();
            writeln!(writer, "git changed entries: {changed}")?;
        }
        Err(err) => writeln!(writer, "git: {err}")?,
    }
    Ok(())
}

fn handle_context_command<W: Write>(
    writer: &mut W,
    brain_session: Option<&brain_project::BrainSession>,
    transcript: &[TurnRecord],
    loaded_skills: &[LoadedSkill],
    checkpoints: &[CheckpointRecord],
) -> ChatResult<()> {
    let transcript_chars = transcript
        .iter()
        .map(|turn| turn.user.len() + turn.assistant.len())
        .sum::<usize>();
    writeln!(writer, "conversation turns: {}", transcript.len())?;
    writeln!(
        writer,
        "approx transcript tokens: {}",
        approx_tokens(transcript_chars)
    )?;
    writeln!(writer, "loaded skills: {}", loaded_skills.len())?;
    writeln!(writer, "checkpoints: {}", checkpoints.len())?;
    writeln!(
        writer,
        "brain retrieval: {}",
        if brain_session.is_some() {
            "enabled"
        } else {
            "disabled"
        }
    )?;
    if let Some(last) = transcript.last() {
        writeln!(writer, "last user: {}", preview(&last.user, 120))?;
        writeln!(writer, "last assistant: {}", preview(&last.assistant, 120))?;
    }
    Ok(())
}

struct StatuslineView<'a> {
    ui: ChatUi,
    model: &'a str,
    permission_mode: PermissionMode,
    effort: ReasoningEffort,
    spent: f32,
    budget: f32,
    turns: usize,
    plan_mode: bool,
}

fn write_statusline<W: Write>(writer: &mut W, view: StatuslineView<'_>) -> ChatResult<()> {
    let mode = if view.plan_mode { "plan" } else { "chat" };
    let line = format!(
        "kimetsu  model={}  effort={}  perms={}  cost=${:.4}/${:.2}  turns={}  mode={}",
        view.model,
        view.effort.as_str(),
        view.permission_mode.as_str(),
        view.spent,
        view.budget,
        view.turns,
        mode
    );
    if view.ui.is_rich() {
        writeln!(writer, "\x1b[2m{line}\x1b[0m")?;
    } else {
        writeln!(writer, "{line}")?;
    }
    Ok(())
}

fn parse_toggle(input: &str) -> Option<bool> {
    match input.trim().to_ascii_lowercase().as_str() {
        "on" | "true" | "1" | "yes" | "enable" | "enabled" => Some(true),
        "off" | "false" | "0" | "no" | "disable" | "disabled" => Some(false),
        _ => None,
    }
}

fn handle_transcript_command<W: Write>(
    writer: &mut W,
    transcript: &[TurnRecord],
    arg: &str,
) -> ChatResult<()> {
    if transcript.is_empty() {
        writeln!(writer, "transcript: empty")?;
        return Ok(());
    }
    let trimmed = arg.trim();
    let count = if trimmed.eq_ignore_ascii_case("all") {
        transcript.len()
    } else if let Some(rest) = trimmed.strip_prefix("last ") {
        rest.trim().parse::<usize>().unwrap_or(8)
    } else if trimmed.is_empty() {
        8
    } else {
        trimmed.parse::<usize>().unwrap_or(8)
    };
    let start = transcript.len().saturating_sub(count);
    writeln!(
        writer,
        "transcript: showing {} of {} turn(s)",
        transcript.len() - start,
        transcript.len()
    )?;
    for (idx, turn) in transcript.iter().enumerate().skip(start) {
        writeln!(writer, "\nturn {}", idx + 1)?;
        writeln!(writer, "user:\n{}", preview(&turn.user, 2_000))?;
        writeln!(writer, "assistant:\n{}", preview(&turn.assistant, 4_000))?;
    }
    Ok(())
}

fn handle_copy_command<W: Write>(
    writer: &mut W,
    transcript: &[TurnRecord],
    arg: &str,
) -> ChatResult<()> {
    if transcript.is_empty() {
        writeln!(writer, "copy: no assistant responses yet")?;
        return Ok(());
    }
    let nth_latest = arg.trim().parse::<usize>().unwrap_or(1).max(1);
    let Some(turn) = transcript.iter().rev().nth(nth_latest - 1) else {
        writeln!(writer, "copy: no response at position {nth_latest}")?;
        return Ok(());
    };
    copy_to_clipboard(&turn.assistant)?;
    writeln!(
        writer,
        "copied assistant response {} to clipboard",
        if nth_latest == 1 {
            "latest".to_string()
        } else {
            format!("#{nth_latest} from latest")
        }
    )?;
    Ok(())
}

fn copy_to_clipboard(text: &str) -> ChatResult<()> {
    #[cfg(windows)]
    let candidates: &[(&str, &[&str])] = &[("clip", &[])];
    #[cfg(target_os = "macos")]
    let candidates: &[(&str, &[&str])] = &[("pbcopy", &[])];
    #[cfg(all(unix, not(target_os = "macos")))]
    let candidates: &[(&str, &[&str])] =
        &[("wl-copy", &[]), ("xclip", &["-selection", "clipboard"])];

    for (program, args) in candidates {
        let mut child = match Command::new(program)
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(_) => continue,
        };
        if let Some(stdin) = child.stdin.as_mut() {
            stdin.write_all(text.as_bytes())?;
        }
        let status = child.wait()?;
        if status.success() {
            return Ok(());
        }
    }
    Err(ChatError::Provider(
        "no clipboard command available (tried clip/pbcopy/wl-copy/xclip)".into(),
    ))
}

fn handle_compact_command<W: Write>(
    writer: &mut W,
    workspace: &Path,
    config: &ChatConfig,
    instructions: &str,
    provider: &mut Option<Box<dyn kimetsu_agent::model::ModelProvider>>,
    cost: &mut CostMeter,
    transcript: &mut Vec<TurnRecord>,
) -> ChatResult<bool> {
    if transcript.is_empty() {
        writeln!(writer, "compact: transcript is empty")?;
        return Ok(false);
    }
    if provider.is_none() {
        *provider = Some(build_chat_provider(config, workspace)?);
    }
    let mut body = String::new();
    for (idx, turn) in transcript.iter().enumerate() {
        body.push_str(&format!(
            "Turn {} user:\n{}\nTurn {} assistant:\n{}\n\n",
            idx + 1,
            preview(&turn.user, 1_500),
            idx + 1,
            preview(&turn.assistant, 2_500)
        ));
    }
    let prompt = format!(
        "Compact this Kimetsu session into a durable working summary. Preserve goals, decisions, file paths, commands run, known failures, pending tasks, loaded skills, and user preferences. {}\n\nTranscript:\n{}",
        if instructions.trim().is_empty() {
            String::new()
        } else {
            format!("Extra focus: {}", instructions.trim())
        },
        body
    );
    let report = run_text_only_chat_with_system(
        provider.as_mut().expect("provider set").as_mut(),
        "You compact coding-agent conversations. Return a concise but complete summary that can replace the prior transcript.",
        &prompt,
        &[],
        "chat_compact",
    )?;
    cost.record_turn(report.usage.cost_usd);
    let summary = format!("Session compacted summary:\n{}", report.text.trim());
    transcript.clear();
    transcript.push(TurnRecord {
        user: format!("/compact {}", instructions.trim()),
        assistant: summary.clone(),
    });
    writeln!(
        writer,
        "compact: reduced session to one summary turn (cost ${:.4})",
        report.usage.cost_usd
    )?;
    writeln!(writer, "\n{}", preview(&summary, 6_000))?;
    Ok(true)
}

fn handle_shell_prefix_command<W: Write>(
    writer: &mut W,
    workspace: &Path,
    permission_mode: PermissionMode,
    command_line: &str,
    transcript: &mut Vec<TurnRecord>,
) -> ChatResult<Option<String>> {
    let command_line = command_line.trim();
    if command_line.is_empty() {
        writeln!(writer, "usage: ! <command>")?;
        return Ok(None);
    }
    if permission_mode == PermissionMode::ReadOnly {
        writeln!(writer, "! shell mode blocked by read-only permissions")?;
        return Ok(None);
    }
    let spec = parse_command_line(command_line)?;
    let mut runtime = ToolRuntime::new(workspace, RunId::new())
        .map_err(|err| ChatError::Provider(format!("tool runtime: {err}")))?
        .with_config(ToolRuntimeConfig {
            trace_fsync: false,
            ..ToolRuntimeConfig::default()
        });
    writeln!(writer, "[shell] {command_line}")?;
    let out = runtime
        .shell_command(spec)
        .map_err(|err| ChatError::Provider(format!("shell: {err}")))?;
    let reply = format!(
        "Shell command completed.\ncommand: {}\nexit={} duration={}ms timed_out={}\nstdout:\n{}\nstderr:\n{}",
        command_line,
        out.exit_code,
        out.duration_ms,
        out.timed_out,
        preview(&out.stdout_summary, 8_000),
        preview(&out.stderr_summary, 8_000)
    );
    transcript.push(TurnRecord {
        user: format!("! {command_line}"),
        assistant: reply.clone(),
    });
    Ok(Some(reply))
}

fn parse_skill_prefix(line: &str) -> Option<(String, String)> {
    let rest = line.trim_start().strip_prefix('$')?;
    let mut parts = rest.splitn(2, char::is_whitespace);
    let name = parts.next()?.trim();
    if name.is_empty() {
        return None;
    }
    let prompt = parts.next().unwrap_or("").trim().to_string();
    Some((name.to_string(), prompt))
}

fn expand_file_mentions(line: &str, workspace: &Path) -> Option<String> {
    let mentions = line
        .split_whitespace()
        .filter_map(|token| token.strip_prefix('@'))
        .filter(|token| !token.is_empty())
        .take(8)
        .collect::<Vec<_>>();
    if mentions.is_empty() {
        return None;
    }
    let mut out = String::new();
    out.push_str("User message:\n");
    out.push_str(line);
    out.push_str("\n\nMentioned files:\n");
    let mut included = 0usize;
    for mention in mentions {
        let rel = mention.trim_matches(|ch| matches!(ch, ',' | ';' | ':' | ')'));
        let path = workspace.join(rel);
        if !path.is_file() {
            out.push_str(&format!("\n--- {rel} ---\n<not found or not a file>\n"));
            continue;
        }
        match fs::read_to_string(&path) {
            Ok(text) => {
                included += 1;
                out.push_str(&format!("\n--- {rel} ---\n{}\n", preview(&text, 12_000)));
            }
            Err(err) => {
                out.push_str(&format!("\n--- {rel} ---\n<read failed: {err}>\n"));
            }
        }
    }
    if included == 0 { None } else { Some(out) }
}

struct ReviewCommandContext<'a> {
    workspace: &'a Path,
    config: &'a ChatConfig,
    provider: &'a mut Option<Box<dyn kimetsu_agent::model::ModelProvider>>,
    cost: &'a mut CostMeter,
    transcript: &'a mut Vec<TurnRecord>,
}

fn handle_review_command<W: Write>(
    writer: &mut W,
    ctx: &mut ReviewCommandContext<'_>,
    focus: &str,
    kind: &str,
) -> ChatResult<()> {
    if ctx.provider.is_none() {
        *ctx.provider = Some(build_chat_provider(ctx.config, ctx.workspace)?);
    }
    let diff = current_git_diff(ctx.workspace).unwrap_or_default();
    if diff.trim().is_empty() {
        writeln!(writer, "{kind}: current git diff is empty")?;
        return Ok(());
    }
    let instruction = match kind {
        "security-review" => {
            "Review the diff for security vulnerabilities, secret leaks, auth bugs, injection risks, unsafe command execution, and data exposure. Lead with findings ordered by severity."
        }
        "simplify" => {
            "Review the diff for unnecessary complexity, duplication, inefficient code, and maintainability issues. Lead with actionable findings."
        }
        _ => {
            "Review the diff for correctness bugs, regressions, missing tests, and risky behavior. Lead with findings ordered by severity."
        }
    };
    let task = format!(
        "{instruction}\nFocus: {}\n\nDiff:\n{}",
        if focus.trim().is_empty() {
            "<none>"
        } else {
            focus.trim()
        },
        preview(&diff, 32_000)
    );
    let report = run_text_only_chat_with_system(
        ctx.provider.as_mut().expect("provider set").as_mut(),
        "You are a strict code reviewer. Lead with concrete findings and file/path evidence. If there are no findings, say so and name residual risks.",
        &task,
        ctx.transcript,
        kind,
    )?;
    ctx.cost.record_turn(report.usage.cost_usd);
    writeln!(writer, "{kind}:")?;
    writeln!(writer, "{}", report.text.trim_end())?;
    ctx.transcript.push(TurnRecord {
        user: format!("/{kind} {}", focus.trim()),
        assistant: report.text,
    });
    Ok(())
}

fn handle_debug_config_command<W: Write>(
    writer: &mut W,
    workspace: &Path,
    config: &ChatConfig,
    session_dir: &Path,
) -> ChatResult<()> {
    writeln!(writer, "kimetsu debug config")?;
    writeln!(writer, "workspace: {}", workspace.display())?;
    writeln!(writer, "session_dir: {}", session_dir.display())?;
    writeln!(writer, "model: {}", config.model)?;
    writeln!(writer, "persist_sessions: {}", config.persist_sessions)?;
    writeln!(writer, "raw_terminal_input: {}", config.raw_terminal_input)?;
    writeln!(writer, "skills enabled: {}", config.skills.enabled)?;
    writeln!(
        writer,
        "workspace skill roots: {}",
        config.skills.include_workspace_roots
    )?;
    writeln!(
        writer,
        "user skill roots: {}",
        config.skills.include_user_roots
    )?;
    writeln!(writer, "extra skill roots: {}", config.skills.roots.len())?;
    writeln!(
        writer,
        "env CLAUDE_CODE_OAUTH_TOKEN: {}",
        resolve_env_value(workspace, "CLAUDE_CODE_OAUTH_TOKEN").is_some()
    )?;
    writeln!(
        writer,
        "env OPENAI_API_KEY: {}",
        resolve_env_value(workspace, "OPENAI_API_KEY").is_some()
    )?;
    writeln!(
        writer,
        ".kimetsu exists: {}",
        workspace.join(".kimetsu").exists()
    )?;
    writeln!(
        writer,
        ".claude exists: {}",
        workspace.join(".claude").exists()
    )?;
    writeln!(
        writer,
        ".codex exists: {}",
        workspace.join(".codex").exists()
    )?;
    Ok(())
}

fn handle_keybindings_command<W: Write>(writer: &mut W) -> ChatResult<()> {
    writeln!(writer, "keybindings:")?;
    writeln!(writer, "  / at prompt          open slash command palette")?;
    writeln!(
        writer,
        "  Tab                  complete slash, @path, or $skill"
    )?;
    writeln!(
        writer,
        "  Up/Down              command history or palette selection"
    )?;
    writeln!(
        writer,
        "  Ctrl+R               reverse-search prompt history"
    )?;
    writeln!(writer, "  Ctrl+L               redraw terminal")?;
    writeln!(
        writer,
        "  Ctrl+C               clear input; on empty input, quit"
    )?;
    writeln!(
        writer,
        "  Ctrl+D               quit on empty input; delete under cursor otherwise"
    )?;
    writeln!(
        writer,
        "  Ctrl+G               edit prompt in $VISUAL or $EDITOR"
    )?;
    writeln!(writer, "  Home/End             move to start/end of input")?;
    writeln!(writer, "  Left/Right           move cursor")?;
    writeln!(
        writer,
        "  Shift/Alt/Ctrl+Enter insert newline when supported by terminal"
    )?;
    writeln!(writer, "  ! <command>          shell mode")?;
    writeln!(writer, "  @path                mention file content")?;
    writeln!(writer, "  $skill <prompt>      load and invoke a skill")?;
    Ok(())
}

fn handle_diff_command<W: Write>(writer: &mut W, workspace: &Path, arg: &str) -> ChatResult<()> {
    let mut staged = false;
    let mut paths = Vec::new();
    for token in arg.split_whitespace() {
        if token == "--staged" || token == "--cached" {
            staged = true;
        } else {
            paths.push(token.to_string());
        }
    }
    match run_git(workspace, &["status", "--short", "--branch"]) {
        Ok(status) if !status.trim().is_empty() => {
            writeln!(writer, "git status:")?;
            write!(writer, "{status}")?;
        }
        Ok(_) => writeln!(writer, "git status: clean")?,
        Err(err) => writeln!(writer, "git status failed: {err}")?,
    }

    let diff = git_diff_for_paths(workspace, staged, &paths)?;
    if diff.trim().is_empty() {
        writeln!(writer, "diff: <empty>")?;
    } else {
        writeln!(writer, "diff:")?;
        write!(writer, "{}", preview(&diff, 24_000))?;
        if diff.chars().count() > 24_000 {
            writeln!(writer, "\n[diff] truncated; narrow with /diff <path>")?;
        }
    }
    Ok(())
}

fn handle_resume_command<W: Write>(
    writer: &mut W,
    session_dir: &Path,
    arg: &str,
) -> ChatResult<Option<ChatSession>> {
    let sessions = list_saved_sessions(session_dir)?;
    let requested = arg.trim();
    if requested.is_empty() {
        if sessions.is_empty() {
            writeln!(writer, "no saved chat sessions")?;
        } else {
            writeln!(writer, "{} saved session(s):", sessions.len())?;
            for session in sessions.iter().take(20) {
                writeln!(
                    writer,
                    "  {}  turns={}  updated={}  {}",
                    session.id,
                    session.transcript.len(),
                    session.updated_at,
                    session.title.as_deref().unwrap_or("")
                )?;
            }
            if sessions.len() > 20 {
                writeln!(writer, "  ... ({} more)", sessions.len() - 20)?;
            }
        }
        return Ok(None);
    }

    let selected = if requested.eq_ignore_ascii_case("last") {
        sessions
            .into_iter()
            .max_by_key(|session| session.updated_at)
    } else {
        sessions
            .into_iter()
            .find(|session| session.id == requested || session.id.starts_with(requested))
    };
    Ok(selected)
}

fn handle_run_command<W: Write>(
    writer: &mut W,
    workspace: &Path,
    permission_mode: PermissionMode,
    terminal_available: bool,
    arg: &str,
    verify: bool,
) -> ChatResult<()> {
    if permission_mode == PermissionMode::ReadOnly {
        writeln!(
            writer,
            "{} blocked by read-only permissions. Use `/permissions auto` first.",
            if verify { "/verify" } else { "/run" }
        )?;
        return Ok(());
    }
    let request = RunCommandRequest::parse(arg);
    let mode = request.mode;
    let command_line = if request.command_line.trim().is_empty() {
        infer_project_command(workspace, verify).ok_or_else(|| {
            ChatError::Provider(
                "no command provided and no project recipe inferred; use /run <command>".into(),
            )
        })?
    } else {
        request.command_line
    };
    if mode == RunMode::Terminal {
        return run_terminal_command(
            writer,
            workspace,
            terminal_available,
            if verify { "verify" } else { "run" },
            &command_line,
        );
    }
    let spec = parse_command_line(&command_line)?;
    writeln!(
        writer,
        "[{}] {}",
        if verify { "verify" } else { "run" },
        command_line
    )?;
    let mut runtime = ToolRuntime::new(workspace, RunId::new())
        .map_err(|err| ChatError::Provider(format!("tool runtime: {err}")))?
        .with_config(ToolRuntimeConfig {
            trace_fsync: false,
            ..ToolRuntimeConfig::default()
        });
    match runtime.shell_command(spec) {
        Ok(out) => {
            writeln!(
                writer,
                "exit={} duration={}ms timed_out={}",
                out.exit_code, out.duration_ms, out.timed_out
            )?;
            if !out.stdout_summary.trim().is_empty() {
                writeln!(writer, "stdout:\n{}", preview(&out.stdout_summary, 8_000))?;
            }
            if !out.stderr_summary.trim().is_empty() {
                writeln!(writer, "stderr:\n{}", preview(&out.stderr_summary, 8_000))?;
            }
        }
        Err(err) => writeln!(writer, "command failed: {err}")?,
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunMode {
    Captured,
    Terminal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RunCommandRequest {
    mode: RunMode,
    command_line: String,
}

impl RunCommandRequest {
    fn parse(arg: &str) -> Self {
        let mut mode = RunMode::Captured;
        let mut rest = arg.trim();
        loop {
            if rest == "--" {
                rest = "";
                break;
            }
            if let Some(after) = rest.strip_prefix("-- ") {
                rest = after.trim_start();
                break;
            }
            if let Some(after) =
                strip_leading_run_flag(rest, &["--terminal", "--tty", "--interactive", "-t", "-i"])
            {
                mode = RunMode::Terminal;
                rest = after;
                continue;
            }
            if let Some(after) = strip_leading_run_flag(rest, &["--capture", "--no-terminal"]) {
                mode = RunMode::Captured;
                rest = after;
                continue;
            }
            break;
        }

        Self {
            mode,
            command_line: rest.trim().to_string(),
        }
    }
}

fn strip_leading_run_flag<'a>(input: &'a str, flags: &[&str]) -> Option<&'a str> {
    let input = input.trim_start();
    for flag in flags {
        if input == *flag {
            return Some("");
        }
        if let Some(after) = input.strip_prefix(flag)
            && after.chars().next().is_some_and(char::is_whitespace)
        {
            return Some(after.trim_start());
        }
    }
    None
}

fn run_terminal_command<W: Write>(
    writer: &mut W,
    workspace: &Path,
    terminal_available: bool,
    label: &str,
    command_line: &str,
) -> ChatResult<()> {
    if !terminal_available {
        let fallback = if label == "tasks" {
            format!("/tasks run {command_line}")
        } else {
            format!("/{label} {command_line}")
        };
        writeln!(
            writer,
            "[{label}:terminal] requires an interactive terminal; use `{fallback}` without --terminal for captured output."
        )?;
        return Ok(());
    }
    let spec = parse_command_line(command_line)?;
    writeln!(writer, "[{label}:terminal] {command_line}")?;
    writeln!(
        writer,
        "terminal attached to command; exit it to return to Kimetsu."
    )?;
    writer.flush()?;

    let _ = disable_raw_mode();
    let started = Instant::now();
    let status = match Command::new(&spec.program)
        .args(&spec.args)
        .current_dir(workspace)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
    {
        Ok(status) => status,
        Err(err) => {
            let _ = disable_raw_mode();
            writeln!(writer, "\n[{label}:terminal] failed to start: {err}")?;
            return Ok(());
        }
    };
    let _ = disable_raw_mode();

    writeln!(
        writer,
        "\n[{label}:terminal] exit={} duration={}ms",
        status.code().unwrap_or(-1),
        started.elapsed().as_millis()
    )?;
    Ok(())
}

fn handle_image_command<W: Write>(
    writer: &mut W,
    workspace: &Path,
    model: &str,
    prompt: &str,
) -> ChatResult<()> {
    let prompt = prompt.trim();
    if prompt.is_empty() {
        writeln!(writer, "usage: /image <prompt>")?;
        return Ok(());
    }
    if !model_supports_image_generation(model) {
        writeln!(
            writer,
            "image generation is disabled for selected model `{model}`. Switch to an image-capable OpenAI model such as a GPT-5, GPT-4.1, GPT-4o, supported o-series, GPT Image, or DALL-E model."
        )?;
        return Ok(());
    }
    let api_key = resolve_env_value(workspace, "OPENAI_API_KEY").ok_or_else(|| {
        ChatError::Provider(
            "OPENAI_API_KEY is required for /image. Put it in the workspace .env or export it."
                .into(),
        )
    })?;
    let image_bytes = generate_image_with_openai(model, prompt, &api_key)?;
    let out_dir = workspace.join(".kimetsu").join("images");
    fs::create_dir_all(&out_dir)?;
    let path = out_dir.join(format!("image-{}.png", now_unix()));
    fs::write(&path, image_bytes)?;
    writeln!(writer, "image written: {}", path.display())?;
    Ok(())
}

fn handle_hooks_command<W: Write>(
    writer: &mut W,
    workspace: &Path,
    session: &ChatSession,
    hooks: &HookRegistry,
    arg: &str,
) -> ChatResult<()> {
    let mut parts = arg.split_whitespace();
    match parts.next().unwrap_or("list") {
        "" | "list" | "ls" => {
            if hooks.hooks.is_empty() {
                writeln!(writer, "hooks: none found")?;
                writeln!(
                    writer,
                    "add scripts under .kimetsu/hooks/<event>.ps1|.cmd|.bat|.sh|.exe"
                )?;
            } else {
                writeln!(writer, "{} hook(s):", hooks.hooks.len())?;
                for hook in &hooks.hooks {
                    writeln!(writer, "  {}  {}", hook.event.as_str(), hook.path.display())?;
                }
            }
        }
        "events" => {
            writeln!(
                writer,
                "events: session-start, session-end, pre-turn, post-turn"
            )?;
        }
        "run" => {
            let Some(event) = parts.next().and_then(HookEvent::parse) else {
                writeln!(writer, "usage: /hooks run <event>")?;
                return Ok(());
            };
            match hooks.run(event, workspace, session, Some("manual")) {
                Ok(report) => {
                    writeln!(writer, "ran {} hook(s)", report.ran)?;
                    for line in report.lines {
                        writeln!(writer, "  {line}")?;
                    }
                }
                Err(err) => writeln!(writer, "hook failed: {err}")?,
            }
        }
        other => writeln!(writer, "unknown /hooks command `{other}`")?,
    }
    Ok(())
}

fn handle_mcp_command<W: Write>(writer: &mut W, workspace: &Path, arg: &str) -> ChatResult<()> {
    let registry = McpRegistry::load(workspace)?;
    let mut parts = arg.splitn(4, char::is_whitespace);
    match parts.next().unwrap_or("list").trim() {
        "" | "list" | "ls" => {
            if registry.servers.is_empty() {
                writeln!(writer, "mcp: no servers configured")?;
                writeln!(writer, "add .kimetsu/mcp.json with a `servers` object")?;
            } else {
                writeln!(writer, "{} MCP server(s):", registry.servers.len())?;
                for server in registry.servers.values() {
                    writeln!(
                        writer,
                        "  {} -> {} {}",
                        server.name,
                        server.command,
                        server.args.join(" ")
                    )?;
                }
            }
        }
        "tools" => {
            let Some(server_name) = parts.next().map(str::trim).filter(|s| !s.is_empty()) else {
                writeln!(writer, "usage: /mcp tools <server>")?;
                return Ok(());
            };
            let server = registry.server(server_name)?;
            let tools = mcp_list_tools(server)?;
            if tools.is_empty() {
                writeln!(writer, "{}: no tools returned", server.name)?;
            } else {
                writeln!(writer, "{} tool(s):", tools.len())?;
                for tool in tools {
                    writeln!(
                        writer,
                        "  {} - {}",
                        tool.name,
                        tool.description.unwrap_or_default()
                    )?;
                }
            }
        }
        "call" => {
            let Some(server_name) = parts.next().map(str::trim).filter(|s| !s.is_empty()) else {
                writeln!(writer, "usage: /mcp call <server> <tool> [json-args]")?;
                return Ok(());
            };
            let Some(tool_name) = parts.next().map(str::trim).filter(|s| !s.is_empty()) else {
                writeln!(writer, "usage: /mcp call <server> <tool> [json-args]")?;
                return Ok(());
            };
            let args_text = parts.next().unwrap_or("").trim();
            let arguments = if args_text.is_empty() {
                serde_json::json!({})
            } else {
                serde_json::from_str(args_text)
                    .map_err(|err| ChatError::Provider(format!("invalid MCP JSON args: {err}")))?
            };
            let server = registry.server(server_name)?;
            let result = mcp_call_tool(server, tool_name, arguments)?;
            writeln!(
                writer,
                "{}",
                serde_json::to_string_pretty(&result).unwrap_or(result.to_string())
            )?;
        }
        other => writeln!(writer, "unknown /mcp command `{other}`")?,
    }
    Ok(())
}

struct AgentCommandContext<'a> {
    workspace: &'a Path,
    config: &'a ChatConfig,
    provider: &'a mut Option<Box<dyn kimetsu_agent::model::ModelProvider>>,
    cost: &'a mut CostMeter,
    transcript: &'a mut Vec<TurnRecord>,
    agent_manager: &'a mut AgentTaskManager,
}

fn handle_agents_command<W: Write>(
    writer: &mut W,
    ctx: &mut AgentCommandContext<'_>,
    arg: &str,
) -> ChatResult<()> {
    let registry = AgentRegistry::discover(ctx.workspace)?;
    let mut parts = arg.splitn(3, char::is_whitespace);
    match parts.next().unwrap_or("list").trim() {
        "" | "list" | "ls" => {
            ctx.agent_manager.write_list(writer)?;
            if registry.agents.is_empty() {
                writeln!(writer, "agents: none found")?;
                writeln!(writer, "add .kimetsu/agents/<name>.md or .json")?;
            } else {
                writeln!(writer, "{} agent(s):", registry.agents.len())?;
                for agent in &registry.agents {
                    writeln!(
                        writer,
                        "  {} - {} ({})",
                        agent.name,
                        agent.description.as_deref().unwrap_or("no description"),
                        agent.path.display()
                    )?;
                }
            }
        }
        "start" | "spawn" => {
            let Some(name) = parts.next().map(str::trim).filter(|s| !s.is_empty()) else {
                writeln!(writer, "usage: /agents start <name> <prompt>")?;
                return Ok(());
            };
            let prompt = parts.next().unwrap_or("").trim();
            if prompt.is_empty() {
                writeln!(writer, "usage: /agents start <name> <prompt>")?;
                return Ok(());
            }
            if ctx.cost.over_budget() {
                writeln!(writer, "budget exhausted; refusing agent start")?;
                return Ok(());
            }
            let agent = registry.agent(name)?.clone();
            let id = ctx.agent_manager.start(
                ctx.workspace,
                ctx.config,
                agent,
                prompt,
                ctx.transcript,
            )?;
            writeln!(writer, "started agent job {id}")?;
        }
        "output" | "show" => {
            let id = parts.next().map(str::trim).unwrap_or("");
            if id.is_empty() {
                writeln!(writer, "usage: /agents output <id>")?;
                return Ok(());
            }
            match ctx.agent_manager.take_result(id)? {
                Some(result) => {
                    if let Some(error) = result.error {
                        writeln!(writer, "agent {id} failed: {error}")?;
                    } else {
                        ctx.cost.record_turn(result.usage.cost_usd);
                        writeln!(writer, "agent {id}:")?;
                        writeln!(writer, "{}", result.text.trim_end())?;
                        ctx.transcript.push(TurnRecord {
                            user: format!("/agents output {id}"),
                            assistant: result.text,
                        });
                    }
                }
                None => writeln!(writer, "agent {id} is still running")?,
            }
        }
        "run" => {
            let Some(name) = parts.next().map(str::trim).filter(|s| !s.is_empty()) else {
                writeln!(writer, "usage: /agents run <name> <prompt>")?;
                return Ok(());
            };
            let prompt = parts.next().unwrap_or("").trim();
            if prompt.is_empty() {
                writeln!(writer, "usage: /agents run <name> <prompt>")?;
                return Ok(());
            }
            if ctx.cost.over_budget() {
                writeln!(writer, "budget exhausted; refusing agent run")?;
                return Ok(());
            }
            let agent = registry.agent(name)?;
            if ctx.provider.is_none() {
                *ctx.provider = Some(build_chat_provider(ctx.config, ctx.workspace)?);
            }
            let report = run_text_only_chat_with_system(
                ctx.provider.as_mut().expect("provider exists").as_mut(),
                &agent.system_prompt,
                prompt,
                ctx.transcript,
                "chat_subagent",
            )?;
            ctx.cost.record_turn(report.usage.cost_usd);
            writeln!(writer, "agent {}:", agent.name)?;
            writeln!(writer, "{}", report.text.trim_end())?;
            ctx.transcript.push(TurnRecord {
                user: format!("/agents run {} {}", agent.name, prompt),
                assistant: report.text,
            });
        }
        other => writeln!(writer, "unknown /agents command `{other}`")?,
    }
    Ok(())
}

fn handle_tasks_command<W: Write>(
    writer: &mut W,
    permission_mode: PermissionMode,
    terminal_available: bool,
    tasks: &mut TaskManager,
    arg: &str,
) -> ChatResult<()> {
    let trimmed = arg.trim();
    let (command, rest) = trimmed
        .split_once(char::is_whitespace)
        .map(|(command, rest)| (command.trim(), rest.trim()))
        .unwrap_or((if trimmed.is_empty() { "list" } else { trimmed }, ""));
    match command {
        "" | "list" | "ls" => tasks.write_list(writer),
        "run" | "start" => {
            if permission_mode == PermissionMode::ReadOnly {
                writeln!(writer, "/tasks run blocked by read-only permissions")?;
                return Ok(());
            }
            if rest.is_empty() {
                writeln!(writer, "usage: /tasks run <command>")?;
                return Ok(());
            }
            let request = RunCommandRequest::parse(rest);
            if request.command_line.trim().is_empty() {
                writeln!(writer, "usage: /tasks run <command>")?;
                return Ok(());
            }
            if request.mode == RunMode::Terminal {
                return tasks.run_terminal(writer, terminal_available, &request.command_line);
            }
            let id = tasks.start(&request.command_line)?;
            writeln!(writer, "started task {id}")?;
            Ok(())
        }
        "terminal" | "tty" | "interactive" => {
            if permission_mode == PermissionMode::ReadOnly {
                writeln!(writer, "/tasks terminal blocked by read-only permissions")?;
                return Ok(());
            }
            if rest.is_empty() {
                writeln!(writer, "usage: /tasks terminal <command>")?;
                return Ok(());
            }
            tasks.run_terminal(writer, terminal_available, rest)
        }
        "output" | "tail" => {
            let id = rest.split_whitespace().next().unwrap_or("");
            if id.is_empty() {
                writeln!(writer, "usage: /tasks output <id>")?;
                return Ok(());
            }
            tasks.write_output(writer, id)
        }
        "stop" | "kill" => {
            let id = rest.split_whitespace().next().unwrap_or("");
            if id.is_empty() {
                writeln!(writer, "usage: /tasks stop <id>")?;
                return Ok(());
            }
            tasks.stop(id)?;
            writeln!(writer, "stopped task {id}")?;
            Ok(())
        }
        other => writeln!(writer, "unknown /tasks command `{other}`").map_err(ChatError::Io),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HookEvent {
    SessionStart,
    SessionEnd,
    PreTurn,
    PostTurn,
}

impl HookEvent {
    fn parse(input: &str) -> Option<Self> {
        match input.trim().to_ascii_lowercase().as_str() {
            "session-start" | "session_start" | "start" => Some(Self::SessionStart),
            "session-end" | "session_end" | "end" => Some(Self::SessionEnd),
            "pre-turn" | "pre_turn" | "pre" => Some(Self::PreTurn),
            "post-turn" | "post_turn" | "post" => Some(Self::PostTurn),
            _ => None,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::SessionStart => "session-start",
            Self::SessionEnd => "session-end",
            Self::PreTurn => "pre-turn",
            Self::PostTurn => "post-turn",
        }
    }
}

#[derive(Debug, Clone)]
struct HookSpec {
    event: HookEvent,
    path: PathBuf,
}

#[derive(Debug, Default)]
struct HookRegistry {
    hooks: Vec<HookSpec>,
}

#[derive(Debug, Default)]
struct HookReport {
    ran: usize,
    lines: Vec<String>,
}

impl HookRegistry {
    fn discover(workspace: &Path) -> Self {
        let mut hooks = Vec::new();
        for root in [".kimetsu/hooks", ".claude/hooks", ".codex/hooks"] {
            let root = workspace.join(root);
            if !root.exists() {
                continue;
            }
            for event in [
                HookEvent::SessionStart,
                HookEvent::SessionEnd,
                HookEvent::PreTurn,
                HookEvent::PostTurn,
            ] {
                for ext in ["ps1", "cmd", "bat", "sh", "exe"] {
                    let path = root.join(format!("{}.{}", event.as_str(), ext));
                    if path.exists() {
                        hooks.push(HookSpec { event, path });
                    }
                }
            }
        }
        Self { hooks }
    }

    fn run(
        &self,
        event: HookEvent,
        workspace: &Path,
        session: &ChatSession,
        line: Option<&str>,
    ) -> ChatResult<HookReport> {
        let mut report = HookReport::default();
        for hook in self.hooks.iter().filter(|hook| hook.event == event) {
            let output = run_hook(hook, workspace, session, line)?;
            report.ran += 1;
            report.lines.push(format!(
                "{} exit={} {}",
                hook.path.display(),
                output.exit_code,
                preview(&output.stdout, 240)
            ));
            if output.exit_code != 0 {
                return Err(ChatError::Provider(format!(
                    "{} failed: {}",
                    hook.path.display(),
                    preview(&output.stderr, 800)
                )));
            }
        }
        Ok(report)
    }
}

#[derive(Debug)]
struct HookOutput {
    exit_code: i32,
    stdout: String,
    stderr: String,
}

fn run_hook(
    hook: &HookSpec,
    workspace: &Path,
    session: &ChatSession,
    line: Option<&str>,
) -> ChatResult<HookOutput> {
    let mut command = hook_command(&hook.path);
    command
        .current_dir(workspace)
        .env("KIMETSU_HOOK_EVENT", hook.event.as_str())
        .env("KIMETSU_WORKSPACE", workspace)
        .env("KIMETSU_SESSION_ID", &session.id)
        .env("KIMETSU_INPUT", line.unwrap_or(""))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    run_child_with_timeout(command, Duration::from_secs(30))
}

fn hook_command(path: &Path) -> Command {
    let path_string = process_path_string(path);
    match path.extension().and_then(|ext| ext.to_str()).unwrap_or("") {
        "ps1" => {
            let mut command = Command::new("powershell");
            command.args([
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
                &path_string,
            ]);
            command
        }
        "cmd" | "bat" => {
            let mut command = Command::new("cmd");
            command.args(["/C", &path_string]);
            command
        }
        "sh" => {
            let mut command = Command::new("bash");
            command.arg(path_string);
            command
        }
        _ => Command::new(path_string),
    }
}

fn process_path_string(path: &Path) -> String {
    let value = path.display().to_string();
    value
        .strip_prefix(r"\\?\")
        .unwrap_or(value.as_str())
        .to_string()
}

fn run_child_with_timeout(mut command: Command, timeout: Duration) -> ChatResult<HookOutput> {
    let started = Instant::now();
    let mut child = command.spawn()?;
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if started.elapsed() >= timeout {
            kill_child_tree(&mut child);
            return Err(ChatError::Provider("hook timed out".into()));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let output = child.wait_with_output()?;
    Ok(HookOutput {
        exit_code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    })
}

#[derive(Debug, Clone)]
struct McpServerConfig {
    name: String,
    command: String,
    args: Vec<String>,
    env: HashMap<String, String>,
    cwd: PathBuf,
}

#[derive(Debug, Default)]
struct McpRegistry {
    servers: HashMap<String, McpServerConfig>,
}

#[derive(Debug, Clone)]
struct McpToolInfo {
    name: String,
    description: Option<String>,
}

impl McpRegistry {
    fn load(workspace: &Path) -> ChatResult<Self> {
        let mut registry = Self::default();
        for path in [
            workspace.join(".kimetsu/mcp.json"),
            workspace.join(".claude/mcp.json"),
        ] {
            if !path.exists() {
                continue;
            }
            let text = fs::read_to_string(&path)?;
            let value: serde_json::Value = serde_json::from_str(&text)
                .map_err(|err| ChatError::Provider(format!("{}: {err}", path.display())))?;
            let servers = value
                .get("servers")
                .or_else(|| value.get("mcpServers"))
                .and_then(|v| v.as_object())
                .ok_or_else(|| {
                    ChatError::Provider(format!("{} has no `servers` object", path.display()))
                })?;
            for (name, raw) in servers {
                let command = raw
                    .get("command")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ChatError::Provider(format!("MCP `{name}` missing command")))?;
                let args = raw
                    .get("args")
                    .and_then(|v| v.as_array())
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                let env = raw
                    .get("env")
                    .and_then(|v| v.as_object())
                    .map(|map| {
                        map.iter()
                            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                            .collect()
                    })
                    .unwrap_or_default();
                registry.servers.insert(
                    name.clone(),
                    McpServerConfig {
                        name: name.clone(),
                        command: command.to_string(),
                        args,
                        env,
                        cwd: workspace.to_path_buf(),
                    },
                );
            }
        }
        Ok(registry)
    }

    fn server(&self, name: &str) -> ChatResult<&McpServerConfig> {
        self.servers
            .get(name)
            .ok_or_else(|| ChatError::Provider(format!("unknown MCP server `{name}`")))
    }
}

fn mcp_list_tools(server: &McpServerConfig) -> ChatResult<Vec<McpToolInfo>> {
    let mut session = McpStdioSession::start(server)?;
    session.initialize()?;
    let value = session.request("tools/list", serde_json::json!({}))?;
    let tools = value
        .get("tools")
        .and_then(|v| v.as_array())
        .map(|values| {
            values
                .iter()
                .filter_map(|tool| {
                    Some(McpToolInfo {
                        name: tool.get("name")?.as_str()?.to_string(),
                        description: tool
                            .get("description")
                            .and_then(|v| v.as_str())
                            .map(str::to_string),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(tools)
}

fn mcp_call_tool(
    server: &McpServerConfig,
    tool_name: &str,
    arguments: serde_json::Value,
) -> ChatResult<serde_json::Value> {
    let mut session = McpStdioSession::start(server)?;
    session.initialize()?;
    session.request(
        "tools/call",
        serde_json::json!({
            "name": tool_name,
            "arguments": arguments,
        }),
    )
}

fn handle_bridge_command<W: Write>(
    writer: &mut W,
    workspace: &Path,
    config: &SkillConfig,
    arg: &str,
) -> ChatResult<()> {
    let trimmed = arg.trim();
    let (command, rest) = trimmed
        .split_once(char::is_whitespace)
        .map(|(command, rest)| (command.trim(), rest.trim()))
        .unwrap_or((if trimmed.is_empty() { "scan" } else { trimmed }, ""));
    match command {
        "" | "scan" | "status" => {
            let scan = bridge_scan(workspace, config).map_err(ChatError::Skill)?;
            writeln!(writer, "bridge extensions: {}", scan.extensions.len())?;
            for extension in &scan.extensions {
                writeln!(
                    writer,
                    "  {} [{}] {}",
                    extension.manifest.name,
                    extension.manifest.source,
                    extension.root.display()
                )?;
            }
            writeln!(writer, "bridge skills: {}", scan.skills.len())?;
            for skill in &scan.skills {
                writeln!(
                    writer,
                    "  {} kimetsu_ext={} kimetsu={} claude={} codex={} origin={}",
                    skill.name,
                    skill.kimetsu_extension,
                    skill.kimetsu_skill,
                    skill.claude_skill,
                    skill.codex_skill,
                    skill.origin
                )?;
            }
        }
        "import" => {
            if rest.is_empty() {
                writeln!(writer, "usage: /bridge import <skill>")?;
                return Ok(());
            }
            let imported =
                bridge_import_skill(workspace, config, rest, false).map_err(ChatError::Skill)?;
            writeln!(
                writer,
                "imported {} into {}",
                imported.manifest.name,
                imported.root.display()
            )?;
        }
        "export" => {
            let mut parts = rest.split_whitespace();
            let Some(selection) = parts.next() else {
                writeln!(
                    writer,
                    "usage: /bridge export <skill> <claude|codex|kimetsu>"
                )?;
                return Ok(());
            };
            let Some(target_text) = parts.next() else {
                writeln!(
                    writer,
                    "usage: /bridge export <skill> <claude|codex|kimetsu>"
                )?;
                return Ok(());
            };
            let target = BridgeTarget::parse(target_text).map_err(ChatError::Skill)?;
            let exported = bridge_export_skill(workspace, config, selection, target, false)
                .map_err(ChatError::Skill)?;
            writeln!(
                writer,
                "exported {selection} to {} at {}",
                target.as_str(),
                exported.display()
            )?;
        }
        "sync" => {
            let imported = bridge_sync(workspace, config, false).map_err(ChatError::Skill)?;
            writeln!(
                writer,
                "imported {imported} skill bundle(s) into .kimetsu/extensions"
            )?;
        }
        other => writeln!(
            writer,
            "unknown /bridge command `{other}`. Use: /bridge scan | /bridge import <skill> | /bridge export <skill> <target> | /bridge sync"
        )?,
    }
    Ok(())
}

struct McpStdioSession {
    child: Child,
    stdin: std::process::ChildStdin,
    lines: mpsc::Receiver<String>,
    next_id: u64,
}

impl McpStdioSession {
    fn start(server: &McpServerConfig) -> ChatResult<Self> {
        let mut command = Command::new(&server.command);
        command
            .args(&server.args)
            .envs(&server.env)
            .current_dir(&server.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = command.spawn().map_err(|err| {
            ChatError::Provider(format!("spawn MCP `{}` failed: {err}", server.name))
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| ChatError::Provider("MCP stdin unavailable".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ChatError::Provider("MCP stdout unavailable".into()))?;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Ok(Self {
            child,
            stdin,
            lines: rx,
            next_id: 1,
        })
    }

    fn initialize(&mut self) -> ChatResult<()> {
        let _ = self.request(
            "initialize",
            serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "kimetsu-chat", "version": env!("CARGO_PKG_VERSION")}
            }),
        )?;
        self.notify("notifications/initialized", serde_json::json!({}))?;
        Ok(())
    }

    fn request(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> ChatResult<serde_json::Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.write_json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))?;
        self.read_response(id)
    }

    fn notify(&mut self, method: &str, params: serde_json::Value) -> ChatResult<()> {
        self.write_json(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
    }

    fn write_json(&mut self, value: &serde_json::Value) -> ChatResult<()> {
        writeln!(self.stdin, "{value}")?;
        self.stdin.flush()?;
        Ok(())
    }

    fn read_response(&mut self, id: u64) -> ChatResult<serde_json::Value> {
        let deadline = Duration::from_secs(15);
        loop {
            let line = self
                .lines
                .recv_timeout(deadline)
                .map_err(|_| ChatError::Provider("MCP response timed out".into()))?;
            let value: serde_json::Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if value.get("id").and_then(|v| v.as_u64()) != Some(id) {
                continue;
            }
            if let Some(error) = value.get("error") {
                return Err(ChatError::Provider(format!("MCP error: {error}")));
            }
            return Ok(value
                .get("result")
                .cloned()
                .unwrap_or(serde_json::Value::Null));
        }
    }
}

impl Drop for McpStdioSession {
    fn drop(&mut self) {
        kill_child_tree(&mut self.child);
    }
}

#[derive(Debug, Clone)]
struct AgentDefinition {
    name: String,
    description: Option<String>,
    system_prompt: String,
    path: PathBuf,
}

#[derive(Debug, Default)]
struct AgentRegistry {
    agents: Vec<AgentDefinition>,
}

impl AgentRegistry {
    fn discover(workspace: &Path) -> ChatResult<Self> {
        let mut agents = Vec::new();
        for path in discover_files(
            workspace,
            &[".kimetsu/agents", ".claude/agents", ".codex/agents"],
            3,
        ) {
            if let Some(agent) = load_agent_definition(&path)? {
                agents.push(agent);
            }
        }
        agents.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Self { agents })
    }

    fn agent(&self, name: &str) -> ChatResult<&AgentDefinition> {
        self.agents
            .iter()
            .find(|agent| agent.name == name || agent.name.starts_with(name))
            .ok_or_else(|| ChatError::Provider(format!("unknown agent `{name}`")))
    }
}

struct AgentTaskManager {
    jobs: HashMap<String, AgentTask>,
    next_id: u64,
}

struct AgentTask {
    agent_name: String,
    prompt: String,
    started_at: u64,
    receiver: mpsc::Receiver<AgentTaskResult>,
    result: Option<AgentTaskResult>,
}

#[derive(Debug, Clone)]
struct AgentTaskResult {
    text: String,
    usage: TokenUsage,
    error: Option<String>,
}

impl AgentTaskManager {
    fn new() -> Self {
        Self {
            jobs: HashMap::new(),
            next_id: 1,
        }
    }

    fn start(
        &mut self,
        workspace: &Path,
        config: &ChatConfig,
        agent: AgentDefinition,
        prompt: &str,
        transcript: &[TurnRecord],
    ) -> ChatResult<String> {
        let id = format!("agent-{}-{}", now_unix(), self.next_id);
        self.next_id += 1;
        let workspace = workspace.to_path_buf();
        let config = config.clone();
        let prompt = prompt.to_string();
        let job_prompt = prompt.clone();
        let transcript = transcript.to_vec();
        let agent_name = agent.name.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result = run_agent_task_job(&workspace, &config, &agent, &job_prompt, &transcript);
            let _ = tx.send(match result {
                Ok(result) => result,
                Err(err) => AgentTaskResult {
                    text: String::new(),
                    usage: TokenUsage::default(),
                    error: Some(err.to_string()),
                },
            });
        });
        self.jobs.insert(
            id.clone(),
            AgentTask {
                agent_name,
                prompt,
                started_at: now_unix(),
                receiver: rx,
                result: None,
            },
        );
        Ok(id)
    }

    fn refresh(&mut self) {
        for job in self.jobs.values_mut() {
            if job.result.is_none()
                && let Ok(result) = job.receiver.try_recv()
            {
                job.result = Some(result);
            }
        }
    }

    fn write_list<W: Write>(&mut self, writer: &mut W) -> ChatResult<()> {
        self.refresh();
        if self.jobs.is_empty() {
            writeln!(writer, "agent jobs: none")?;
            return Ok(());
        }
        writeln!(writer, "{} agent job(s):", self.jobs.len())?;
        for (id, job) in &self.jobs {
            let state = if job.result.is_some() {
                "done"
            } else {
                "running"
            };
            writeln!(
                writer,
                "  {}  {}  agent={}  started={}  {}",
                id,
                state,
                job.agent_name,
                job.started_at,
                preview(&job.prompt, 80)
            )?;
        }
        Ok(())
    }

    fn take_result(&mut self, id: &str) -> ChatResult<Option<AgentTaskResult>> {
        self.refresh();
        let Some(job) = self.jobs.get_mut(id) else {
            return Err(ChatError::Provider(format!("unknown agent job `{id}`")));
        };
        if job.result.is_none() {
            return Ok(None);
        }
        Ok(self.jobs.remove(id).and_then(|job| job.result))
    }
}

fn run_agent_task_job(
    workspace: &Path,
    config: &ChatConfig,
    agent: &AgentDefinition,
    prompt: &str,
    transcript: &[TurnRecord],
) -> ChatResult<AgentTaskResult> {
    let mut provider = build_chat_provider(config, workspace)?;
    let mut runtime = ToolRuntime::new(workspace, RunId::new())
        .map_err(|err| ChatError::Provider(format!("tool runtime: {err}")))?
        .with_config(ToolRuntimeConfig {
            trace_fsync: false,
            ..ToolRuntimeConfig::default()
        });
    let mut context = String::new();
    if !transcript.is_empty() {
        context.push_str("Main-thread context summary:\n");
        for turn in transcript.iter().rev().take(6).rev() {
            context.push_str(&format!(
                "user: {}\nassistant: {}\n",
                preview(&turn.user, 400),
                preview(&turn.assistant, 500)
            ));
        }
    }
    let task = format!(
        "Subagent `{}` instructions:\n{}\n\n{}\nTask:\n{}",
        agent.name, agent.system_prompt, context, prompt
    );
    let report = run_model_agent(
        &task,
        &mut runtime,
        provider.as_mut(),
        KimetsuAgentOpts {
            turn_budget: 24,
            auto_orient: true,
            min_actions_before_finish: 0,
            tool_loadout: ToolLoadout::ReadOnly,
            mode: "chat_subagent",
            task_source: "user",
            strict_verify: Some(false),
            self_verify_nudge_enabled: false,
        },
        None,
    )
    .map_err(|err| ChatError::Provider(format!("{err}")))?;
    Ok(AgentTaskResult {
        text: report.final_text.unwrap_or(report.summary),
        usage: report.usage,
        error: None,
    })
}

fn load_agent_definition(path: &Path) -> ChatResult<Option<AgentDefinition>> {
    let ext = path.extension().and_then(|ext| ext.to_str()).unwrap_or("");
    if !matches!(ext, "md" | "json") {
        return Ok(None);
    }
    let text = fs::read_to_string(path)?;
    if ext == "json" {
        let value: serde_json::Value = serde_json::from_str(&text)
            .map_err(|err| ChatError::Provider(format!("{}: {err}", path.display())))?;
        let name = value
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| {
                path.file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string()
            });
        let description = value
            .get("description")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let system_prompt = value
            .get("prompt")
            .or_else(|| value.get("system_prompt"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if system_prompt.is_empty() {
            return Ok(None);
        }
        return Ok(Some(AgentDefinition {
            name,
            description,
            system_prompt,
            path: path.to_path_buf(),
        }));
    }
    let (metadata, body) = parse_frontmatter(&text);
    let name = metadata.get("name").cloned().unwrap_or_else(|| {
        path.file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string()
    });
    let description = metadata.get("description").cloned();
    Ok(Some(AgentDefinition {
        name,
        description,
        system_prompt: body.trim().to_string(),
        path: path.to_path_buf(),
    }))
}

fn parse_frontmatter(text: &str) -> (HashMap<String, String>, String) {
    let mut metadata = HashMap::new();
    let mut lines = text.lines();
    if lines.next() != Some("---") {
        return (metadata, text.to_string());
    }
    let mut body_start = 0usize;
    let mut consumed = 4usize;
    for line in lines {
        if line == "---" {
            body_start = consumed + line.len() + 1;
            break;
        }
        consumed += line.len() + 1;
        if let Some((key, value)) = line.split_once(':') {
            metadata.insert(
                key.trim().to_string(),
                value.trim().trim_matches('"').to_string(),
            );
        }
    }
    let body = text.get(body_start..).unwrap_or("").to_string();
    (metadata, body)
}

struct TaskManager {
    workspace: PathBuf,
    task_dir: PathBuf,
    tasks: HashMap<String, ChatTask>,
    next_id: u64,
}

struct ChatTask {
    command_line: String,
    started_at: u64,
    out_path: PathBuf,
    err_path: PathBuf,
    child: Child,
    exit_code: Option<i32>,
}

impl TaskManager {
    fn new(workspace: PathBuf) -> Self {
        let task_dir = workspace.join(".kimetsu").join("chat").join("tasks");
        Self {
            workspace,
            task_dir,
            tasks: HashMap::new(),
            next_id: 1,
        }
    }

    fn start(&mut self, command_line: &str) -> ChatResult<String> {
        fs::create_dir_all(&self.task_dir)?;
        let spec = parse_command_line(command_line)?;
        let id = format!("task-{}-{}", now_unix(), self.next_id);
        self.next_id += 1;
        let out_path = self.task_dir.join(format!("{id}.out"));
        let err_path = self.task_dir.join(format!("{id}.err"));
        let stdout = File::create(&out_path)?;
        let stderr = File::create(&err_path)?;
        let mut child = Command::new(&spec.program);
        child
            .args(&spec.args)
            .current_dir(&self.workspace)
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .stdin(Stdio::null());
        let child = child.spawn()?;
        self.tasks.insert(
            id.clone(),
            ChatTask {
                command_line: command_line.to_string(),
                started_at: now_unix(),
                out_path,
                err_path,
                child,
                exit_code: None,
            },
        );
        Ok(id)
    }

    fn run_terminal<W: Write>(
        &self,
        writer: &mut W,
        terminal_available: bool,
        command_line: &str,
    ) -> ChatResult<()> {
        run_terminal_command(
            writer,
            &self.workspace,
            terminal_available,
            "tasks",
            command_line,
        )
    }

    fn refresh(&mut self) {
        for task in self.tasks.values_mut() {
            if task.exit_code.is_none()
                && let Ok(Some(status)) = task.child.try_wait()
            {
                task.exit_code = Some(status.code().unwrap_or(-1));
            }
        }
    }

    fn write_list<W: Write>(&mut self, writer: &mut W) -> ChatResult<()> {
        self.refresh();
        if self.tasks.is_empty() {
            writeln!(writer, "tasks: none")?;
            writeln!(writer, "start one with `/tasks run <command>`")?;
            return Ok(());
        }
        for (id, task) in &self.tasks {
            let state = match task.exit_code {
                Some(code) => format!("exited({code})"),
                None => "running".to_string(),
            };
            writeln!(
                writer,
                "{}  {}  started={}  {}",
                id, state, task.started_at, task.command_line
            )?;
        }
        Ok(())
    }

    fn write_output<W: Write>(&mut self, writer: &mut W, id: &str) -> ChatResult<()> {
        self.refresh();
        let task = self
            .tasks
            .get(id)
            .ok_or_else(|| ChatError::Provider(format!("unknown task `{id}`")))?;
        writeln!(writer, "stdout:\n{}", tail_file(&task.out_path, 12_000)?)?;
        writeln!(writer, "stderr:\n{}", tail_file(&task.err_path, 12_000)?)?;
        Ok(())
    }

    fn stop(&mut self, id: &str) -> ChatResult<()> {
        let task = self
            .tasks
            .get_mut(id)
            .ok_or_else(|| ChatError::Provider(format!("unknown task `{id}`")))?;
        kill_child_tree(&mut task.child);
        task.exit_code = Some(-1);
        Ok(())
    }

    fn stop_all(&mut self) -> usize {
        self.refresh();
        let mut stopped = 0;
        for task in self.tasks.values_mut() {
            if task.exit_code.is_none() {
                kill_child_tree(&mut task.child);
                task.exit_code = Some(-1);
                stopped += 1;
            }
        }
        stopped
    }
}

fn tail_file(path: &Path, max_chars: usize) -> ChatResult<String> {
    if !path.exists() {
        return Ok(String::new());
    }
    let text = fs::read_to_string(path)?;
    let char_count = text.chars().count();
    if char_count <= max_chars {
        Ok(text)
    } else {
        Ok(text.chars().skip(char_count - max_chars).collect())
    }
}

fn kill_child_tree(child: &mut Child) {
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &child.id().to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn handle_doctor_command<W: Write>(
    writer: &mut W,
    workspace: &Path,
    config: &ChatConfig,
    session_dir: &Path,
) -> ChatResult<()> {
    writeln!(writer, "kimetsu chat doctor")?;
    writeln!(writer, "workspace exists: {}", workspace.exists())?;
    writeln!(
        writer,
        "claude token: {}",
        resolve_env_value(workspace, "CLAUDE_CODE_OAUTH_TOKEN").is_some()
    )?;
    writeln!(
        writer,
        "openai key for /image: {}",
        resolve_env_value(workspace, "OPENAI_API_KEY").is_some()
    )?;
    writeln!(
        writer,
        "selected model: {} (image_generation={})",
        config.model,
        model_supports_image_generation(&config.model)
    )?;
    writeln!(
        writer,
        "hooks found: {}",
        HookRegistry::discover(workspace).hooks.len()
    )?;
    writeln!(
        writer,
        "mcp servers: {}",
        McpRegistry::load(workspace)
            .map(|registry| registry.servers.len())
            .unwrap_or(0)
    )?;
    writeln!(
        writer,
        "agents found: {}",
        AgentRegistry::discover(workspace)
            .map(|registry| registry.agents.len())
            .unwrap_or(0)
    )?;
    fs::create_dir_all(session_dir)?;
    writeln!(writer, "session dir writable: {}", session_dir.exists())?;
    match run_git(workspace, &["rev-parse", "--is-inside-work-tree"]) {
        Ok(value) => writeln!(writer, "git worktree: {}", value.trim())?,
        Err(err) => writeln!(writer, "git worktree: false ({err})")?,
    }
    Ok(())
}

fn create_checkpoint(
    workspace: &Path,
    transcript: &[TurnRecord],
    name: &str,
    kind: &str,
) -> ChatResult<CheckpointRecord> {
    let diff = current_git_diff(workspace)?;
    let now = now_unix();
    Ok(CheckpointRecord {
        id: format!("cp-{now}-{}", transcript.len()),
        name: (!name.trim().is_empty()).then(|| name.trim().to_string()),
        kind: kind.to_string(),
        created_at: now,
        transcript_len: transcript.len(),
        pre_diff: diff.clone(),
        post_diff: diff,
    })
}

fn undo_last_checkpoint(
    workspace: &Path,
    checkpoints: &mut Vec<CheckpointRecord>,
) -> ChatResult<Option<CheckpointRecord>> {
    let Some(checkpoint) = checkpoints.pop() else {
        return Ok(None);
    };
    let current = current_git_diff(workspace)?;
    if current != checkpoint.post_diff {
        checkpoints.push(checkpoint);
        return Err(ChatError::Provider(
            "working tree changed since the last checkpoint; refusing automatic undo".into(),
        ));
    }
    if !checkpoint.post_diff.trim().is_empty() {
        git_apply(
            workspace,
            &["apply", "-R", "--whitespace=nowarn"],
            &checkpoint.post_diff,
        )?;
    }
    if !checkpoint.pre_diff.trim().is_empty() {
        git_apply(
            workspace,
            &["apply", "--whitespace=nowarn"],
            &checkpoint.pre_diff,
        )?;
    }
    Ok(Some(checkpoint))
}

fn export_transcript(
    workspace: &Path,
    session: &ChatSession,
    transcript: &[TurnRecord],
    requested_path: &str,
) -> ChatResult<PathBuf> {
    let path = if requested_path.trim().is_empty() {
        workspace
            .join(".kimetsu")
            .join("chat")
            .join(format!("{}.md", session.id))
    } else {
        let p = PathBuf::from(requested_path.trim());
        if p.is_absolute() {
            p
        } else {
            workspace.join(p)
        }
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut out = String::new();
    out.push_str(&format!("# Kimetsu Chat {}\n\n", session.id));
    for (idx, turn) in transcript.iter().enumerate() {
        out.push_str(&format!("## Turn {}\n\n", idx + 1));
        out.push_str("### User\n\n");
        out.push_str(&turn.user);
        out.push_str("\n\n### Assistant\n\n");
        out.push_str(&turn.assistant);
        out.push_str("\n\n");
    }
    fs::write(&path, out)?;
    Ok(path)
}

fn chat_session_dir(workspace: &Path) -> PathBuf {
    workspace.join(".kimetsu").join("chat").join("sessions")
}

fn persist_session(
    enabled: bool,
    session_dir: &Path,
    session: &ChatSession,
    transcript: &[TurnRecord],
) -> ChatResult<()> {
    if !enabled {
        return Ok(());
    }
    fs::create_dir_all(session_dir)?;
    let mut saved = session.clone();
    saved.transcript = transcript.to_vec();
    saved.updated_at = now_unix();
    let path = session_dir.join(format!("{}.json", saved.id));
    let json = serde_json::to_string_pretty(&saved)
        .map_err(|err| ChatError::Provider(format!("serialize session: {err}")))?;
    fs::write(path, json)?;
    Ok(())
}

fn list_saved_sessions(session_dir: &Path) -> ChatResult<Vec<ChatSession>> {
    if !session_dir.exists() {
        return Ok(Vec::new());
    }
    let mut sessions = Vec::new();
    for entry in fs::read_dir(session_dir)? {
        let entry = entry?;
        if entry.path().extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = fs::read_to_string(entry.path()) else {
            continue;
        };
        if let Ok(session) = serde_json::from_str::<ChatSession>(&text) {
            sessions.push(session);
        }
    }
    sessions.sort_by_key(|session| std::cmp::Reverse(session.updated_at));
    Ok(sessions)
}

fn current_git_diff(workspace: &Path) -> ChatResult<String> {
    git_diff_for_paths(workspace, false, &[])
}

fn git_diff_for_paths(workspace: &Path, staged: bool, paths: &[String]) -> ChatResult<String> {
    let mut args = vec!["diff", "--no-ext-diff"];
    if staged {
        args.push("--staged");
    }
    args.push("--");
    let path_refs = paths.iter().map(String::as_str).collect::<Vec<_>>();
    args.extend(path_refs);
    run_git(workspace, &args)
}

fn run_git(workspace: &Path, args: &[&str]) -> ChatResult<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(workspace)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;
    if !output.status.success() {
        return Err(ChatError::Provider(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn git_apply(workspace: &Path, args: &[&str], patch: &str) -> ChatResult<()> {
    let mut child = Command::new("git")
        .args(args)
        .current_dir(workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(stdin) = child.stdin.as_mut() {
        stdin.write_all(patch.as_bytes())?;
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(ChatError::Provider(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

fn parse_command_line(line: &str) -> ChatResult<CommandSpec> {
    let parts = split_command_line(line)?;
    let Some((program, args)) = parts.split_first() else {
        return Err(ChatError::Provider("empty command".into()));
    };
    Ok(CommandSpec {
        program: program.clone(),
        args: args.to_vec(),
        cwd_relative: None,
        timeout_secs: Some(600),
        expected_exit: None,
    })
}

fn split_command_line(line: &str) -> ChatResult<Vec<String>> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\'' | '"' if quote == Some(ch) => quote = None,
            '\'' | '"' if quote.is_none() => quote = Some(ch),
            '\\' if quote != Some('\'') => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            ch if ch.is_whitespace() && quote.is_none() => {
                if !current.is_empty() {
                    parts.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }
    if quote.is_some() {
        return Err(ChatError::Provider("unterminated quote in command".into()));
    }
    if !current.is_empty() {
        parts.push(current);
    }
    Ok(parts)
}

fn infer_project_command(workspace: &Path, verify: bool) -> Option<String> {
    if workspace.join("Cargo.toml").exists() {
        return Some(if verify {
            "cargo test --workspace".to_string()
        } else {
            "cargo check --workspace".to_string()
        });
    }
    if workspace.join("package.json").exists() {
        return Some(if verify {
            "npm test".to_string()
        } else {
            "npm run build".to_string()
        });
    }
    if workspace.join("pyproject.toml").exists() || workspace.join("pytest.ini").exists() {
        return Some("pytest".to_string());
    }
    if workspace.join("Makefile").exists() {
        return Some(if verify {
            "make test".to_string()
        } else {
            "make".to_string()
        });
    }
    None
}

fn model_supports_image_generation(model: &str) -> bool {
    let m = model.trim().to_ascii_lowercase();
    m.starts_with("gpt-5")
        || m.starts_with("gpt-4.1")
        || m.starts_with("gpt-4o")
        || matches!(m.as_str(), "o1" | "o3" | "o3-mini" | "o4-mini")
        || m.starts_with("gpt-image-")
        || m == "chatgpt-image-latest"
        || m == "dall-e-2"
        || m == "dall-e-3"
}

fn image_model_uses_image_api(model: &str) -> bool {
    let m = model.trim().to_ascii_lowercase();
    m.starts_with("gpt-image-") || m == "chatgpt-image-latest" || m == "dall-e-2" || m == "dall-e-3"
}

fn generate_image_with_openai(model: &str, prompt: &str, api_key: &str) -> ChatResult<Vec<u8>> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(180))
        .build()
        .map_err(|err| ChatError::Provider(format!("openai client: {err}")))?;
    let request = if image_model_uses_image_api(model) {
        client
            .post("https://api.openai.com/v1/images/generations")
            .bearer_auth(api_key)
            .json(&serde_json::json!({
                "model": model,
                "prompt": prompt,
                "size": "1024x1024",
                "quality": "medium",
                "n": 1
            }))
    } else {
        client
            .post("https://api.openai.com/v1/responses")
            .bearer_auth(api_key)
            .json(&serde_json::json!({
                "model": model,
                "input": prompt,
                "tools": [{"type": "image_generation", "action": "generate"}]
            }))
    };
    let response = request
        .send()
        .map_err(|err| ChatError::Provider(format!("openai image request: {err}")))?;
    let status = response.status();
    let value: serde_json::Value = response
        .json()
        .map_err(|err| ChatError::Provider(format!("openai image response: {err}")))?;
    if !status.is_success() {
        return Err(ChatError::Provider(format!(
            "openai image request failed ({status}): {}",
            preview(&value.to_string(), 800)
        )));
    }
    let b64 = find_image_b64(&value)
        .ok_or_else(|| ChatError::Provider("openai response did not include image data".into()))?;
    general_purpose::STANDARD
        .decode(b64)
        .map_err(|err| ChatError::Provider(format!("decode image: {err}")))
}

fn find_image_b64(value: &serde_json::Value) -> Option<&str> {
    if let Some(s) = value.get("b64_json").and_then(|v| v.as_str()) {
        return Some(s);
    }
    if let Some(s) = value.get("result").and_then(|v| v.as_str()) {
        return Some(s);
    }
    match value {
        serde_json::Value::Array(values) => values.iter().find_map(find_image_b64),
        serde_json::Value::Object(map) => map.values().find_map(find_image_b64),
        _ => None,
    }
}

fn discover_files(workspace: &Path, rels: &[&str], max_depth: usize) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for rel in rels {
        collect_files(&workspace.join(rel), max_depth, &mut out);
    }
    out
}

fn collect_files(path: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth == 0 || !path.exists() {
        return;
    }
    if path.is_file() {
        out.push(path.to_path_buf());
        return;
    }
    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        let child = entry.path();
        if child.is_file() {
            out.push(child);
        } else if child.is_dir() {
            collect_files(&child, depth - 1, out);
        }
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn approx_tokens(chars: usize) -> usize {
    chars.div_ceil(4)
}

/// v0.3.3: one user/assistant exchange in the chat transcript. Used to
/// render the conversational history into the brain-context block on
/// the next turn so the model sees its own previous responses.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct TurnRecord {
    user: String,
    assistant: String,
}

/// v0.3.3: a single MP-18 record_deviation call captured from a turn.
/// Accumulated for the session; on /quit we walk these and write each
/// `lesson_for_next_time` to the brain pool as a failure_pattern
/// memory - closes the MP-18 feedback loop in chat mode.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct DeviationRecord {
    #[allow(dead_code)]
    missing: String,
    #[allow(dead_code)]
    where_deviated: String,
    lesson_for_next_time: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum PermissionMode {
    ReadOnly,
    Auto,
    Full,
}

impl PermissionMode {
    fn parse(input: &str) -> Option<Self> {
        match input.trim().to_ascii_lowercase().as_str() {
            "read-only" | "readonly" | "read" | "suggest" => Some(Self::ReadOnly),
            "auto" | "auto-edit" | "default" => Some(Self::Auto),
            "full" | "full-auto" | "full-access" | "yolo" => Some(Self::Full),
            _ => None,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::Auto => "auto",
            Self::Full => "full",
        }
    }

    const fn description(self) -> &'static str {
        match self {
            Self::ReadOnly => "inspect files only; blocks workspace-agent write/shell tasks",
            Self::Auto => "dynamic tools; edit and shell profiles load only when needed",
            Self::Full => "full Kimetsu tool surface from the first agent turn",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum ReasoningEffort {
    Low,
    Medium,
    High,
    XHigh,
}

impl ReasoningEffort {
    fn parse(input: &str) -> Option<Self> {
        match input.trim().to_ascii_lowercase().as_str() {
            "low" => Some(Self::Low),
            "medium" | "med" | "default" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" | "extra-high" | "extra" | "max" => Some(Self::XHigh),
            _ => None,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CheckpointRecord {
    id: String,
    name: Option<String>,
    kind: String,
    created_at: u64,
    transcript_len: usize,
    pre_diff: String,
    post_diff: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChatSession {
    id: String,
    title: Option<String>,
    created_at: u64,
    updated_at: u64,
    model: String,
    permission_mode: PermissionMode,
    reasoning_effort: ReasoningEffort,
    goal: Option<String>,
    transcript: Vec<TurnRecord>,
    checkpoints: Vec<CheckpointRecord>,
}

impl ChatSession {
    fn new(model: &str, permission_mode: PermissionMode, goal: Option<&str>) -> Self {
        let now = now_unix();
        Self {
            id: format!("chat-{now}-{}", std::process::id()),
            title: None,
            created_at: now,
            updated_at: now,
            model: model.to_string(),
            permission_mode,
            reasoning_effort: ReasoningEffort::Medium,
            goal: goal.map(str::to_string),
            transcript: Vec::new(),
            checkpoints: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ChatRoute {
    Local(String),
    TextOnly,
    WorkspaceRead,
    WorkspaceAgent,
}

#[derive(Debug, Clone)]
struct TextOnlyReport {
    text: String,
    usage: TokenUsage,
}

fn classify_chat_route(line: &str, goal: Option<&str>, skills_loaded: bool) -> ChatRoute {
    if goal.map(|goal| !goal.trim().is_empty()).unwrap_or(false) || skills_loaded {
        return ChatRoute::WorkspaceAgent;
    }
    if let Some(reply) = local_reply(line) {
        return ChatRoute::Local(reply);
    }
    if looks_like_workspace_write_task(line) {
        ChatRoute::WorkspaceAgent
    } else if looks_like_workspace_read_task(line) {
        ChatRoute::WorkspaceRead
    } else {
        ChatRoute::TextOnly
    }
}

fn local_reply(line: &str) -> Option<String> {
    let normalized = normalize_user_line(line);
    match normalized.as_str() {
        "hi" | "hello" | "hey" | "hiya" | "yo" | "good morning" | "good afternoon"
        | "good evening" => Some(
            "Hi. I'm Kimetsu, your terminal coding agent. What would you like to work on?"
                .to_string(),
        ),
        "who are you" | "what are you" | "what is kimetsu" => Some(
            "I'm Kimetsu, an evidence-first terminal coding agent running inside this chat harness. I can answer directly or use workspace tools when you ask me to inspect, edit, run, or verify something."
                .to_string(),
        ),
        "what can you do" | "help me" => Some(
            "I can answer questions directly, inspect this workspace, edit files, run commands, use project memory, and load Agent Skills when a task needs them."
                .to_string(),
        ),
        "thanks" | "thank you" | "thx" => Some("You're welcome.".to_string()),
        _ => None,
    }
}

fn normalize_user_line(line: &str) -> String {
    line.trim()
        .trim_start_matches('\u{feff}')
        .trim_matches(|ch: char| ch.is_ascii_punctuation() || ch.is_whitespace())
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn looks_like_workspace_write_task(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    contains_any(
        &lower,
        &[
            "fix ",
            "fixes",
            "bug",
            "implement",
            "add ",
            "create ",
            "update ",
            "change ",
            "modify",
            "edit",
            "refactor",
            "delete",
            "remove",
            "move ",
            "rename",
            "write ",
            "patch",
            "commit",
            "push",
            "run ",
            "test",
            "build",
            "lint",
            "format",
            "cargo ",
            "npm ",
            "pnpm ",
            "pytest",
            "make ",
            "compile",
            "verify",
        ],
    )
}

fn looks_like_workspace_read_task(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    contains_any(
        &lower,
        &[
            "repo",
            "repository",
            "workspace",
            "codebase",
            "source",
            "file",
            "folder",
            "crate",
            "module",
            "function",
            "struct",
            "class",
            "readme",
            "docs",
            "where is",
            "where are",
            "find ",
            "search",
            "inspect",
            "look at",
            "open ",
            ".rs",
            ".py",
            ".ts",
            ".tsx",
            ".js",
            ".md",
            "src/",
            "crates/",
            "docs/",
        ],
    )
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

fn run_text_only_chat(
    provider: &mut dyn kimetsu_agent::model::ModelProvider,
    task: &str,
    transcript: &[TurnRecord],
) -> ChatResult<TextOnlyReport> {
    run_text_only_chat_with_system(
        provider,
        "You are Kimetsu. Answer concise conversational questions directly. Do not claim to have inspected files or run tools in this text-only route.",
        task,
        transcript,
        "chat_text_only",
    )
}

fn run_text_only_chat_with_system(
    provider: &mut dyn kimetsu_agent::model::ModelProvider,
    system_prompt: &str,
    task: &str,
    transcript: &[TurnRecord],
    mode: &str,
) -> ChatResult<TextOnlyReport> {
    let mut context = String::new();
    if !transcript.is_empty() {
        context.push_str("Conversation so far:\n");
        for turn in transcript.iter().rev().take(6).rev() {
            context.push_str(&format!(
                "user: {}\nassistant: {}\n",
                preview(&turn.user, 300),
                preview(&turn.assistant, 300)
            ));
        }
        context.push('\n');
    }

    let response = provider
        .complete(ModelRequest {
            messages: vec![
                ModelMessage {
                    role: MessageRole::System,
                    content: vec![MessageContent::Text {
                        text: system_prompt.to_string(),
                    }],
                },
                ModelMessage::user_text(format!("{context}User message:\n{task}")),
            ],
            tools: Vec::new(),
            tool_choice: ToolChoice::None,
            max_output_tokens: 1024,
            temperature: 0.0,
            metadata: serde_json::json!({
                "kimetsu_mode": mode,
                "task_preview": preview(task, 120),
            }),
        })
        .map_err(|err| ChatError::Provider(format!("{err}")))?;

    let text = response.text.unwrap_or_else(|| match response.stop_reason {
        StopReason::Refusal => "I can't answer that request.".to_string(),
        StopReason::MaxTokens => "I hit the output limit while answering.".to_string(),
        _ => "I couldn't produce a text answer for that message.".to_string(),
    });
    Ok(TextOnlyReport {
        text,
        usage: response.usage,
    })
}

/// v0.3.3: build the brain-context string for one chat turn.
///
/// Two concatenated blocks (either may be empty):
///   1. Retrieved brain capsules (if --project is set) - surfaced from
///      `kimetsu_brain::project::retrieve_context` with the current
///      task as the query, 2000-token budget, stage="chat".
///   2. Conversation transcript so far (if any prior turns) - last
///      ~10 turns of user/assistant exchanges, each capped so a long
///      session doesn't blow the prompt budget.
///
/// The agent loop renders this whole string as a "Prior knowledge"
/// block before the new user message. The model gets memory-pool
/// retrieval AND conversational continuity through a single channel.
fn build_chat_brain_context(
    brain_session: Option<&brain_project::BrainSession>,
    transcript: &[TurnRecord],
    loaded_skills: &[LoadedSkill],
    task: &str,
) -> Option<String> {
    const TURN_CAP_BYTES: usize = 1200;
    const TRANSCRIPT_TAIL: usize = 10;

    let mut out = String::new();

    // Block 0: explicit skills loaded by the user.
    if let Some(skill_context) = render_loaded_skills(loaded_skills) {
        out.push_str(&skill_context);
    }

    // Block 1: retrieved capsules.
    if let Some(session) = brain_session {
        match session.retrieve_context("chat", task, 2000) {
            Ok(bundle) if !bundle.capsules.is_empty() => {
                if !out.is_empty() {
                    out.push('\n');
                }
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
                // We deliberately stay silent in the prompt - no
                // need to tell the model the brain was empty.
            }
            Err(_) => {
                // Silent failure: brain retrieval is best-effort.
                // If the DB is missing or corrupt the user will
                // see it on the brain CLI; don't pollute the prompt.
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

    if out.is_empty() { None } else { Some(out) }
}

/// Build the model provider for chat. Today: claude_code via
/// CLAUDE_CODE_OAUTH_TOKEN from the environment or workspace .env.
/// Future: switch on `config.model` prefix
/// (anthropic API key vs CC OAuth).
fn build_chat_provider(
    config: &ChatConfig,
    workspace: &std::path::Path,
) -> ChatResult<Box<dyn kimetsu_agent::model::ModelProvider>> {
    let mut project_cfg = ProjectConfig::default_for_project("kimetsu-chat");
    project_cfg.model.provider = "claude_code".to_string();
    project_cfg.model.model = config.model.clone();
    project_cfg.model.api_key_env = "CLAUDE_CODE_OAUTH_TOKEN".to_string();
    project_cfg.model.request_timeout_secs = std::env::var("KIMETSU_PROVIDER_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(1500);
    project_cfg.run.max_total_cost_usd = config.max_cost_usd;

    let oauth =
        resolve_env_value(workspace, "CLAUDE_CODE_OAUTH_TOKEN").ok_or(ChatError::NoOauthToken)?;

    match ClaudeCodeProvider::from_config_with_key_and_persistent_default(
        workspace,
        &project_cfg,
        Some(&oauth),
        true,
    ) {
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
        format!("{head}...")
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
    let _ = brain_project::list_memories(p).map_err(|e| ChatError::BrainProject(format!("{e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Cursor;
    use std::time::{SystemTime, UNIX_EPOCH};

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
        // CLAUDE_CODE_OAUTH_TOKEN the provider build fails - non-fatal,
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
    fn greeting_is_handled_without_provider_or_cost() {
        unsafe {
            std::env::remove_var("CLAUDE_CODE_OAUTH_TOKEN");
        }
        let input = b"hi!\n/cost\n/quit\n";
        let mut output = Vec::new();
        let config = ChatConfig::new(std::env::current_dir().unwrap());
        run_repl(Cursor::new(input), &mut output, config).expect("repl");
        let out_str = String::from_utf8(output).unwrap();
        assert!(out_str.contains("Hi. I'm Kimetsu"));
        assert!(out_str.contains("cost so far: $0.0000"));
        assert!(!out_str.contains("CLAUDE_CODE_OAUTH_TOKEN"));
    }

    #[test]
    fn chat_route_distinguishes_conversation_from_workspace_work() {
        assert!(matches!(
            classify_chat_route("who are you?", None, false),
            ChatRoute::Local(_)
        ));
        assert_eq!(
            classify_chat_route("what is ownership in rust?", None, false),
            ChatRoute::TextOnly
        );
        assert_eq!(
            classify_chat_route("what files are in this repo?", None, false),
            ChatRoute::WorkspaceRead
        );
        assert_eq!(
            classify_chat_route("fix the failing tests", None, false),
            ChatRoute::WorkspaceAgent
        );
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
        assert!(out_str.contains("/clear"));
        assert!(out_str.contains("/logo"));
    }

    #[test]
    fn slash_palette_filters_and_completes_commands() {
        let all = matching_slash_commands("/");
        assert!(all.iter().any(|spec| spec.command == "/help"));
        assert!(all.iter().any(|spec| spec.command == "/skills"));

        let skills = matching_slash_commands("/ski");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].command, "/skills");
        assert_eq!(
            selected_slash_completion_for_enter("/ski", 0),
            Some("/skills ".to_string())
        );

        let cost = matching_slash_commands("/cos");
        assert_eq!(cost.len(), 1);
        assert_eq!(cost[0].command, "/cost");
        assert_eq!(
            selected_slash_completion_for_enter("/cos", 0),
            Some("/cost".to_string())
        );
        assert_eq!(selected_slash_completion_for_enter("/skills list", 0), None);
    }

    #[test]
    fn run_command_request_parses_terminal_flags() {
        let request = RunCommandRequest::parse("--terminal npm create vite@latest");
        assert_eq!(request.mode, RunMode::Terminal);
        assert_eq!(request.command_line, "npm create vite@latest");

        let request = RunCommandRequest::parse("--tty -- npm --version");
        assert_eq!(request.mode, RunMode::Terminal);
        assert_eq!(request.command_line, "npm --version");

        let request = RunCommandRequest::parse("--terminal --capture cargo check");
        assert_eq!(request.mode, RunMode::Captured);
        assert_eq!(request.command_line, "cargo check");
    }

    #[test]
    fn skill_prefix_parses_name_and_prompt() {
        assert_eq!(
            parse_skill_prefix("$review inspect the diff"),
            Some(("review".to_string(), "inspect the diff".to_string()))
        );
        assert_eq!(
            parse_skill_prefix(" $frontend"),
            Some(("frontend".to_string(), String::new()))
        );
        assert_eq!(parse_skill_prefix("not a skill"), None);
    }

    #[test]
    fn file_mentions_expand_existing_files() {
        let root = temp_root("chat_mentions");
        fs::write(root.join("README.md"), "hello from readme").expect("readme");
        let expanded = expand_file_mentions("summarize @README.md", &root).expect("expanded");
        assert!(expanded.contains("Mentioned files"));
        assert!(expanded.contains("hello from readme"));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn input_history_moves_backward_and_forward() {
        let mut state = ChatInputState::default();
        state.record("first");
        state.record("second");
        let (idx, line) = state.previous(None).expect("previous");
        assert_eq!(line, "second");
        let (idx, line) = state.previous(Some(idx)).expect("previous");
        assert_eq!(line, "first");
        let (next, line) = state.next(Some(idx)).expect("next");
        assert!(next.is_some());
        assert_eq!(line, "second");
    }

    #[test]
    fn terminal_run_requires_interactive_terminal() {
        let input =
            b"/run --terminal cargo --version\n/tasks run --terminal cargo --version\n/quit\n";
        let mut output = Vec::new();
        let config = ChatConfig::new(std::env::current_dir().unwrap());
        run_repl(Cursor::new(input), &mut output, config).expect("repl");
        let out_str = String::from_utf8(output).unwrap();
        assert!(out_str.contains("[run:terminal] requires an interactive terminal"));
        assert!(out_str.contains("[tasks:terminal] requires an interactive terminal"));
        assert!(out_str.contains("bye."));
    }

    #[test]
    fn image_generation_is_model_gated() {
        assert!(model_supports_image_generation("gpt-5.5"));
        assert!(model_supports_image_generation("gpt-image-1.5"));
        assert!(model_supports_image_generation("dall-e-3"));
        assert!(!model_supports_image_generation("claude-opus-4-7"));
        assert!(!model_supports_image_generation("llama-local"));
    }

    #[test]
    fn hook_registry_discovers_event_scripts() {
        let root = temp_root("chat_hooks");
        let dir = root.join(".kimetsu/hooks");
        fs::create_dir_all(&dir).expect("hooks dir");
        fs::write(dir.join("pre-turn.cmd"), "@echo off\necho ok\n").expect("hook");

        let hooks = HookRegistry::discover(&root);
        assert_eq!(hooks.hooks.len(), 1);
        assert_eq!(hooks.hooks[0].event, HookEvent::PreTurn);
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn mcp_registry_loads_json_servers() {
        let root = temp_root("chat_mcp");
        fs::create_dir_all(root.join(".kimetsu")).expect("mcp dir");
        fs::write(
            root.join(".kimetsu/mcp.json"),
            r#"{"servers":{"fake":{"command":"node","args":["server.js"],"env":{"A":"B"}}}}"#,
        )
        .expect("mcp config");

        let registry = McpRegistry::load(&root).expect("mcp registry");
        let server = registry.server("fake").expect("fake server");
        assert_eq!(server.command, "node");
        assert_eq!(server.args, vec!["server.js"]);
        assert_eq!(server.env.get("A").map(String::as_str), Some("B"));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn agent_registry_loads_markdown_agent() {
        let root = temp_root("chat_agents");
        let dir = root.join(".kimetsu/agents");
        fs::create_dir_all(&dir).expect("agents dir");
        fs::write(
            dir.join("reviewer.md"),
            "---\nname: reviewer\ndescription: Review changes.\n---\nLead with findings.",
        )
        .expect("agent");

        let registry = AgentRegistry::discover(&root).expect("agents");
        let agent = registry.agent("review").expect("reviewer");
        assert_eq!(agent.name, "reviewer");
        assert_eq!(agent.description.as_deref(), Some("Review changes."));
        assert!(agent.system_prompt.contains("Lead with findings"));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn slash_clear_redraws_banner_and_keeps_session_alive() {
        let input = b"/clear\n/quit\n";
        let mut output = Vec::new();
        let config = ChatConfig::new(std::env::current_dir().unwrap());
        run_repl(Cursor::new(input), &mut output, config).expect("repl");
        let out_str = String::from_utf8(output).unwrap();
        assert!(out_str.contains("[clear] conversation context cleared"));
        assert!(out_str.matches("kimetsu chat v").count() >= 2);
        assert!(out_str.contains("bye."));
    }

    #[test]
    fn rich_ui_renders_dragon_banner() {
        let input = b"/quit\n";
        let mut output = Vec::new();
        let mut config = ChatConfig::new(std::env::current_dir().unwrap());
        config.ui = crate::ui::ChatUi::rich();
        run_repl(Cursor::new(input), &mut output, config).expect("repl");
        let out_str = String::from_utf8(output).unwrap();
        assert!(out_str.contains("\u{1b}["));
        assert!(out_str.contains("( o.o )"));
        assert!(out_str.contains("powers"));
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

    #[test]
    fn slash_skills_lists_and_loads_workspace_skill() {
        let root = temp_root("chat_skills");
        let skill_dir = root.join(".codex/skills/refactor");
        fs::create_dir_all(&skill_dir).expect("skill dir");
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: refactor\ndescription: Refactor safely.\n---\nAlways run focused tests.",
        )
        .expect("write skill");

        let input = b"/skills list\n/skills use refactor\n/skills loaded\n/quit\n";
        let mut output = Vec::new();
        let config = ChatConfig::new(root.clone());
        run_repl(Cursor::new(input), &mut output, config).expect("repl");
        let out_str = String::from_utf8(output).unwrap();
        assert!(out_str.contains("refactor [codex; no resources]"));
        assert!(out_str.contains("[skills] loaded `refactor`"));
        assert!(out_str.contains("loaded skills: refactor"));
        fs::remove_dir_all(root).ok();
    }

    // ----- v0.3.3: multi-turn transcript + brain retrieval helpers -----

    #[test]
    fn build_chat_brain_context_returns_none_when_no_state() {
        // No project, no transcript -> nothing to surface.
        let out = build_chat_brain_context(None, &[], &[], "do something");
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
        let out = build_chat_brain_context(None, &transcript, &[], "yes please")
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
        let out = build_chat_brain_context(None, &transcript, &[], "next").expect("context");
        // 1200/2 = 600-char cap per side; should be << 5000 chars.
        // Plus the cap appends "..." only when actually truncated, and
        // preview uses char count not bytes. Verify clipping happened.
        assert!(out.contains("aaa"));
        assert!(out.contains("bbb"));
        assert!(out.contains("..."), "expected the ... truncation marker");
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
        let out = build_chat_brain_context(None, &transcript, &[], "next").expect("context");
        // Last 10 means turns 16..=25 are present, 1..=15 are gone.
        assert!(out.contains("user message 25"));
        assert!(out.contains("user message 16"));
        assert!(!out.contains("user message 15"));
        assert!(!out.contains("user message 1\n"));
        assert!(out.contains("last 10 turn(s)"));
    }

    fn temp_root(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("kimetsu_{label}_{nanos}"));
        fs::create_dir_all(&root).expect("root");
        root
    }
}
