//! Slash commands parsed inline before any model round-trip.
//!
//! Patterns mirror the conversational coding-agent UX users already
//! know from Claude Code (`/help`, `/cost`, `/clear`) and Codex
//! (`/loop`, `/goal`), with kimetsu-specific additions for the brain
//! and MP-18's verify-strictness toggle.

use std::io::Write;

/// One parsed slash command. Returned by [`SlashCommand::parse`]; if
/// the user's input doesn't begin with `/`, parsing returns `None` and
/// the caller forwards the line to the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    /// `/help` — print available commands.
    Help,
    /// `/quit` — end the session.
    Quit,
    /// `/cost` — print running cost vs budget.
    Cost,
    /// `/goal [<text>]` — set or recall the current session goal.
    /// Empty text = recall.
    Goal(String),
    /// `/strict on|off` — toggle MP-18 strict verify (model MUST call
    /// `record_deviation` on every fix-up cycle).
    Strict(bool),
    /// `/memory <subcommand>` — defer to kimetsu-brain CLI semantics.
    /// In v0.3.0-alpha this just prints a TODO; v0.3.0 wires it to
    /// `kimetsu_brain::project::add_memory` / proposals.
    Memory(String),
}

impl SlashCommand {
    pub fn parse(line: &str) -> Option<Self> {
        let stripped = line.strip_prefix('/')?;
        let mut iter = stripped.splitn(2, char::is_whitespace);
        let cmd = iter.next().unwrap_or("").trim();
        let rest = iter.next().unwrap_or("").trim();
        match cmd {
            "help" | "h" | "?" => Some(Self::Help),
            "quit" | "q" | "exit" => Some(Self::Quit),
            "cost" => Some(Self::Cost),
            "goal" => Some(Self::Goal(rest.to_string())),
            "strict" => {
                let on = matches!(
                    rest.to_ascii_lowercase().as_str(),
                    "on" | "true" | "1" | "yes"
                );
                Some(Self::Strict(on))
            }
            "memory" | "m" | "brain" => Some(Self::Memory(rest.to_string())),
            // Unknown slash commands fall through to the agent — the
            // model can interpret them or surface a "I don't know what
            // /foo means" response. Better than swallowing user intent.
            _ => None,
        }
    }

    pub fn print_help<W: Write>(out: &mut W) -> std::io::Result<()> {
        writeln!(out, "slash commands:")?;
        writeln!(out, "  /help                show this list")?;
        writeln!(out, "  /quit                end the session")?;
        writeln!(out, "  /cost                running cost + budget")?;
        writeln!(
            out,
            "  /goal [<text>]       set the session goal (empty = recall)"
        )?;
        writeln!(
            out,
            "  /strict on|off       toggle MP-18 strict verify mode"
        )?;
        writeln!(
            out,
            "  /memory <args>       brain memory operations (see kimetsu brain memory)"
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_commands() {
        assert_eq!(SlashCommand::parse("/help"), Some(SlashCommand::Help));
        assert_eq!(SlashCommand::parse("/h"), Some(SlashCommand::Help));
        assert_eq!(SlashCommand::parse("/?"), Some(SlashCommand::Help));
        assert_eq!(SlashCommand::parse("/quit"), Some(SlashCommand::Quit));
        assert_eq!(SlashCommand::parse("/exit"), Some(SlashCommand::Quit));
        assert_eq!(SlashCommand::parse("/cost"), Some(SlashCommand::Cost));
    }

    #[test]
    fn parses_goal_with_and_without_text() {
        assert_eq!(
            SlashCommand::parse("/goal"),
            Some(SlashCommand::Goal(String::new()))
        );
        assert_eq!(
            SlashCommand::parse("/goal refactor errors to thiserror"),
            Some(SlashCommand::Goal(
                "refactor errors to thiserror".to_string()
            ))
        );
    }

    #[test]
    fn parses_strict_truthy_and_falsy() {
        assert_eq!(
            SlashCommand::parse("/strict on"),
            Some(SlashCommand::Strict(true))
        );
        assert_eq!(
            SlashCommand::parse("/strict ON"),
            Some(SlashCommand::Strict(true))
        );
        assert_eq!(
            SlashCommand::parse("/strict true"),
            Some(SlashCommand::Strict(true))
        );
        assert_eq!(
            SlashCommand::parse("/strict 1"),
            Some(SlashCommand::Strict(true))
        );
        assert_eq!(
            SlashCommand::parse("/strict yes"),
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
    fn parses_memory_arguments() {
        assert_eq!(
            SlashCommand::parse("/memory list"),
            Some(SlashCommand::Memory("list".to_string()))
        );
        assert_eq!(
            SlashCommand::parse("/memory add some text"),
            Some(SlashCommand::Memory("add some text".to_string()))
        );
    }

    #[test]
    fn non_slash_input_is_not_a_command() {
        assert_eq!(SlashCommand::parse("hello world"), None);
        assert_eq!(SlashCommand::parse(""), None);
        assert_eq!(SlashCommand::parse(" /quit"), None); // leading space disqualifies
    }

    #[test]
    fn unknown_slash_falls_through_to_agent() {
        // /foo isn't recognized; parse returns None so the caller
        // forwards it to the model.
        assert_eq!(SlashCommand::parse("/foo bar"), None);
    }
}
