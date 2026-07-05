//! bridge, mcp serve, plugin install.
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

pub(crate) fn bridge(command: BridgeCommand) -> KimetsuResult<()> {
    use kimetsu_chat::{
        BridgeTarget, bridge_export_skill, bridge_import_skill, bridge_scan, bridge_sync,
    };

    match command {
        BridgeCommand::Scan(args) | BridgeCommand::Status(args) | BridgeCommand::Doctor(args) => {
            let workspace = args.workspace.canonicalize()?;
            let config = bridge_skill_config(args.no_user_skills);
            let scan = bridge_scan(&workspace, &config)
                .map_err(|err| format!("kimetsu bridge scan: {err}"))?;
            println!("workspace: {}", workspace.display());
            println!("extensions: {}", scan.extensions.len());
            for extension in &scan.extensions {
                println!(
                    "  {} [{}] {}",
                    extension.manifest.name,
                    extension.manifest.source,
                    extension.root.display()
                );
            }
            println!("skills: {}", scan.skills.len());
            for skill in &scan.skills {
                println!(
                    "  {}  kimetsu_ext={} kimetsu={} claude={} codex={}  origin={}",
                    skill.name,
                    skill.kimetsu_extension,
                    skill.kimetsu_skill,
                    skill.claude_skill,
                    skill.codex_skill,
                    skill.origin
                );
            }
            if scan.skills.is_empty() {
                println!(
                    "no skills found; add provider skills or run `kimetsu plugin install <target>`"
                );
            }
        }
        BridgeCommand::Import(args) => {
            let workspace = args.workspace.canonicalize()?;
            let config = bridge_skill_config(args.no_user_skills);
            let imported = bridge_import_skill(&workspace, &config, &args.selection, args.force)
                .map_err(|err| format!("kimetsu bridge import: {err}"))?;
            println!(
                "imported {} into {}",
                imported.manifest.name,
                imported.root.display()
            );
        }
        BridgeCommand::Export(args) => {
            let workspace = args.workspace.canonicalize()?;
            let config = bridge_skill_config(args.no_user_skills);
            let target = BridgeTarget::parse(&args.target)
                .map_err(|err| format!("kimetsu bridge export: {err}"))?;
            let exported =
                bridge_export_skill(&workspace, &config, &args.selection, target, args.force)
                    .map_err(|err| format!("kimetsu bridge export: {err}"))?;
            println!(
                "exported {} to {} at {}",
                args.selection,
                target.as_str(),
                exported.display()
            );
        }
        BridgeCommand::Sync(args) => {
            let workspace = args.workspace.canonicalize()?;
            let config = bridge_skill_config(args.no_user_skills);
            let imported = bridge_sync(&workspace, &config, args.force)
                .map_err(|err| format!("kimetsu bridge sync: {err}"))?;
            println!("imported {imported} skill bundle(s) into .kimetsu/extensions");
        }
    }
    Ok(())
}

pub(crate) fn mcp(command: McpCommand) -> KimetsuResult<()> {
    use kimetsu_chat::{McpServeConfig, serve_mcp};

    match command {
        McpCommand::Serve(args) => {
            let mut config = McpServeConfig::new(args.workspace);
            config.skills.include_user_roots = !args.no_user_skills;
            let stdin = io::stdin();
            let stdout = io::stdout();
            serve_mcp(stdin.lock(), stdout.lock(), config)
                .map_err(|err| format!("kimetsu mcp serve: {err}"))?;
        }
    }
    Ok(())
}

// ── plugin install self-check ────────────────────────────────────────────────

/// Check whether the `kimetsu` binary is resolvable on the current PATH.
///
/// Returns `true` when any entry in `PATH` contains a file named `kimetsu`
/// (or `kimetsu.exe` on Windows). Factored out for unit-testability.
pub fn kimetsu_on_path() -> bool {
    kimetsu_on_path_with(std::env::var_os("PATH").as_deref())
}

/// Inner implementation; takes an optional raw PATH value so tests can
/// inject a controlled PATH without touching the real environment.
pub fn kimetsu_on_path_with(path_var: Option<&std::ffi::OsStr>) -> bool {
    let Some(path_var) = path_var else {
        return false;
    };
    let bin = if cfg!(windows) {
        "kimetsu.exe"
    } else {
        "kimetsu"
    };
    std::env::split_paths(path_var).any(|dir| dir.join(bin).is_file())
}

/// Best-effort post-install self-check.
///
/// 1. Confirms `kimetsu` resolves on PATH.
/// 2. Calls `plugin_status` and verifies the just-installed (host, scope)
///    reports `WiringState::Installed`.
/// 3. Prints a concise summary + the "restart your host" next-step message.
///
/// A failed check prints a warning but does NOT cause the install to fail
/// (the files were already written).  Returns the list of warning strings
/// so tests can assert on the output without capturing stdout.
pub fn plugin_install_self_check(
    workspace: &std::path::Path,
    host: &str,
    scope: &str,
) -> Vec<String> {
    use kimetsu_chat::{WiringState, plugin_status};

    let mut warnings: Vec<String> = Vec::new();

    // 1. PATH check.
    if !kimetsu_on_path() {
        warnings.push(
            "warning: `kimetsu` is not on your PATH — the installed hooks call the bare \
             `kimetsu` command, but it won't be found. Add the install directory \
             (e.g. `~/.cargo/bin`) to your PATH so the hooks can run."
                .to_string(),
        );
    }

    // 2. Wiring check via plugin_status.
    let statuses = plugin_status(workspace);
    let entry = statuses.iter().find(|s| s.host == host && s.scope == scope);

    match entry {
        Some(s) if matches!(s.state, WiringState::Installed) => {
            // All good — success line.
            let host_label = match host {
                "claude-code" => "Claude Code",
                "codex" => "Codex",
                other => other,
            };
            println!(
                "✓ wired into {host_label} ({scope} scope). \
                 Restart your host agent ({host_label}) so it picks up the MCP server."
            );
        }
        Some(s) if matches!(s.state, WiringState::Partial) => {
            let warn = format!(
                "warning: wiring is partial for {} ({}). Missing pieces: [{}]. \
                 Re-run `kimetsu plugin install {}` to complete it.",
                host,
                scope,
                s.missing.join(", "),
                host
            );
            warnings.push(warn.clone());
            eprintln!("{warn}");
        }
        Some(_) | None => {
            let warn = format!(
                "warning: could not confirm wiring landed for {host} ({scope}). \
                 Run `kimetsu plugin status` to inspect."
            );
            warnings.push(warn.clone());
            eprintln!("{warn}");
        }
    }

    // Emit any PATH warnings to stderr.
    for w in &warnings {
        if w.contains("PATH") {
            eprintln!("{w}");
        }
    }

    warnings
}

/// Normalize a git remote URL (or an explicit `--repo`) into a stable,
/// server-safe id: drop scheme/credentials/`.git`, then slug to
/// `[a-z0-9-]`. `https://github.com/org/repo.git` and
/// `git@github.com:org/repo.git` both → `github-com-org-repo`.
pub(crate) fn normalize_repo_id(raw: &str) -> String {
    let mut s = raw.trim();
    if let Some(stripped) = s.strip_suffix(".git") {
        s = stripped;
    }
    if let Some((_, rest)) = s.split_once("://") {
        s = rest;
    }
    if let Some((_, rest)) = s.split_once('@') {
        s = rest;
    }
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

/// Derive a repo id from `git -C <workspace> remote get-url origin`.
pub(crate) fn derive_repo_id(workspace: &std::path::Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let id = normalize_repo_id(String::from_utf8_lossy(&out.stdout).trim());
    (!id.is_empty()).then_some(id)
}

/// Wire a host to a remote kimetsu-remote server (HTTP MCP).
pub(crate) fn run_plugin_install_remote(
    workspace: &std::path::Path,
    target: kimetsu_chat::BridgeTarget,
    scope: kimetsu_chat::InstallScope,
    mode: kimetsu_chat::PluginMode,
    args: &PluginInstallArgs,
    base: &str,
) -> KimetsuResult<()> {
    let repo_id = match &args.repo {
        Some(r) => normalize_repo_id(r),
        None => derive_repo_id(workspace).ok_or_else(|| {
            "kimetsu plugin install: could not derive a repo id from this repo's git remote; \
             pass --repo <id>"
                .to_string()
        })?,
    };
    if repo_id.is_empty() {
        return Err("kimetsu plugin install: --repo resolved to an empty id".into());
    }
    let remote = kimetsu_chat::RemoteInstall {
        base_url: base.to_string(),
        repo_id: repo_id.clone(),
        token: args.token.clone(),
    };
    let report = kimetsu_chat::plugin_install_remote(workspace, target, scope, mode, &remote)
        .map_err(|err| format!("kimetsu plugin install: {err}"))?;

    let host_label = match target {
        kimetsu_chat::BridgeTarget::ClaudeCode => "Claude Code",
        #[cfg(feature = "openclaw")]
        kimetsu_chat::BridgeTarget::OpenClaw => "OpenClaw",
        _ => "host",
    };
    println!(
        "Wiring Kimetsu (remote) into {host_label} ({} scope) → repo `{repo_id}`…",
        report.scope.as_str()
    );
    println!("  wrote/updated:");
    for file in &report.files {
        let rel = file
            .strip_prefix(workspace)
            .map(|r| r.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| kimetsu_core::paths::display_path(file));
        println!("    {rel}");
    }
    for note in &report.notes {
        println!("  {note}");
    }
    println!("  ✓ wired. Restart your host agent so it connects to the remote brain.");
    println!(
        "  note: Kimetsu Remote is BETA (under active testing — expect rough edges). The \
         `kimetsu-remote` server is a SEPARATE binary: `cargo install kimetsu-remote --features \
         embeddings` (or the embeddings release archive) — it is not installed with `kimetsu`."
    );
    Ok(())
}

pub(crate) fn plugin(command: PluginCommand) -> KimetsuResult<()> {
    use kimetsu_chat::{
        BridgeTarget, InstallScope, PluginMode, WiringState, plugin_install, plugin_status,
        plugin_uninstall,
    };

    match command {
        PluginCommand::Install(args) => {
            // Canonicalize leniently: a global install doesn't use the
            // workspace, so a missing `--workspace` path shouldn't fail it.
            let workspace = args
                .workspace
                .canonicalize()
                .unwrap_or_else(|_| args.workspace.clone());
            let target = BridgeTarget::parse(&args.target)
                .map_err(|err| format!("kimetsu plugin install: {err}"))?;
            let scope = InstallScope::parse(&args.scope)
                .map_err(|err| format!("kimetsu plugin install: {err}"))?;
            let mode = PluginMode::parse(&args.mode)
                .map_err(|err| format!("kimetsu plugin install: {err}"))?;
            // Remote wiring: point the host at a kimetsu-remote HTTP MCP server.
            if let Some(base) = args.remote.clone() {
                return run_plugin_install_remote(&workspace, target, scope, mode, &args, &base);
            }
            // The kimetsu extensions target is workspace-only; warn rather
            // than silently ignore a `--scope global` for it.
            if matches!(scope, InstallScope::Global) && matches!(target, BridgeTarget::Kimetsu) {
                eprintln!(
                    "kimetsu plugin install: --scope global has no effect for the `kimetsu` target; \
                     installing to the workspace .kimetsu/extensions."
                );
            }
            let report = plugin_install(
                &workspace,
                target,
                scope,
                mode,
                args.force,
                !args.no_proactive,
            )
            .map_err(|err| format!("kimetsu plugin install: {err}"))?;

            // Friendly framing: intro line with plain-language scope/mode glosses.
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
            let scope_gloss = match scope {
                InstallScope::Workspace => "this project only",
                InstallScope::Global => "every project",
            };
            let mode_gloss = match mode {
                PluginMode::Optional => "recommended, non-blocking",
                PluginMode::Required => "treated as a setup blocker for big tasks",
            };
            println!(
                "Wiring Kimetsu into {host_label} ({} scope — {scope_gloss}, {} mode — {mode_gloss})…",
                report.scope.as_str(),
                report.mode.as_str(),
            );
            println!("  wrote/updated:");
            for file in &report.files {
                // Show workspace-relative path when possible; fall back to display_path.
                let rel = file
                    .strip_prefix(&workspace)
                    .map(|r| r.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_else(|_| kimetsu_core::paths::display_path(file));
                println!("    {rel}");
            }
            for note in &report.notes {
                println!("  {note}");
            }
            // Offer interactive distiller setup for host targets on a TTY.
            let interactive = args.setup_harvest
                || (std::io::stdin().is_terminal() && std::io::stdout().is_terminal());
            if matches!(target, BridgeTarget::ClaudeCode | BridgeTarget::Codex)
                && !args.no_setup
                && interactive
            {
                let target_for_scope = match scope {
                    InstallScope::Global => match kimetsu_core::paths::user_kimetsu_dir() {
                        Some(dir) => Some((
                            harvest_setup::SetupTarget {
                                project_toml: dir.join("project.toml"),
                                env_path: dir.join(".env"),
                                gitignore_dir: dir,
                            },
                            "globally (all projects, ~/.kimetsu)",
                        )),
                        None => {
                            eprintln!(
                                "kimetsu plugin install: cannot resolve ~/.kimetsu; skipping distiller setup."
                            );
                            None
                        }
                    },
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
                    let stdin = std::io::stdin();
                    let mut reader = stdin.lock();
                    let mut stdout = std::io::stdout();
                    if let Err(err) = harvest_setup::run_harvest_setup(
                        &mut reader,
                        &mut stdout,
                        &setup_target,
                        label,
                    ) {
                        eprintln!("kimetsu plugin install: distiller setup skipped: {err}");
                    }
                }
            }
            // Self-check: confirm wiring landed + PATH hint.
            // Only for host targets; the `kimetsu` extensions target
            // doesn't invoke the bare `kimetsu` command.
            if matches!(target, BridgeTarget::ClaudeCode | BridgeTarget::Codex) {
                plugin_install_self_check(&workspace, target.as_str(), scope.as_str());
            }
        }

        PluginCommand::Status(args) => {
            let workspace = args
                .workspace
                .canonicalize()
                .unwrap_or_else(|_| args.workspace.clone());

            let statuses = plugin_status(&workspace);

            // Collect running MCP servers.
            let mcp_procs: Vec<_> = process::list_kimetsu_processes()
                .into_iter()
                .filter(|p| p.kind == process::ProcKind::McpServe)
                .collect();

            // Determine the on-PATH kimetsu version.
            let path_version = kimetsu_version_on_path();
            let this_version = env!("CARGO_PKG_VERSION");

            if args.json {
                #[derive(serde::Serialize)]
                struct StatusOutput<'a> {
                    wiring: &'a Vec<kimetsu_chat::PluginScopeStatus>,
                    this_binary_version: &'a str,
                    path_version: Option<String>,
                    mcp_servers: Vec<MiniProc>,
                }
                #[derive(serde::Serialize)]
                struct MiniProc {
                    pid: u32,
                    workspace: Option<String>,
                    exe_path: Option<String>,
                }
                let output = StatusOutput {
                    wiring: &statuses,
                    this_binary_version: this_version,
                    path_version,
                    mcp_servers: mcp_procs
                        .iter()
                        .map(|p| MiniProc {
                            pid: p.pid,
                            workspace: p.workspace.clone(),
                            exe_path: p.exe_path.clone(),
                        })
                        .collect(),
                };
                println!("{}", serde_json::to_string_pretty(&output)?);
                return Ok(());
            }

            // Human-readable report.
            let any_wired = statuses
                .iter()
                .any(|s| !matches!(s.state, WiringState::Absent));

            if !any_wired {
                println!(
                    "Kimetsu is not installed into any host (workspace or global).\n\
                     Run `kimetsu plugin install <claude-code|codex>` to wire it in."
                );
                return Ok(());
            }

            println!("Kimetsu plugin wiring status");
            println!("{}", "─".repeat(60));

            for s in &statuses {
                let state_label = match s.state {
                    WiringState::Installed => "INSTALLED",
                    WiringState::Partial => "PARTIAL  ",
                    WiringState::Absent => "absent   ",
                };
                let present_str = if s.present.is_empty() {
                    String::new()
                } else {
                    format!("  present: [{}]", s.present.join(", "))
                };
                let missing_str = if s.missing.is_empty() {
                    String::new()
                } else {
                    format!("  missing: [{}]", s.missing.join(", "))
                };
                println!(
                    "  {:<12}  {:<10}  {}{}{}",
                    s.host, s.scope, state_label, present_str, missing_str
                );
                if !matches!(s.state, WiringState::Absent) {
                    // Strip \\?\ prefix that canonicalize() can add on Windows.
                    let cfg_display =
                        kimetsu_core::paths::display_path(std::path::Path::new(&s.config_path));
                    println!("    config: {cfg_display}");
                }
            }

            println!("{}", "─".repeat(60));
            println!("This binary:  v{this_version}");
            match &path_version {
                Some(pv) if pv != this_version => {
                    println!("On PATH:      v{pv}  (differs from this binary)");
                }
                Some(pv) => println!("On PATH:      v{pv}"),
                None => println!("On PATH:      (could not determine)"),
            }

            if mcp_procs.is_empty() {
                println!("MCP servers:  none running");
            } else {
                println!("MCP servers:");
                for p in &mcp_procs {
                    println!(
                        "  PID {}  workspace={}",
                        p.pid,
                        p.workspace.as_deref().unwrap_or("-")
                    );
                }
            }
        }

        PluginCommand::Uninstall(args) => {
            let workspace = args
                .workspace
                .canonicalize()
                .unwrap_or_else(|_| args.workspace.clone());

            let target = BridgeTarget::parse(&args.target)
                .map_err(|err| format!("kimetsu plugin uninstall: {err}"))?;

            // Collect scopes to uninstall from.
            let scopes: Vec<InstallScope> = if args.all_scopes {
                vec![InstallScope::Workspace, InstallScope::Global]
            } else {
                let scope = InstallScope::parse(&args.scope)
                    .map_err(|err| format!("kimetsu plugin uninstall: {err}"))?;
                vec![scope]
            };

            // Show current status for the target+scopes and confirm.
            let all_statuses = plugin_status(&workspace);
            let relevant: Vec<_> = all_statuses
                .iter()
                .filter(|s| {
                    s.host == target.as_str()
                        && scopes.iter().any(|sc| sc.as_str() == s.scope.as_str())
                })
                .collect();

            let anything_present = relevant
                .iter()
                .any(|s| !matches!(s.state, WiringState::Absent));

            if !anything_present {
                println!(
                    "No Kimetsu wiring found for {} ({}) — nothing to remove.",
                    target.as_str(),
                    scopes
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join("+")
                );
                return Ok(());
            }

            // Show what will be removed.
            for s in &relevant {
                if !matches!(s.state, WiringState::Absent) {
                    println!(
                        "Will remove Kimetsu wiring from {} ({}): [{}]",
                        s.host,
                        s.scope,
                        s.present.join(", ")
                    );
                }
            }
            println!(
                "\nThis removes ONLY the host wiring — the Kimetsu binary, brain, and your \
                 other hooks/servers are NOT touched."
            );

            // Interactive confirm.
            let scope_label = scopes
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(" + ");
            if !args.yes && io::stdin().is_terminal() {
                print!(
                    "Remove Kimetsu's wiring from {} ({})? [y/N] ",
                    target.as_str(),
                    scope_label
                );
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
                return Err("stdin is not a TTY; pass --yes to confirm non-interactively".into());
            }

            // Execute uninstall for each scope.
            for scope in &scopes {
                let report = plugin_uninstall(&workspace, target, *scope)
                    .map_err(|err| format!("kimetsu plugin uninstall: {err}"))?;

                if report.removed.is_empty() && report.modified.is_empty() {
                    println!(
                        "  {} scope: nothing to remove (already clean)",
                        scope.as_str()
                    );
                } else {
                    for path in &report.removed {
                        println!("  removed  {}", path.display());
                    }
                    for path in &report.modified {
                        println!("  modified {}", path.display());
                    }
                }
            }

            println!(
                "\nKimetsu plugin wiring removed from {} ({}).",
                target.as_str(),
                scope_label
            );
            println!(
                "The Kimetsu binary, brain, and any other hooks/servers are untouched.\n\
                 To reinstall: `kimetsu plugin install {}`",
                target.as_str()
            );
        }
    }
    Ok(())
}

/// Try to determine the version of `kimetsu` on the PATH by running `kimetsu --version`.
/// Returns `None` if not found or if the output is not parseable.
pub(crate) fn kimetsu_version_on_path() -> Option<String> {
    let output = std::process::Command::new("kimetsu")
        .arg("--version")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let text = stdout.trim();
    // clap emits "kimetsu <version>"
    text.strip_prefix("kimetsu ").map(|rest| rest.to_string())
}

pub(crate) fn bridge_skill_config(no_user_skills: bool) -> kimetsu_chat::SkillConfig {
    kimetsu_chat::SkillConfig {
        include_user_roots: !no_user_skills,
        ..kimetsu_chat::SkillConfig::default()
    }
}
