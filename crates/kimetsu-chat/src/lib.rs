#![recursion_limit = "256"]

//! Interactive REPL transport for kimetsu.
//!
//! v0.3 — `kimetsu chat`: turn the agent runtime that's been validated
//! on Terminal-Bench into a real interactive coding assistant. Goals:
//!
//!   1. Zero dependence on Terminal-Bench / Harbor — chat is its own
//!      product, not a benchmark subset.
//!   2. Reuse the entire 20-tool surface, prompts, brain integration,
//!      providers (`claude_code` today; `anthropic` natively next), and
//!      MP-18's iterative goal verify with no per-feature porting.
//!   3. Tool runtime swaps to host-side `LocalShellExecutor` — commands
//!      execute against the user's actual filesystem.
//!   4. Per-session brain context: one project = one brain pool, so
//!      failure_pattern memories from MP-18 record_deviation calls
//!      accumulate over time and surface on later sessions.
//!   5. Slash commands for human-driven UX: `/goal`, `/plan`, `/verify`,
//!      `/cost`, `/memory`, `/quit`, `/help`.
//!
//! Scaffold state (v0.3.0-alpha): public API surface is declared,
//! REPL loop is a minimal echo placeholder. Implementation lands in
//! subsequent commits as the v0.3 sprint progresses.

pub mod bridge;
pub mod commands;
pub mod cost;
pub mod mcp_server;
pub mod repl;
pub mod skills;
pub mod ui;

pub use bridge::{
    BridgeTarget, PluginMode, bridge_export_skill, bridge_import_skill, bridge_scan, bridge_sync,
    plugin_install,
};
pub use commands::SlashCommand;
pub use cost::CostMeter;
pub use mcp_server::{McpServeConfig, serve_mcp};
pub use repl::{ChatConfig, ChatError, ChatResult, run_repl};
pub use skills::{SkillConfig, SkillRegistry, skill_origin_label};
pub use ui::{ChatUi, ChatUiMode, rich_ui_enabled_from_env};

/// Version of the chat transport. Bumped on protocol-affecting
/// changes (e.g. when we add session resume / persistent transcripts).
pub const CHAT_PROTOCOL_VERSION: &str = "0.1";
