//! update, uninstall, checkpoint, resume, ps, stop, restart.
//! Split out of main.rs (v2.5.1); implementations only — the clap
//! surface stays in main.rs.

#![allow(unused_imports)]
use std::env;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use kimetsu_brain::project;
use kimetsu_core::KimetsuResult;
use kimetsu_core::memory::{MemoryKind, MemoryScope};

use crate::*;

pub(crate) fn update_cmd(args: UpdateArgs) -> KimetsuResult<()> {
    let flavor = update::UpdateFlavor::parse(&args.flavor)?;
    update::run(update::UpdateOptions {
        check: args.check,
        dry_run: args.dry_run,
        force: args.force,
        flavor,
    })
}

pub(crate) fn uninstall_cmd(args: UninstallArgs) -> KimetsuResult<()> {
    update::uninstall(update::UninstallOptions {
        dry_run: args.dry_run,
        yes: args.yes,
        keep_plugins: args.keep_plugins,
        delete_user_data: args.delete_user_data,
    })
}

// ── kimetsu checkpoint ────────────────────────────────────────────────────────

/// `kimetsu checkpoint [note]` — manually save a mid-session work episode.
pub(crate) fn checkpoint_cmd(args: CheckpointArgs) -> KimetsuResult<()> {
    let workspace = args
        .workspace
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());
    let note = args.note.as_deref().unwrap_or("");

    // Use capture_episode_now with an empty transcript (manual save does not
    // require a transcript — the note itself is sufficient context).
    let ok = distiller::capture_episode_now(&workspace, "", note);

    if ok {
        println!("[Kimetsu] Work checkpoint saved.");
        if !note.is_empty() {
            println!("  Note: {note}");
        }
    } else {
        // Could not write — likely no project initialised here.
        eprintln!(
            "[Kimetsu] Could not save checkpoint: no Kimetsu project found at {}.\n\
             Run `kimetsu init` to initialise one.",
            workspace.display()
        );
    }
    Ok(())
}

// ── kimetsu resume ────────────────────────────────────────────────────────────

/// `kimetsu resume` — print the last saved work episode.
pub(crate) fn resume_cmd(args: ResumeArgs) -> KimetsuResult<()> {
    let workspace = args
        .workspace
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());

    match kimetsu_brain::episode::load_live_episode_for_workspace(&workspace) {
        Ok(Some(ep)) => {
            println!("── Resume: last session ──────────────────────────────");
            if !ep.task.is_empty() {
                println!("Task:       {}", ep.task);
            }
            if !ep.summary.is_empty() {
                println!("Summary:    {}", ep.summary);
            }
            if !ep.open_threads.is_empty() {
                println!("Open:       {}", ep.open_threads.join("; "));
            }
            if !ep.dead_ends.is_empty() {
                println!("Avoid:      {}", ep.dead_ends.join("; "));
            }
            if !ep.hypothesis.is_empty() {
                println!("Hypothesis: {}", ep.hypothesis);
            }
            if !ep.note.is_empty() {
                println!("Note:       {}", ep.note);
            }
            println!("Saved:      {}", ep.created_at);
            println!("─────────────────────────────────────────────────────");
        }
        Ok(None) => {
            println!("[Kimetsu] No work episode saved for this repo yet.");
            println!("  Episodes are captured automatically at session end.");
            println!("  You can save one now with: kimetsu checkpoint");
        }
        Err(e) => {
            eprintln!("[Kimetsu] Could not load episode: {e}");
            eprintln!(
                "  Make sure a Kimetsu project is initialised at {}.",
                workspace.display()
            );
        }
    }
    Ok(())
}

// ── kimetsu ps ───────────────────────────────────────────────────────────────

pub(crate) fn ps_cmd(args: PsArgs) -> KimetsuResult<()> {
    let procs = process::list_kimetsu_processes();

    if args.json {
        println!("{}", serde_json::to_string_pretty(&procs)?);
        return Ok(());
    }

    if procs.is_empty() {
        println!("no running kimetsu processes");
        return Ok(());
    }

    // Human table: PID  KIND        WORKSPACE                        EXE
    println!("{:<8}  {:<12}  {:<40}  EXE", "PID", "KIND", "WORKSPACE");
    println!("{}", "-".repeat(100));
    for p in &procs {
        let kind = p.kind.label();
        let workspace = p.workspace.as_deref().unwrap_or("-");
        let exe = p.exe_path.as_deref().unwrap_or("-");
        println!("{:<8}  {:<12}  {:<40}  {}", p.pid, kind, workspace, exe);
    }
    Ok(())
}

// ── kimetsu stop ─────────────────────────────────────────────────────────────

pub(crate) fn stop_cmd(args: StopArgs) -> KimetsuResult<()> {
    let all_procs = process::list_kimetsu_processes();

    // Build the target set.
    let targets: Vec<&process::KimetsuProc> = if !args.pids.is_empty() && !args.all {
        // Explicit PIDs only.
        all_procs
            .iter()
            .filter(|p| args.pids.contains(&p.pid))
            .collect()
    } else {
        // --all, or no pids given — default to all.
        all_procs.iter().collect()
    };

    if targets.is_empty() {
        println!("no running kimetsu processes to stop");
        return Ok(());
    }

    // List what will be stopped.
    println!("The following kimetsu process(es) will be stopped:");
    for p in &targets {
        println!(
            "  PID {}  [{}]  workspace={}",
            p.pid,
            p.kind.label(),
            p.workspace.as_deref().unwrap_or("-")
        );
    }

    // Confirm unless --yes or non-TTY.
    if !args.yes && io::stdin().is_terminal() {
        print!("Stop these processes? [y/N] ");
        io::stdout().flush().ok();
        let stdin = io::stdin();
        let line = stdin.lock().lines().next();
        let answer = match line {
            Some(Ok(l)) => l.trim().to_lowercase(),
            _ => String::new(),
        };
        if answer != "y" && answer != "yes" {
            println!("Aborted.");
            return Ok(());
        }
    } else if !args.yes {
        // Non-TTY without --yes: refuse (same pattern as uninstall).
        return Err(
            "stdin is not a TTY; pass --yes to confirm stopping processes non-interactively".into(),
        );
    }

    let pids: Vec<u32> = targets.iter().map(|p| p.pid).collect();
    let results = process::stop_processes(&pids);

    let mut any_err = false;
    for (pid, result) in &results {
        match result {
            Ok(()) => println!("  stopped PID {pid}"),
            Err(e) => {
                eprintln!("  failed to stop PID {pid}: {e}");
                any_err = true;
            }
        }
    }

    // Hint: host-owned MCP servers are respawned automatically.
    let has_mcp = targets
        .iter()
        .any(|p| p.kind == process::ProcKind::McpServe);
    if has_mcp {
        println!(
            "hint: MCP servers spawned by a host (Claude Code, Codex) are respawned automatically \
             on the next tool call — no manual restart needed."
        );
    }

    if any_err {
        Err("one or more processes could not be stopped (see errors above)".into())
    } else {
        Ok(())
    }
}

// ── kimetsu restart ──────────────────────────────────────────────────────────

pub(crate) fn restart_cmd(args: RestartArgs) -> KimetsuResult<()> {
    // Target: all MCP-serve processes.
    let all_procs = process::list_kimetsu_processes();
    let mcp_procs: Vec<&process::KimetsuProc> = all_procs
        .iter()
        .filter(|p| p.kind == process::ProcKind::McpServe)
        .collect();

    if mcp_procs.is_empty() {
        println!("no running kimetsu MCP server processes found");
        println!(
            "hint: MCP servers are spawned by the host (Claude Code, Codex) on first use. \
             If you expected one, check `kimetsu ps` to see all kimetsu processes."
        );
        return Ok(());
    }

    println!("The following kimetsu MCP server(s) will be stopped:");
    for p in &mcp_procs {
        println!(
            "  PID {}  workspace={}",
            p.pid,
            p.workspace.as_deref().unwrap_or("-")
        );
    }

    if !args.yes && io::stdin().is_terminal() {
        print!("Stop and let the host respawn them? [y/N] ");
        io::stdout().flush().ok();
        let stdin = io::stdin();
        let line = stdin.lock().lines().next();
        let answer = match line {
            Some(Ok(l)) => l.trim().to_lowercase(),
            _ => String::new(),
        };
        if answer != "y" && answer != "yes" {
            println!("Aborted.");
            return Ok(());
        }
    } else if !args.yes {
        return Err(
            "stdin is not a TTY; pass --yes to confirm stopping processes non-interactively".into(),
        );
    }

    let pids: Vec<u32> = mcp_procs.iter().map(|p| p.pid).collect();
    let results = process::stop_processes(&pids);

    let mut any_err = false;
    for (pid, result) in &results {
        match result {
            Ok(()) => println!("  stopped PID {pid}"),
            Err(e) => {
                eprintln!("  failed to stop PID {pid}: {e}");
                any_err = true;
            }
        }
    }

    println!(
        "\nThe host agent (Claude Code / Codex) will automatically respawn the MCP server \
         on the next kimetsu tool call — no manual restart is needed."
    );

    if any_err {
        Err("one or more MCP server processes could not be stopped (see errors above)".into())
    } else {
        Ok(())
    }
}

// ── kimetsu setup — one-command onboarding ───────────────────────────────────

/// Resolve which host(s) to install into.
///
/// Priority:
/// 1. `--host` flag (explicit wins).
/// 2. Auto-detect from present home config dirs (`~/.claude`, `~/.codex`, `~/.pi`).
/// 3. None present + non-TTY → default `claude-code` with a note.
/// 4. None present + TTY → prompt with the provided `reader`.
///
/// Factored as a pure-ish function so it can be unit-tested without real installs.
#[allow(clippy::too_many_arguments)]
pub fn resolve_setup_hosts(
    arg: Option<&str>,
    present_claude: bool,
    present_codex: bool,
    present_cursor: bool,
    present_openclaw: bool,
    present_pi: bool,
    is_tty: bool,
    mut reader: impl io::BufRead,
) -> Result<Vec<kimetsu_chat::BridgeTarget>, String> {
    use kimetsu_chat::BridgeTarget;

    if let Some(raw) = arg {
        if raw.eq_ignore_ascii_case("both") {
            return Ok(vec![BridgeTarget::ClaudeCode, BridgeTarget::Codex]);
        }
        let target = BridgeTarget::parse(raw)?;
        return Ok(vec![target]);
    }

    // Auto-detect from present home dirs.
    let mut detected: Vec<BridgeTarget> = Vec::new();
    if present_claude {
        detected.push(BridgeTarget::ClaudeCode);
    }
    if present_codex {
        detected.push(BridgeTarget::Codex);
    }
    if present_cursor {
        detected.push(BridgeTarget::Cursor);
    }
    #[cfg(feature = "openclaw")]
    if present_openclaw {
        detected.push(BridgeTarget::OpenClaw);
    }
    #[cfg(not(feature = "openclaw"))]
    let _ = present_openclaw;
    #[cfg(feature = "pi")]
    if present_pi {
        detected.push(BridgeTarget::Pi);
    }
    #[cfg(not(feature = "pi"))]
    let _ = present_pi;

    if !detected.is_empty() {
        return Ok(detected);
    }

    // Nothing detected.
    if !is_tty {
        eprintln!(
            "note: no recognized host config dirs found; defaulting to claude-code. \
             Pass --host to choose explicitly."
        );
        Ok(vec![BridgeTarget::ClaudeCode])
    } else {
        #[cfg(all(feature = "pi", feature = "openclaw"))]
        let prompt = "Which host agent do you use? [claude-code/codex/cursor/openclaw/pi/both]: ";
        #[cfg(all(feature = "pi", not(feature = "openclaw")))]
        let prompt = "Which host agent do you use? [claude-code/codex/cursor/pi/both]: ";
        #[cfg(all(not(feature = "pi"), feature = "openclaw"))]
        let prompt = "Which host agent do you use? [claude-code/codex/cursor/openclaw/both]: ";
        #[cfg(all(not(feature = "pi"), not(feature = "openclaw")))]
        let prompt = "Which host agent do you use? [claude-code/codex/cursor/both]: ";
        print!("{prompt}");
        io::stdout().flush().ok();
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .map_err(|e| format!("setup: failed to read host selection: {e}"))?;
        let answer = line.trim().to_ascii_lowercase();
        if answer.is_empty() || answer == "claude-code" || answer == "claude" || answer == "cc" {
            Ok(vec![BridgeTarget::ClaudeCode])
        } else if answer == "codex" {
            Ok(vec![BridgeTarget::Codex])
        } else if answer == "both" {
            Ok(vec![BridgeTarget::ClaudeCode, BridgeTarget::Codex])
        } else {
            BridgeTarget::parse(&answer).map(|t| vec![t])
        }
    }
}
