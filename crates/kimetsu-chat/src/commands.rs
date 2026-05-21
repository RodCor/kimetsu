//! Slash commands parsed inline before any model round-trip.
//!
//! Patterns mirror the conversational coding-agent UX users already
//! know from Claude Code and Codex, while keeping command behavior owned
//! by the Kimetsu chat surface.

use std::io::Write;

/// Metadata for one slash command. Kept separate from parsing so the
/// interactive terminal can render and filter the same command surface
/// that `/help` documents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlashCommandSpec {
    pub command: &'static str,
    pub args: &'static str,
    pub summary: &'static str,
    pub accepts_args: bool,
}

impl SlashCommandSpec {
    pub fn display(self) -> String {
        if self.args.is_empty() {
            self.command.to_string()
        } else {
            format!("{} {}", self.command, self.args)
        }
    }

    pub fn completion(self) -> String {
        if self.accepts_args {
            format!("{} ", self.command)
        } else {
            self.command.to_string()
        }
    }
}

pub const SLASH_COMMANDS: &[SlashCommandSpec] = &[
    SlashCommandSpec {
        command: "/help",
        args: "",
        summary: "show this list",
        accepts_args: false,
    },
    SlashCommandSpec {
        command: "/status",
        args: "",
        summary: "session, model, permission, git, and image status",
        accepts_args: false,
    },
    SlashCommandSpec {
        command: "/context",
        args: "",
        summary: "show conversation and context usage",
        accepts_args: false,
    },
    SlashCommandSpec {
        command: "/compact",
        args: "[instructions]",
        summary: "summarize the session to reduce context pressure",
        accepts_args: true,
    },
    SlashCommandSpec {
        command: "/plan",
        args: "[task]",
        summary: "enter read-only planning mode for the next task",
        accepts_args: true,
    },
    SlashCommandSpec {
        command: "/transcript",
        args: "[all|last N]",
        summary: "show recent conversation turns in the terminal",
        accepts_args: true,
    },
    SlashCommandSpec {
        command: "/copy",
        args: "[N]",
        summary: "copy the last or Nth-latest assistant response",
        accepts_args: true,
    },
    SlashCommandSpec {
        command: "/diff",
        args: "[paths]",
        summary: "show current git status and diff",
        accepts_args: true,
    },
    SlashCommandSpec {
        command: "/checkpoint",
        args: "[name]",
        summary: "save a tracked-files rollback point",
        accepts_args: true,
    },
    SlashCommandSpec {
        command: "/undo",
        args: "",
        summary: "restore the last clean checkpoint",
        accepts_args: false,
    },
    SlashCommandSpec {
        command: "/new",
        args: "[name]",
        summary: "start a fresh saved chat session",
        accepts_args: true,
    },
    SlashCommandSpec {
        command: "/resume",
        args: "[id|last]",
        summary: "list or load saved chat sessions",
        accepts_args: true,
    },
    SlashCommandSpec {
        command: "/export",
        args: "[path]",
        summary: "write the transcript to Markdown",
        accepts_args: true,
    },
    SlashCommandSpec {
        command: "/permissions",
        args: "[read-only|auto|full]",
        summary: "view or change workspace permissions",
        accepts_args: true,
    },
    SlashCommandSpec {
        command: "/model",
        args: "[name]",
        summary: "view or switch the selected model",
        accepts_args: true,
    },
    SlashCommandSpec {
        command: "/effort",
        args: "[low|medium|high|xhigh]",
        summary: "set the session reasoning hint",
        accepts_args: true,
    },
    SlashCommandSpec {
        command: "/theme",
        args: "[rich|plain]",
        summary: "switch terminal theme for this session",
        accepts_args: true,
    },
    SlashCommandSpec {
        command: "/statusline",
        args: "[on|off]",
        summary: "toggle compact status above the prompt",
        accepts_args: true,
    },
    SlashCommandSpec {
        command: "/raw",
        args: "[on|off]",
        summary: "toggle raw scrollback-friendly output mode",
        accepts_args: true,
    },
    SlashCommandSpec {
        command: "/keybindings",
        args: "",
        summary: "show terminal composer shortcuts",
        accepts_args: false,
    },
    SlashCommandSpec {
        command: "/goal",
        args: "[text]",
        summary: "set or show the session goal",
        accepts_args: true,
    },
    SlashCommandSpec {
        command: "/strict",
        args: "on|off",
        summary: "toggle strict verify mode",
        accepts_args: true,
    },
    SlashCommandSpec {
        command: "/cost",
        args: "",
        summary: "running cost and budget",
        accepts_args: false,
    },
    SlashCommandSpec {
        command: "/skills",
        args: "[select|search|list|sources|use|install|loaded|clear]",
        summary: "search, load, or import Agent Skills folders",
        accepts_args: true,
    },
    SlashCommandSpec {
        command: "/memory",
        args: "<args>",
        summary: "brain memory operations",
        accepts_args: true,
    },
    SlashCommandSpec {
        command: "/run",
        args: "[--terminal] [command]",
        summary: "run a local command; --terminal lets interactive CLIs own the terminal",
        accepts_args: true,
    },
    SlashCommandSpec {
        command: "/verify",
        args: "[--terminal] [command]",
        summary: "run the project verification recipe; --terminal for interactive checks",
        accepts_args: true,
    },
    SlashCommandSpec {
        command: "/image",
        args: "<prompt>",
        summary: "generate an image only when the selected model supports it",
        accepts_args: true,
    },
    SlashCommandSpec {
        command: "/hooks",
        args: "[list|run <event>]",
        summary: "list or run project hook events",
        accepts_args: true,
    },
    SlashCommandSpec {
        command: "/mcp",
        args: "[list|tools <server>|call <server> <tool> <json>]",
        summary: "list or call configured stdio MCP servers",
        accepts_args: true,
    },
    SlashCommandSpec {
        command: "/bridge",
        args: "[scan|import|export|sync]",
        summary: "bridge extensions across Kimetsu, Claude Code, and Codex",
        accepts_args: true,
    },
    SlashCommandSpec {
        command: "/agents",
        args: "[list|run|start|output]",
        summary: "list, run, or collect local agent jobs",
        accepts_args: true,
    },
    SlashCommandSpec {
        command: "/tasks",
        args: "[list|run|terminal|output|stop]",
        summary: "manage background tasks or hand a command the terminal",
        accepts_args: true,
    },
    SlashCommandSpec {
        command: "/review",
        args: "[focus]",
        summary: "run a read-only review of the current diff",
        accepts_args: true,
    },
    SlashCommandSpec {
        command: "/security-review",
        args: "[focus]",
        summary: "review current changes for security risks",
        accepts_args: true,
    },
    SlashCommandSpec {
        command: "/simplify",
        args: "[focus]",
        summary: "review changed files for quality and efficiency fixes",
        accepts_args: true,
    },
    SlashCommandSpec {
        command: "/doctor",
        args: "",
        summary: "diagnose local Kimetsu chat setup",
        accepts_args: false,
    },
    SlashCommandSpec {
        command: "/debug-config",
        args: "",
        summary: "show effective local chat configuration sources",
        accepts_args: false,
    },
    SlashCommandSpec {
        command: "/clear",
        args: "",
        summary: "clear transcript and redraw terminal",
        accepts_args: false,
    },
    SlashCommandSpec {
        command: "/logo",
        args: "",
        summary: "show the Kimetsu dragon banner",
        accepts_args: false,
    },
    SlashCommandSpec {
        command: "/quit",
        args: "",
        summary: "end the session",
        accepts_args: false,
    },
];

/// One parsed slash command. Returned by [`SlashCommand::parse`]; if
/// the user's input doesn't begin with `/`, parsing returns `None` and
/// the caller forwards the line to the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    Help,
    Quit,
    Cost,
    Status,
    Context,
    Compact(String),
    Plan(String),
    Transcript(String),
    Copy(String),
    Diff(String),
    Checkpoint(String),
    Undo,
    New(String),
    Resume(String),
    Export(String),
    Permissions(String),
    Model(String),
    Effort(String),
    Theme(String),
    StatusLine(String),
    Raw(String),
    Keybindings,
    Clear,
    Logo,
    Goal(String),
    Strict(bool),
    Memory(String),
    Skills(String),
    Run(String),
    Verify(String),
    Image(String),
    Hooks(String),
    Mcp(String),
    Bridge(String),
    Agents(String),
    Tasks(String),
    Review(String),
    SecurityReview(String),
    Simplify(String),
    Doctor,
    DebugConfig,
}

impl SlashCommand {
    pub fn parse(line: &str) -> Option<Self> {
        let line = line.trim().trim_start_matches('\u{feff}').trim();
        let stripped = line.strip_prefix('/')?;
        let mut iter = stripped.splitn(2, char::is_whitespace);
        let cmd = iter.next().unwrap_or("").trim();
        let rest = iter.next().unwrap_or("").trim();
        match cmd {
            "" => Some(Self::Help),
            "help" | "h" | "?" => Some(Self::Help),
            "quit" | "q" | "exit" => Some(Self::Quit),
            "cost" | "usage" | "stats" => Some(Self::Cost),
            "status" => Some(Self::Status),
            "context" | "ctx" => Some(Self::Context),
            "compact" | "summarize" => Some(Self::Compact(rest.to_string())),
            "plan" => Some(Self::Plan(rest.to_string())),
            "transcript" | "history" => Some(Self::Transcript(rest.to_string())),
            "copy" => Some(Self::Copy(rest.to_string())),
            "diff" => Some(Self::Diff(rest.to_string())),
            "checkpoint" | "check" => Some(Self::Checkpoint(rest.to_string())),
            "undo" | "rewind" => Some(Self::Undo),
            "new" | "reset" => Some(Self::New(rest.to_string())),
            "resume" | "continue" => Some(Self::Resume(rest.to_string())),
            "export" => Some(Self::Export(rest.to_string())),
            "permissions" | "permission" | "perms" | "mode" => {
                Some(Self::Permissions(rest.to_string()))
            }
            "model" => Some(Self::Model(rest.to_string())),
            "effort" | "reason" | "reasoning" => Some(Self::Effort(rest.to_string())),
            "theme" | "color" => Some(Self::Theme(rest.to_string())),
            "statusline" | "status-line" => Some(Self::StatusLine(rest.to_string())),
            "raw" => Some(Self::Raw(rest.to_string())),
            "keybindings" | "keymap" | "keys" => Some(Self::Keybindings),
            "clear" | "cls" => Some(Self::Clear),
            "logo" => Some(Self::Logo),
            "goal" => Some(Self::Goal(rest.to_string())),
            "strict" => {
                let on = matches!(
                    rest.to_ascii_lowercase().as_str(),
                    "on" | "true" | "1" | "yes"
                );
                Some(Self::Strict(on))
            }
            "memory" | "m" | "brain" => Some(Self::Memory(rest.to_string())),
            "skills" | "skill" => Some(Self::Skills(rest.to_string())),
            "run" => Some(Self::Run(rest.to_string())),
            "verify" => Some(Self::Verify(rest.to_string())),
            "image" | "img" => Some(Self::Image(rest.to_string())),
            "hooks" | "hook" => Some(Self::Hooks(rest.to_string())),
            "mcp" => Some(Self::Mcp(rest.to_string())),
            "bridge" => Some(Self::Bridge(rest.to_string())),
            "agents" | "agent" => Some(Self::Agents(rest.to_string())),
            "tasks" | "task" | "bashes" => Some(Self::Tasks(rest.to_string())),
            "review" => Some(Self::Review(rest.to_string())),
            "security-review" | "security" | "sec-review" => {
                Some(Self::SecurityReview(rest.to_string()))
            }
            "simplify" => Some(Self::Simplify(rest.to_string())),
            "doctor" => Some(Self::Doctor),
            "debug-config" => Some(Self::DebugConfig),
            _ => None,
        }
    }

    pub fn print_help<W: Write>(out: &mut W) -> std::io::Result<()> {
        writeln!(out, "slash commands:")?;
        writeln!(out, "  /                    open command palette")?;
        for spec in SLASH_COMMANDS {
            writeln!(out, "  {:<24} {}", spec.display(), spec.summary)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_commands() {
        assert_eq!(SlashCommand::parse("/"), Some(SlashCommand::Help));
        assert_eq!(SlashCommand::parse("/help"), Some(SlashCommand::Help));
        assert_eq!(SlashCommand::parse("/quit"), Some(SlashCommand::Quit));
        assert_eq!(SlashCommand::parse("/cost"), Some(SlashCommand::Cost));
        assert_eq!(SlashCommand::parse("/status"), Some(SlashCommand::Status));
        assert_eq!(SlashCommand::parse("/context"), Some(SlashCommand::Context));
        assert_eq!(
            SlashCommand::parse("/copy"),
            Some(SlashCommand::Copy(String::new()))
        );
        assert_eq!(SlashCommand::parse("/undo"), Some(SlashCommand::Undo));
        assert_eq!(SlashCommand::parse("/clear"), Some(SlashCommand::Clear));
        assert_eq!(SlashCommand::parse("/logo"), Some(SlashCommand::Logo));
        assert_eq!(SlashCommand::parse("/doctor"), Some(SlashCommand::Doctor));
        assert_eq!(
            SlashCommand::parse("/debug-config"),
            Some(SlashCommand::DebugConfig)
        );
        assert_eq!(
            SlashCommand::parse("/keybindings"),
            Some(SlashCommand::Keybindings)
        );
    }

    #[test]
    fn parses_argument_commands() {
        assert_eq!(
            SlashCommand::parse("/goal refactor errors"),
            Some(SlashCommand::Goal("refactor errors".to_string()))
        );
        assert_eq!(
            SlashCommand::parse("/diff crates/kimetsu-chat"),
            Some(SlashCommand::Diff("crates/kimetsu-chat".to_string()))
        );
        assert_eq!(
            SlashCommand::parse("/permissions read-only"),
            Some(SlashCommand::Permissions("read-only".to_string()))
        );
        assert_eq!(
            SlashCommand::parse("/image chibi dragon"),
            Some(SlashCommand::Image("chibi dragon".to_string()))
        );
        assert_eq!(
            SlashCommand::parse("/plan migrate parser"),
            Some(SlashCommand::Plan("migrate parser".to_string()))
        );
        assert_eq!(
            SlashCommand::parse("/compact focus on decisions"),
            Some(SlashCommand::Compact("focus on decisions".to_string()))
        );
        assert_eq!(
            SlashCommand::parse("/transcript last 3"),
            Some(SlashCommand::Transcript("last 3".to_string()))
        );
        assert_eq!(
            SlashCommand::parse("/review tests"),
            Some(SlashCommand::Review("tests".to_string()))
        );
        assert_eq!(
            SlashCommand::parse("/security-review auth"),
            Some(SlashCommand::SecurityReview("auth".to_string()))
        );
        assert_eq!(
            SlashCommand::parse("/simplify"),
            Some(SlashCommand::Simplify(String::new()))
        );
        assert_eq!(
            SlashCommand::parse("/theme plain"),
            Some(SlashCommand::Theme("plain".to_string()))
        );
    }

    #[test]
    fn parses_strict_truthy_and_falsy() {
        assert_eq!(
            SlashCommand::parse("/strict on"),
            Some(SlashCommand::Strict(true))
        );
        assert_eq!(
            SlashCommand::parse("/strict off"),
            Some(SlashCommand::Strict(false))
        );
        assert_eq!(
            SlashCommand::parse("/strict"),
            Some(SlashCommand::Strict(false))
        );
    }

    #[test]
    fn parses_memory_and_skills_arguments() {
        assert_eq!(
            SlashCommand::parse("/memory list"),
            Some(SlashCommand::Memory("list".to_string()))
        );
        assert_eq!(
            SlashCommand::parse("/skills list"),
            Some(SlashCommand::Skills("list".to_string()))
        );
        assert_eq!(
            SlashCommand::parse("/bridge scan"),
            Some(SlashCommand::Bridge("scan".to_string()))
        );
    }

    #[test]
    fn non_slash_input_is_not_a_command() {
        assert_eq!(SlashCommand::parse("hello world"), None);
        assert_eq!(SlashCommand::parse(""), None);
        assert_eq!(SlashCommand::parse(" /quit"), Some(SlashCommand::Quit));
        assert_eq!(
            SlashCommand::parse("\u{feff}/help"),
            Some(SlashCommand::Help)
        );
    }

    #[test]
    fn unknown_slash_falls_through_to_agent() {
        assert_eq!(SlashCommand::parse("/foo bar"), None);
    }
}
