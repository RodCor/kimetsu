//! interactive chat REPL entry.
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

pub(crate) fn chat(args: ChatArgs) -> KimetsuResult<()> {
    use kimetsu_chat::{
        ChatConfig, ChatUi, SkillRegistry, rich_ui_enabled_from_env, run_repl, skill_origin_label,
    };
    use std::io::{stdin, stdout};

    let mut config = ChatConfig::new(args.workspace);
    config.brain_project = args.project;
    if let Some(m) = args.model {
        config.model = m;
    } else if let Ok(m) = std::env::var("KIMETSU_MODEL")
        && !m.is_empty()
    {
        config.model = m;
    }
    config.max_cost_usd = args.max_cost_usd;
    config.goal = args.goal;
    config.strict_verify = args.strict;
    config.skills.selected = args.skills;
    config.skills.roots = args.skill_dirs;
    config.skills.include_workspace_roots = !args.no_workspace_skills;
    config.skills.include_user_roots = !args.no_user_skills;

    let stdin = stdin();
    let stdout = stdout();
    config.raw_terminal_input = stdin.is_terminal() && stdout.is_terminal();
    config.persist_sessions = true;
    config.ui = if !args.plain && stdout.is_terminal() && rich_ui_enabled_from_env() {
        ChatUi::rich()
    } else {
        ChatUi::plain()
    }
    .with_logo(!args.no_logo);
    if args.list_skill_sources {
        let workspace = config.workspace_root.canonicalize()?;
        let registry = SkillRegistry::discover(&workspace, &config.skills)
            .map_err(|err| format!("kimetsu chat --list-skill-sources: {err}"))?;
        if registry.roots().is_empty() {
            println!("no skill sources configured");
        } else {
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
                println!(
                    "{} [{}; {}; {}{}]\n  {}",
                    root.source.as_str(),
                    root.kind.as_str(),
                    status,
                    login,
                    marketplace,
                    root.path.display()
                );
            }
        }
        return Ok(());
    }
    if !args.install_skills.is_empty() {
        let workspace = config.workspace_root.canonicalize()?;
        let mut registry = SkillRegistry::discover(&workspace, &config.skills)
            .map_err(|err| format!("kimetsu chat --install-skill: {err}"))?;
        for selection in &args.install_skills {
            let installed = registry
                .install_as_kimetsu(selection, args.install_skill_force)
                .map_err(|err| format!("kimetsu chat --install-skill {selection}: {err}"))?;
            println!(
                "installed {} as Kimetsu skill\n  {}",
                installed.name,
                installed.root.display()
            );
            registry
                .refresh(&config.skills)
                .map_err(|err| format!("kimetsu chat --install-skill refresh: {err}"))?;
        }
        if !args.list_skills {
            return Ok(());
        }
    }
    if let Some(query) = &args.search_skills {
        let workspace = config.workspace_root.canonicalize()?;
        let registry = SkillRegistry::discover(&workspace, &config.skills)
            .map_err(|err| format!("kimetsu chat --search-skills: {err}"))?;
        let matches = registry.matching_skills(query);
        if matches.is_empty() {
            println!("no skills matched `{query}`");
        } else {
            for skill in matches {
                let state = if registry.is_installed(skill) {
                    "installed"
                } else {
                    "available"
                };
                println!(
                    "{} [{}; {}]\n  {}\n  root: {}\n  entrypoint: {}\n  resources: {}",
                    skill.name,
                    state,
                    skill_origin_label(skill),
                    skill.description,
                    skill.root.display(),
                    skill.path.display(),
                    skill.resource_summary()
                );
            }
        }
        return Ok(());
    }
    if args.list_skills {
        let workspace = config.workspace_root.canonicalize()?;
        let registry = SkillRegistry::discover(&workspace, &config.skills)
            .map_err(|err| format!("kimetsu chat --list-skills: {err}"))?;
        if registry.skills().is_empty() {
            println!("no skills found");
        } else {
            for skill in registry.skills() {
                println!(
                    "{} [{}]\n  {}\n  root: {}\n  entrypoint: {}\n  resources: {}",
                    skill.name,
                    skill_origin_label(skill),
                    skill.description,
                    skill.root.display(),
                    skill.path.display(),
                    skill.resource_summary()
                );
            }
        }
        return Ok(());
    }
    let reader = stdin.lock();
    let writer = stdout.lock();
    run_repl(reader, writer, config).map_err(|e| format!("kimetsu chat: {e}").into())
}
