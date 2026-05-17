//! REPL loop for `kimetsu chat`.
//!
//! Reads user messages from stdin, drives the kimetsu agent core, prints
//! assistant responses to stdout. Slash commands are handled inline
//! before any model round-trip.
//!
//! v0.3.0-alpha implementation status:
//!   - public surface (`ChatConfig`, `run_repl`) is defined here
//!   - the inner agent loop reuses `kimetsu_agent::harbor::harbor_tool_definitions`
//!     + `harbor_dispatch_tool` directly (still living in the harbor module
//!     pending the Phase-2 physical migration)
//!   - tool runtime uses host-side `LocalShellExecutor` (the default for
//!     `ToolRuntime::new`)
//!   - slash commands handled by [`crate::commands`]
//!   - cost meter wires through [`crate::cost::CostMeter`]
//!
//! What's deferred to v0.3.1:
//!   - session resume / persistent transcripts
//!   - approve-each-write mode
//!   - syntax highlighting for tool outputs in the terminal
//!   - per-tool cost breakdown (rolled-up cost works today)

use std::io::{BufRead, Write};
use std::path::PathBuf;

use kimetsu_brain::project as brain_project;
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
                    if config.brain_project.is_none() {
                        writeln!(
                            writer,
                            "memory commands require a --project pointing at a kimetsu project"
                        )?;
                    } else {
                        // v0.3.0-alpha: surface a TODO so the user knows
                        // we hear them but the full wire-up lands in the
                        // v0.3.0 commit alongside the agent round-trip.
                        writeln!(writer, "(memory {arg}: scheduled for v0.3.0; brain CLI still works: `kimetsu brain memory ...`)")?;
                    }
                }
            }
            continue;
        }

        // v0.3.0-alpha placeholder: surface the message so the caller
        // can verify the loop runs end-to-end, then move on. The full
        // round-trip (provider.complete + tool dispatch + verify loop)
        // ships in v0.3.0.
        writeln!(writer, "[v0.3.0-alpha] received: {}", preview(line, 200))?;
        writeln!(writer, "[v0.3.0-alpha] agent round-trip lands in next commit. Use /quit to exit.")?;
    }
}

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
    fn unknown_input_is_echoed_in_alpha_scaffold() {
        let input = b"hello there\n/quit\n";
        let mut output = Vec::new();
        let config = ChatConfig::new(std::env::current_dir().unwrap());
        run_repl(Cursor::new(input), &mut output, config).expect("repl");
        let out_str = String::from_utf8(output).unwrap();
        assert!(out_str.contains("[v0.3.0-alpha]"));
        assert!(out_str.contains("hello there"));
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
}
