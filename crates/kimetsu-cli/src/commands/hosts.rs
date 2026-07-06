//! setup wizard, doctor, host detection, init.
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

pub(crate) fn detect_present_hosts() -> (bool, bool, bool, bool, bool) {
    let home = std::env::var_os("USERPROFILE")
        .filter(|v| !v.is_empty())
        .or_else(|| std::env::var_os("HOME").filter(|v| !v.is_empty()))
        .map(std::path::PathBuf::from);

    let home = match home {
        Some(h) => h,
        None => return (false, false, false, false, false),
    };

    let claude_present = home.join(".claude").is_dir();
    let codex_present = home.join(".codex").is_dir();
    // Cursor: global config lives in ~/.cursor
    let cursor_present = home.join(".cursor").is_dir();
    #[cfg(feature = "openclaw")]
    let openclaw_present = home.join(".openclaw").is_dir();
    #[cfg(not(feature = "openclaw"))]
    let openclaw_present = false;
    #[cfg(feature = "pi")]
    let pi_present = home.join(".pi").is_dir();
    #[cfg(not(feature = "pi"))]
    let pi_present = false;
    (
        claude_present,
        codex_present,
        cursor_present,
        openclaw_present,
        pi_present,
    )
}

/// `kimetsu setup` — one-command onboarding.
pub(crate) fn setup_cmd(args: SetupArgs) -> KimetsuResult<()> {
    use kimetsu_chat::{BridgeTarget, InstallScope, PluginMode, plugin_install};

    let workspace = args
        .workspace
        .canonicalize()
        .unwrap_or_else(|_| args.workspace.clone());

    println!("=== kimetsu setup ===");
    println!(
        "workspace: {}",
        kimetsu_core::paths::display_path(&workspace)
    );
    println!();

    // ── Step 1: Init ──────────────────────────────────────────────────────────
    println!("[1/4] Initializing project...");
    let init_result = project::init_project(&workspace, false);
    let init_ok = match init_result {
        Ok(ref summary) => {
            if summary.wrote_project_toml {
                println!(
                    "  initialized .kimetsu/ at {}",
                    kimetsu_core::paths::display_path(&summary.kimetsu_dir)
                );
            } else {
                println!(
                    "  project already initialized at {}",
                    kimetsu_core::paths::display_path(&summary.kimetsu_dir)
                );
            }
            true
        }
        Err(ref e) => {
            eprintln!("  error: init failed: {e}");
            eprintln!("  cannot continue without a valid project. Fix the error and re-run setup.");
            // Print summary of what succeeded (nothing) before bailing.
            println!();
            println!("=== setup summary ===");
            println!("  init:    FAILED — {e}");
            println!("  install: skipped");
            println!("  verify:  skipped");
            return Err(format!("kimetsu setup: init failed: {e}").into());
        }
    };
    let _ = init_ok;

    // ── Step 2: Choose host(s) ────────────────────────────────────────────────
    println!();
    println!("[2/4] Selecting host(s)...");
    let (present_claude, present_codex, present_cursor, present_openclaw, present_pi) =
        detect_present_hosts();
    let is_tty = io::stdin().is_terminal();
    let stdin = io::stdin();
    let hosts = resolve_setup_hosts(
        args.host.as_deref(),
        present_claude,
        present_codex,
        present_cursor,
        present_openclaw,
        present_pi,
        is_tty,
        stdin.lock(),
    )
    .map_err(|e| format!("kimetsu setup: {e}"))?;

    let scope = InstallScope::parse(&args.scope).map_err(|e| format!("kimetsu setup: {e}"))?;
    let mode = PluginMode::parse(&args.mode).map_err(|e| format!("kimetsu setup: {e}"))?;

    let host_labels: Vec<&str> = hosts.iter().map(|h| h.as_str()).collect();
    let scope_gloss = match scope {
        InstallScope::Workspace => "this project only",
        InstallScope::Global => "every project",
    };
    let mode_gloss = match mode {
        PluginMode::Optional => "recommended, non-blocking",
        PluginMode::Required => "treated as a setup blocker for big tasks",
    };
    println!(
        "  hosts: {}   scope: {} ({})   mode: {} ({})",
        host_labels.join(", "),
        scope.as_str(),
        scope_gloss,
        mode.as_str(),
        mode_gloss,
    );

    // ── Step 3: Install ───────────────────────────────────────────────────────
    println!();
    println!("[3/4] Installing plugin wiring...");

    let mut install_warnings: Vec<String> = Vec::new();
    let mut install_failed = false;
    let mut installed_hosts: Vec<String> = Vec::new();

    for &target in &hosts {
        let host_label = match target {
            BridgeTarget::ClaudeCode => "Claude Code",
            BridgeTarget::Codex => "Codex",
            BridgeTarget::Kimetsu => "Kimetsu",
            BridgeTarget::Cursor => "Cursor",
            #[cfg(feature = "openclaw")]
            BridgeTarget::OpenClaw => "OpenClaw",
            #[cfg(feature = "pi")]
            BridgeTarget::Pi => "Pi",
        };
        println!(
            "  installing into {host_label} ({} scope)...",
            scope.as_str()
        );

        match plugin_install(
            &workspace,
            target,
            scope,
            mode,
            false, // force — idempotent
            !args.no_proactive,
        ) {
            Ok(report) => {
                for f in &report.files {
                    let rel = f
                        .strip_prefix(&workspace)
                        .map(|r| r.to_string_lossy().replace('\\', "/"))
                        .unwrap_or_else(|_| kimetsu_core::paths::display_path(f));
                    println!("    {rel}");
                }
                for note in &report.notes {
                    println!("    {note}");
                }
                installed_hosts.push(format!("{} ({})", host_label, scope.as_str()));

                // Run the distiller setup wizard unless suppressed.
                if matches!(target, BridgeTarget::ClaudeCode | BridgeTarget::Codex)
                    && !args.no_setup
                    && is_tty
                    && io::stdout().is_terminal()
                {
                    let target_for_scope = match scope {
                        InstallScope::Global => {
                            kimetsu_core::paths::user_kimetsu_dir().map(|dir| {
                                (
                                    harvest_setup::SetupTarget {
                                        project_toml: dir.join("project.toml"),
                                        env_path: dir.join(".env"),
                                        gitignore_dir: dir,
                                    },
                                    "globally (all projects, ~/.kimetsu)",
                                )
                            })
                        }
                        InstallScope::Workspace => {
                            let p = kimetsu_core::paths::ProjectPaths::at_root(&workspace);
                            Some((
                                harvest_setup::SetupTarget {
                                    project_toml: p.project_toml.clone(),
                                    env_path: p.repo_root.join(".env"),
                                    gitignore_dir: p.repo_root.clone(),
                                },
                                "this workspace",
                            ))
                        }
                    };
                    if let Some((setup_target, label)) = target_for_scope {
                        let stdin2 = std::io::stdin();
                        let mut reader2 = stdin2.lock();
                        let mut stdout2 = std::io::stdout();
                        if let Err(e) = harvest_setup::run_harvest_setup(
                            &mut reader2,
                            &mut stdout2,
                            &setup_target,
                            label,
                        ) {
                            eprintln!("  distiller setup skipped: {e}");
                        }
                    }
                }

                // Self-check: confirm wiring landed.
                if matches!(target, BridgeTarget::ClaudeCode | BridgeTarget::Codex) {
                    let warnings =
                        plugin_install_self_check(&workspace, target.as_str(), scope.as_str());
                    install_warnings.extend(warnings);
                }
            }
            Err(e) => {
                eprintln!("  error: install into {host_label} failed: {e}");
                install_failed = true;
            }
        }
    }

    if install_failed {
        // Core step failed — return non-zero.
        println!();
        println!("=== setup summary ===");
        println!("  init:    OK");
        if installed_hosts.is_empty() {
            println!("  install: FAILED (all hosts)");
        } else {
            println!(
                "  install: partial — succeeded: {}",
                installed_hosts.join(", ")
            );
        }
        println!("  verify:  skipped");
        return Err("kimetsu setup: one or more plugin installs failed (see errors above)".into());
    }

    // ── Step 4: Verify (selftest) ─────────────────────────────────────────────
    println!();
    println!("[4/4] Verifying brain (doctor --selftest)...");
    let selftest_ok = if args.no_selftest {
        println!("  skipped (--no-selftest)");
        true
    } else {
        match doctor::run_selftest() {
            Ok(()) => true,
            Err(e) => {
                eprintln!("  selftest FAILED: {e}");
                false
            }
        }
    };

    // ── Summary ───────────────────────────────────────────────────────────────
    println!();
    println!("=== setup summary ===");
    println!("  init:    OK");
    println!("  install: {}", installed_hosts.join(", "));
    if args.no_selftest {
        println!("  verify:  skipped");
    } else if selftest_ok {
        println!("  verify:  ✓ PASS");
    } else {
        println!("  verify:  ✗ FAIL (brain not working — check logs above)");
    }

    // Surface PATH warnings prominently if present.
    let path_warnings: Vec<&String> = install_warnings
        .iter()
        .filter(|w| w.contains("PATH"))
        .collect();
    if !path_warnings.is_empty() {
        println!();
        println!("IMPORTANT — kimetsu not on PATH:");
        for w in &path_warnings {
            println!("  {w}");
        }
    }

    println!();
    let host_names: Vec<&str> = hosts
        .iter()
        .map(|t| match t {
            BridgeTarget::ClaudeCode => "Claude Code",
            BridgeTarget::Codex => "Codex",
            BridgeTarget::Kimetsu => "Kimetsu",
            BridgeTarget::Cursor => "Cursor",
            #[cfg(feature = "openclaw")]
            BridgeTarget::OpenClaw => "OpenClaw",
            #[cfg(feature = "pi")]
            BridgeTarget::Pi => "Pi",
        })
        .collect();
    println!(
        "Next step: Restart your host agent ({}) so it loads the Kimetsu MCP server.",
        host_names.join(" / ")
    );

    Ok(())
}

/// v0.4.6: `kimetsu doctor` entry point. Runs the full health
/// suite + prints either the human or JSON report.
///
/// Exit codes:
///   0 — all checks passed or warned.
///   1 — at least one Fail.
///   2 — internal doctor error (couldn't even run the checks).
pub(crate) fn doctor_cmd(args: DoctorArgs) -> KimetsuResult<()> {
    // D4: --selftest runs a hermetic round-trip and exits early.
    if args.selftest {
        return doctor::run_selftest();
    }
    let opts = doctor::DoctorOptions {
        json: args.json,
        skip_mcp: args.skip_mcp,
    };
    let workspace = match args.workspace.canonicalize() {
        Ok(p) => p,
        Err(_) => args.workspace.clone(),
    };
    let report = doctor::run(&workspace, opts.clone())?;
    if opts.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        doctor::print_human(&report);
    }
    if !report.ok() {
        std::process::exit(1);
    }
    Ok(())
}

pub(crate) fn init(args: InitArgs) -> KimetsuResult<()> {
    let cwd = env::current_dir()?;
    let summary = project::init_project(&cwd, args.force)?;

    // Friendly summary — use display_path to strip \\?\ on Windows.
    let pretty_root = kimetsu_core::paths::display_path(&summary.repo_root);
    println!("✓ Initialized Kimetsu in {pretty_root}");

    // Show inner files workspace-relative.
    println!("    brain:  .kimetsu/brain.db  ({} memories)", {
        project::list_memories(&summary.repo_root)
            .map(|m| m.len())
            .unwrap_or(0)
    });
    println!("    config: .kimetsu/project.toml");
    println!("    model:  {}", summary.model);

    if !summary.api_key_present {
        println!(
            "    note: {} isn't set — needed only for model-backed commands \
             (`kimetsu run`, `kimetsu chat`), not for the brain.",
            summary.api_key_env
        );
    }

    println!();
    println!("Next — wire a host agent so it uses the brain:");
    println!("    kimetsu plugin install claude-code      (also: codex, pi, openclaw)");
    println!(
        "    kimetsu setup                           (init + install + health check, in one step)"
    );

    Ok(())
}
