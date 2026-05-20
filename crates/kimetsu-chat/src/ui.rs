use std::io::{self, Write};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatUiMode {
    Plain,
    Rich,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChatUi {
    pub mode: ChatUiMode,
    pub show_logo: bool,
}

impl Default for ChatUi {
    fn default() -> Self {
        Self::plain()
    }
}

impl ChatUi {
    pub const fn plain() -> Self {
        Self {
            mode: ChatUiMode::Plain,
            show_logo: true,
        }
    }

    pub const fn rich() -> Self {
        Self {
            mode: ChatUiMode::Rich,
            show_logo: true,
        }
    }

    pub const fn with_logo(mut self, show_logo: bool) -> Self {
        self.show_logo = show_logo;
        self
    }

    pub const fn is_rich(self) -> bool {
        matches!(self.mode, ChatUiMode::Rich)
    }

    pub fn write_banner<W: Write>(&self, out: &mut W, banner: ChatBanner<'_>) -> io::Result<()> {
        if self.is_rich() {
            writeln!(out, "{}", self.paint(BOLD_RED, "kimetsu chat"))?;
            writeln!(out, "{}", self.paint(DIM, &format!("v{}", banner.version)))?;
        } else {
            writeln!(out, "kimetsu chat v{}", banner.version)?;
        }

        if self.show_logo {
            for line in DRAGON_LOGO {
                writeln!(out, "{}", self.paint(RED, line))?;
            }
        }

        self.write_kv(out, "workspace", &banner.workspace.display().to_string())?;
        self.write_kv(
            out,
            "brain",
            &banner
                .brain_project
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "<none>".to_string()),
        )?;
        self.write_kv(out, "model", banner.model)?;
        self.write_kv(
            out,
            "skills",
            &format!(
                "{} loaded / {} available",
                banner.skills_loaded, banner.skills_available
            ),
        )?;
        self.write_kv(
            out,
            "budget",
            &format!(
                "${:.2}  strict_verify={}",
                banner.budget, banner.strict_verify
            ),
        )?;
        if let Some(goal) = banner.goal {
            self.write_kv(out, "goal", goal)?;
        }
        self.write_kv(
            out,
            "powers",
            "read, edit, patch, shell, verify inside workspace",
        )?;
        self.write_kv(out, "controls", "/help  /skills  /clear  /cost  /quit")?;
        Ok(())
    }

    pub fn write_prompt<W: Write>(&self, out: &mut W) -> io::Result<()> {
        if self.is_rich() {
            write!(
                out,
                "\n{} {} ",
                self.paint(BOLD_CYAN, "you"),
                self.paint(DIM, ">")
            )
        } else {
            write!(out, "\nyou> ")
        }
    }

    pub fn write_thinking<W: Write>(&self, out: &mut W) -> io::Result<()> {
        if self.is_rich() {
            writeln!(
                out,
                "\n{} {}",
                self.paint(BOLD_RED, "kimetsu"),
                self.paint(
                    DIM,
                    "thinking; workspace tools may edit files and run commands..."
                )
            )
        } else {
            writeln!(
                out,
                "\n[kimetsu] thinking; workspace tools may edit files and run commands..."
            )
        }
    }

    pub fn write_assistant<W: Write>(&self, out: &mut W, text: &str) -> io::Result<()> {
        if self.is_rich() {
            writeln!(
                out,
                "\n{} {}",
                self.paint(BOLD_RED, "kimetsu"),
                self.paint(DIM, "dragon")
            )?;
            writeln!(
                out,
                "{}",
                self.paint(
                    DIM,
                    "------------------------------------------------------------"
                )
            )?;
            write!(out, "{}", text.trim_end())?;
            writeln!(out)?;
            writeln!(
                out,
                "{}",
                self.paint(
                    DIM,
                    "------------------------------------------------------------"
                )
            )?;
            Ok(())
        } else {
            writeln!(out, "\nassistant>")?;
            writeln!(out, "{text}")
        }
    }

    pub fn write_error<W: Write>(&self, out: &mut W, message: &str) -> io::Result<()> {
        if self.is_rich() {
            writeln!(out, "{} {}", self.paint(BOLD_RED, "[error]"), message)
        } else {
            writeln!(out, "[error] {message}")
        }
    }

    pub fn write_note<W: Write>(&self, out: &mut W, label: &str, message: &str) -> io::Result<()> {
        if self.is_rich() {
            writeln!(out, "{} {}", self.paint(BOLD_GREEN, label), message)
        } else {
            writeln!(out, "{label} {message}")
        }
    }

    pub fn write_turn_stats<W: Write>(&self, out: &mut W, stats: TurnStats) -> io::Result<()> {
        let label = if self.is_rich() {
            self.paint(BOLD_GREEN, "[turn]")
        } else {
            "[turn]".to_string()
        };
        writeln!(
            out,
            "\n{} cost=$${:.4}  total=$${:.4}/$${:.2}  turns={}  tool_calls={}",
            label, stats.turn_cost, stats.total_cost, stats.budget, stats.turns, stats.tool_calls,
        )
    }

    pub fn write_cache_stats<W: Write>(&self, out: &mut W, stats: CacheStats) -> io::Result<()> {
        let label = if self.is_rich() {
            self.paint(BOLD_YELLOW, "[cache]")
        } else {
            "[cache]".to_string()
        };
        writeln!(
            out,
            "{} input={} output={} cache_write={} cache_read={}",
            label, stats.input_tokens, stats.output_tokens, stats.cache_write, stats.cache_read,
        )
    }

    pub fn write_budget_stop<W: Write>(
        &self,
        out: &mut W,
        spent: f32,
        budget: f32,
    ) -> io::Result<()> {
        let label = if self.is_rich() {
            self.paint(BOLD_YELLOW, "[budget]")
        } else {
            "[budget]".to_string()
        };
        writeln!(
            out,
            "{} $${:.4} of $${:.2} spent - refusing further model calls. Raise --max-cost-usd or /quit.",
            label, spent, budget,
        )
    }

    pub fn clear_screen<W: Write>(&self, out: &mut W) -> io::Result<()> {
        if self.is_rich() {
            write!(out, "\x1b[2J\x1b[H")?;
        }
        Ok(())
    }

    fn write_kv<W: Write>(&self, out: &mut W, key: &str, value: &str) -> io::Result<()> {
        if self.is_rich() {
            writeln!(out, "{} {}", self.paint(DIM, &format!("{key:>9}:")), value)
        } else {
            writeln!(out, "{key}: {value}")
        }
    }

    fn paint(&self, code: &str, text: &str) -> String {
        if self.is_rich() {
            format!("{code}{text}{RESET}")
        } else {
            text.to_string()
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ChatBanner<'a> {
    pub version: &'a str,
    pub workspace: &'a Path,
    pub brain_project: Option<&'a Path>,
    pub model: &'a str,
    pub budget: f32,
    pub strict_verify: bool,
    pub goal: Option<&'a str>,
    pub skills_loaded: usize,
    pub skills_available: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct TurnStats {
    pub turn_cost: f32,
    pub total_cost: f32,
    pub budget: f32,
    pub turns: u32,
    pub tool_calls: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct CacheStats {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_write: u32,
    pub cache_read: u32,
}

pub fn rich_ui_enabled_from_env() -> bool {
    if env_truthy("KIMETSU_PLAIN") || std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    if std::env::var("TERM")
        .map(|term| term.eq_ignore_ascii_case("dumb"))
        .unwrap_or(false)
    {
        return false;
    }
    true
}

fn env_truthy(key: &str) -> bool {
    std::env::var(key)
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "" | "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

const RESET: &str = "\x1b[0m";
const DIM: &str = "\x1b[2m";
const BOLD_RED: &str = "\x1b[1;31m";
const BOLD_CYAN: &str = "\x1b[1;36m";
const BOLD_GREEN: &str = "\x1b[1;32m";
const BOLD_YELLOW: &str = "\x1b[1;33m";
const RED: &str = "\x1b[31m";

const DRAGON_LOGO: &[&str] = &[
    "        /\\_/\\",
    "   .---( o.o )---.",
    "  /  .-\\_^-_/- .  \\",
    "  \\_/   /___\\   \\_/",
    "        ^^ ^^",
];
