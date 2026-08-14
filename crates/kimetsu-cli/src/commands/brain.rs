//! brain subcommands: status/insights/roi/tune/consolidate/forget/export/import/sync/etc.
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

pub(crate) fn brain(command: BrainCommand) -> KimetsuResult<()> {
    // v0.8: honor the [embedder] config (env still wins) for every
    // command except `model set`, which sets the new selection itself.
    // The embedder is a process-static OnceLock, so this must run
    // before any retrieval/reindex touches it — entry is the safe spot.
    if !matches!(command, BrainCommand::Model { .. }) {
        apply_embedder_from_cwd();
    }
    match command {
        BrainCommand::IngestRepo { path } => {
            let summary = project::ingest_repo(&path)?;
            println!("repo_root: {}", summary.repo_root.display());
            println!("indexed_files: {}", summary.indexed_files);
            println!("skipped_files: {}", summary.skipped_files);
            println!("manifests: {}", summary.manifests);
            Ok(())
        }
        BrainCommand::Search(args) => {
            let capsules = project::search_files(&env::current_dir()?, &args.query, args.limit)?;
            if capsules.is_empty() {
                println!("no file matches");
                return Ok(());
            }

            for capsule in capsules {
                println!(
                    "{:.3} {} {}",
                    capsule.score, capsule.expansion_handle, capsule.summary
                );
            }
            Ok(())
        }
        BrainCommand::Context(args) => {
            let cwd = env::current_dir()?;
            // v0.4.4: auto-augment with ambient workspace context
            // (git branch + dirty files + recent edits) unless the
            // caller opts out via --no-ambient or
            // `KIMETSU_BRAIN_AMBIENT=off`. The augmentation appends
            // a short, lexically + semantically retrievable suffix
            // to the query before retrieval — see
            // `kimetsu_brain::ambient::augment_query`.
            // W3.2: load broker.ambient from project config (env still wins).
            // Load the resolved config once here and reuse it below for the
            // retrieval-level HyDE decision (load_config has already applied
            // the [retrieval] level preset).
            let context_config = kimetsu_core::paths::ProjectPaths::discover(&cwd)
                .ok()
                .and_then(|paths| project::load_config(&paths).ok());
            let config_ambient = context_config
                .as_ref()
                .map(|cfg| cfg.broker.ambient)
                .unwrap_or(true);
            let (effective_query, ambient_payload) = if !args.no_ambient
                && kimetsu_brain::ambient::ambient_enabled_with(config_ambient)
            {
                let ctx = kimetsu_brain::ambient::collect(&cwd);
                let augmented = kimetsu_brain::ambient::augment_query(&args.query, &ctx);
                (augmented, Some(ctx))
            } else {
                (args.query.clone(), None)
            };
            // #1a HyDE: expand the (ambient-augmented) query with a hypothetical
            // answer before retrieval. HyDE is on when explicitly requested
            // (--hyde) OR when the configured retrieval level is "advanced".
            let hyde_from_level = context_config
                .as_ref()
                .map(|cfg| cfg.hyde_from_level())
                .unwrap_or(false);
            let hyde_enabled = args.hyde || hyde_from_level;
            // Advanced level leans on a capable cheap model; nudge the user if
            // none is configured (non-fatal; the raw query is still used).
            if hyde_from_level && distiller::resolve_pipeline_distiller(&cwd).is_none() {
                eprintln!(
                    "kimetsu: retrieval level 'advanced' needs the deep tier and a capable cheap model (OpenAI/Anthropic or a larger local model like qwen2.5:14b); set [cheap_model] in project.toml."
                );
            }
            let effective_query = if hyde_enabled {
                hyde_augment_query(&cwd, &effective_query)
            } else {
                effective_query
            };
            let bundle =
                project::retrieve_context(&cwd, &args.stage, &effective_query, args.budget_tokens)?;
            // v2.7: cross-encoder parity with the MCP/daemon paths + evidence
            // -band arbitration. Without this the CLI (and every benchmark
            // driving it) measured the bi-encoder alone while `deep` configs
            // promised a reranker, and the abstention band always failed
            // closed. Floor 0.0 / cap 0: pure reorder — no capsule dropped by
            // the reranker itself, arbitration is the only conversion.
            #[cfg(feature = "embeddings")]
            let bundle = {
                let reranker = context_config.as_ref().and_then(|cfg| {
                    kimetsu_brain::embeddings::open_reranker_for_model(&cfg.embedder.reranker)
                });
                let abstain = context_config
                    .as_ref()
                    .map(project::resolved_abstain_evidence_for)
                    .unwrap_or(0.0);
                kimetsu_brain::context::rerank_and_arbitrate(
                    &effective_query,
                    bundle,
                    reranker.as_deref(),
                    abstain,
                    0.0,
                    0,
                )
            };
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "ok": true,
                        "stage": bundle.stage,
                        "query": args.query,
                        "augmented_query": effective_query,
                        "ambient": ambient_payload,
                        "budget_tokens": bundle.budget_tokens,
                        "used_tokens": bundle.used_tokens,
                        "capsule_count": bundle.capsules.len(),
                        "excluded_count": bundle.excluded.len(),
                        // v2.6: the bundle's own judgement of itself, which the
                        // MCP surface has exposed since abstention landed. It
                        // belongs here too — a JSON caller cannot tell a bundle
                        // that answers the question from one that touches half
                        // of it without these, and that is exactly the
                        // difference BrainBench's sycophancy track scores.
                        "top_score": bundle.top_score,
                        "top_abs_evidence": bundle.top_abs_evidence,
                        "skipped": bundle.skipped,
                        "evidence_coverage": bundle.evidence_coverage,
                        "uncovered_terms": bundle.uncovered_terms,
                        "chronological": bundle.chronological,
                        "capsules": bundle.capsules,
                        "excluded": bundle.excluded,
                    }))?
                );
                return Ok(());
            }
            println!(
                "stage: {} used_tokens: {}/{} capsules: {} excluded: {}",
                bundle.stage,
                bundle.used_tokens,
                bundle.budget_tokens,
                bundle.capsules.len(),
                bundle.excluded.len()
            );
            for capsule in bundle.capsules {
                println!(
                    "{:.3} {} [{} rel={:.2} conf={:.2} fresh={:.2} scope={:.2} tokens={}]",
                    capsule.score,
                    capsule.expansion_handle,
                    capsule.kind,
                    capsule.relevance,
                    capsule.confidence,
                    capsule.freshness,
                    capsule.scope_weight,
                    capsule.token_estimate
                );
                println!("  {}", capsule.summary);
            }
            Ok(())
        }
        BrainCommand::Memory { command } => memory(command),
        BrainCommand::Rebuild { from_traces } => {
            let events = project::rebuild_projection(&env::current_dir()?, from_traces)?;
            println!("brain projection rebuilt from {events} events");
            Ok(())
        }
        BrainCommand::Stats => stats(),
        BrainCommand::Status { json } => brain_status(json),
        BrainCommand::Insights {
            json,
            last_n_runs,
            since,
            top,
        } => brain_insights(json, last_n_runs, since, top),
        BrainCommand::ContextHook(args) => brain_context_hook(args),
        BrainCommand::StopHook(args) => brain_stop_hook(args),
        BrainCommand::Reindex(args) => reindex_brain(args),
        BrainCommand::Model { command } => brain_model(command),
        BrainCommand::PreToolHook(args) => proactive_hook(ProactiveEvent::PreTool, args),
        BrainCommand::PostToolHook(args) => proactive_hook(ProactiveEvent::PostTool, args),
        BrainCommand::SessionEndHook(args) => {
            let workspace = args
                .workspace
                .unwrap_or_else(|| env::current_dir().unwrap_or_default());
            distiller::run_session_end_hook(&workspace);
            // Slice B: hands-off team memory — auto-sync at session end when a
            // `[sync] dir` is configured (and `auto` not disabled). Best-effort:
            // a sync failure must never break session shutdown.
            auto_sync_at_session_end(&workspace);
            // v2.6: the other upkeep tick. Codex, Pi and OpenClaw have no
            // session-start event, so session end is where their brains get
            // their maintenance — and on Claude Code it is a second chance
            // after a long session moved the corpus.
            spawn_maintenance_if_due(&workspace);
            Ok(())
        }
        BrainCommand::SessionStartHook(args) => {
            let workspace = args
                .workspace
                .unwrap_or_else(|| env::current_dir().unwrap_or_default());
            brain_session_start_hook(&workspace)
        }
        BrainCommand::Digest(args) => {
            let workspace = args
                .workspace
                .unwrap_or_else(|| env::current_dir().unwrap_or_default());
            brain_digest_cmd(&workspace, args.refresh)
        }
        BrainCommand::Compact(args) => brain_compact(args),
        BrainCommand::Export(args) => brain_export(args),
        BrainCommand::Import(args) => brain_import(args),
        BrainCommand::Backup(args) => brain_backup(args),
        BrainCommand::EmbedDaemon(args) => brain_embed_daemon(args),
        BrainCommand::Warm => brain_warm(),
        BrainCommand::Daemon(args) => brain_daemon(args),
        BrainCommand::Eval(args) => brain_eval(args),
        BrainCommand::Bench(args) => brain_bench(args),
        BrainCommand::Roi(args) => brain_roi(args),
        BrainCommand::Tune(args) => brain_tune(args),
        BrainCommand::Policy(args) => brain_policy(args),
        BrainCommand::Maintain(args) => brain_maintain(args),
        BrainCommand::Audit(args) => brain_audit(args),
        BrainCommand::Drift(args) => brain_drift(args),
        BrainCommand::AsOf(args) => brain_as_of(args),
        BrainCommand::Consolidate(args) => brain_consolidate(args),
        BrainCommand::Reflect(args) => brain_reflect(args),
        BrainCommand::Triage(args) => brain_triage(args),
        BrainCommand::Forget(args) => brain_forget(args),
        BrainCommand::Cite(args) => brain_cite(args),
        BrainCommand::Reinforce(args) => brain_reinforce(args),
        BrainCommand::BenchmarkCredit(args) => brain_benchmark_credit(args),
        BrainCommand::Regret(args) => brain_regret(args),
        BrainCommand::Distill(args) => brain_distill(args),
        BrainCommand::Graph { command } => brain_graph(command),
        BrainCommand::Ask(args) => brain_ask(args),
        BrainCommand::Skills(args) => brain_skills(args),
        BrainCommand::Sync(args) => brain_sync(args),
    }
}

/// v0.8: best-effort — load the project config from the current dir and
/// record its `[embedder] model` so brain-internal callers resolve it
/// (env still wins). Silently no-ops when the brain isn't initialized.
pub(crate) fn apply_embedder_from_cwd() {
    if let Ok(cwd) = env::current_dir()
        && let Ok(paths) = kimetsu_core::paths::ProjectPaths::discover(&cwd)
        && let Ok(config) = project::load_config(&paths)
    {
        kimetsu_brain::embeddings::apply_embedder_selection(Some(&config.embedder.model));
    }
}

/// v0.8: `kimetsu brain model list|set`.
pub(crate) fn brain_model(command: ModelCommand) -> KimetsuResult<()> {
    match command {
        ModelCommand::List { json } => brain_model_list(json),
        ModelCommand::Set(args) => brain_model_set(args),
    }
}

pub(crate) fn brain_model_list(json: bool) -> KimetsuResult<()> {
    use kimetsu_brain::embeddings::{BUILTIN_MODELS, resolve_embedder_id};

    // Resolve the active id + where it came from, best-effort.
    let (config_model, source) = match env::current_dir()
        .ok()
        .and_then(|cwd| kimetsu_core::paths::ProjectPaths::discover(&cwd).ok())
        .and_then(|paths| project::load_config(&paths).ok())
    {
        Some(cfg) => (Some(cfg.embedder.model.clone()), "config"),
        None => (None, "default"),
    };
    let env_set = env::var("KIMETSU_BRAIN_EMBEDDER")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let source = if env_set.is_some() { "env" } else { source };
    let active = resolve_embedder_id(config_model.as_deref());

    if json {
        let models: Vec<_> = BUILTIN_MODELS
            .iter()
            .map(|(id, dim, blurb)| {
                serde_json::json!({
                    "id": id,
                    "dim": dim,
                    "description": blurb,
                    "active": *id == active,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ok": true,
                "active": active,
                "source": source,
                "models": models,
            }))?
        );
        return Ok(());
    }

    println!("Embedding models (active resolved from {source}):");
    for (id, dim, blurb) in BUILTIN_MODELS {
        let marker = if *id == active { "*" } else { " " };
        println!("  {marker} {id:<22} {dim:>5}d  {blurb}");
    }
    println!("\nChange with: kimetsu brain model set <id>");
    println!("(env KIMETSU_BRAIN_EMBEDDER always overrides the config field)");
    Ok(())
}

pub(crate) fn brain_model_set(args: ModelSetArgs) -> KimetsuResult<()> {
    use kimetsu_brain::embeddings::{apply_embedder_selection, resolve_embedder_id};

    // Validate against the curated set so `set` never silently falls
    // back to the default for a typo'd id.
    if !is_known_alias(&args.id) {
        return Err(format!(
            "unknown embedder id `{}`. Run `kimetsu brain model list` for the options.",
            args.id
        )
        .into());
    }
    let canonical = resolve_embedder_id(Some(&args.id));

    let workspace = args.workspace.clone().unwrap_or(env::current_dir()?);
    let paths = kimetsu_core::paths::ProjectPaths::discover(&workspace)?;
    let mut config = project::load_config(&paths)?;
    let previous = config.embedder.model.clone();
    let prev_dim = dim_for(resolve_embedder_id(Some(&previous)));
    let new_dim = dim_for(canonical);

    config.embedder.model = canonical.to_string();
    std::fs::write(&paths.project_toml, config.to_toml()?)?;

    // Fresh CLI process: the embedder OnceLock is not yet initialized,
    // so recording the override here means the reindex below loads the
    // NEW model.
    apply_embedder_selection(Some(canonical));

    let dim_changed = prev_dim != new_dim;

    if args.no_reindex {
        if args.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "ok": true, "model": canonical, "previous": previous,
                    "reindexed": false, "dimension_changed": dim_changed,
                }))?
            );
        } else {
            println!(
                "Embedder set to `{canonical}` (was `{previous}`). Skipped reindex (--no-reindex)."
            );
            if dim_changed {
                println!(
                    "Dimension changed {prev_dim}d -> {new_dim}d: run `kimetsu brain reindex --force` so cosine retrieval uses the new model."
                );
            }
        }
        return Ok(());
    }

    // Re-embed with a FRESH embedder for the new model (not whatever the
    // default cache might resolve to), so the corpus is migrated to the
    // chosen model deterministically.
    let embedder = kimetsu_brain::embeddings::open_embedder_for_model(canonical);
    let report = kimetsu_brain::reindex::reindex_all_with_embedder(
        &workspace,
        kimetsu_brain::reindex::ReindexOptions {
            scope: kimetsu_brain::reindex::ReindexScope::All,
            dry_run: false,
            force: dim_changed,
            limit: None,
        },
        embedder.as_ref(),
    )?;

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ok": true, "model": canonical, "previous": previous,
                "reindexed": !report.embedder_noop,
                "dimension_changed": dim_changed,
                "updated": report.updated_total(),
                "embedder_noop": report.embedder_noop,
            }))?
        );
        return Ok(());
    }

    println!("Embedder set to `{canonical}` (was `{previous}`).");
    if report.embedder_noop {
        println!(
            "Active embedder is `noop` (lean build or KIMETSU_BRAIN_EMBEDDER=noop): id recorded, but no vectors were produced. Build with `--features embeddings` then run `kimetsu brain reindex`."
        );
    } else {
        println!(
            "Reindexed {} memories with the new model.",
            report.updated_total()
        );
    }
    Ok(())
}

pub(crate) fn is_known_alias(id: &str) -> bool {
    matches!(
        id.trim().to_ascii_lowercase().as_str(),
        "default"
            | "bge-small"
            | "bge-small-en-v1.5"
            | "bge-m3"
            | "m3"
            | "jina-code"
            | "jina-v2-base-code"
            | "jina-embeddings-v2-base-code"
    )
}

pub(crate) fn dim_for(canonical_id: &str) -> usize {
    kimetsu_brain::embeddings::BUILTIN_MODELS
        .iter()
        .find(|(id, _, _)| *id == canonical_id)
        .map(|(_, dim, _)| *dim)
        .unwrap_or(0)
}

/// v0.4.3: `kimetsu brain reindex` — backfill missing / stale
/// embeddings. The interesting cases:
///
///   * NoopEmbedder (default Cargo build OR
///     `KIMETSU_BRAIN_EMBEDDER=noop`): we print a hint and exit.
///     Without a real embedder there's nothing to reindex against.
///   * Real embedder + dry-run: counts how many rows are stale per
///     scope without writing.
///   * Real embedder + apply: walks both project and (optionally)
///     user brains, re-embeds candidate rows in created_at order,
///     prints a summary per scope.
pub(crate) fn reindex_brain(args: ReindexArgs) -> KimetsuResult<()> {
    let scope = kimetsu_brain::reindex::ReindexScope::parse(&args.scope)?;
    let opts = kimetsu_brain::reindex::ReindexOptions {
        scope,
        dry_run: args.dry_run,
        force: args.force,
        limit: args.limit,
    };
    let report = kimetsu_brain::reindex::reindex_all(&env::current_dir()?, opts)?;

    if report.embedder_noop {
        println!(
            "[reindex] active embedder is `noop` — nothing to do. \
             Build kimetsu with `--features embeddings` and unset \
             KIMETSU_BRAIN_EMBEDDER=noop to enable semantic retrieval."
        );
        return Ok(());
    }

    println!(
        "[reindex] model={} dry_run={} force={} scope={:?}{}",
        report.embedder_model_id,
        args.dry_run,
        args.force,
        scope,
        args.limit
            .map(|n| format!(" limit={n}"))
            .unwrap_or_default(),
    );
    for sub in [&report.project, &report.user] {
        if !sub.opened {
            println!("  {}: skipped (scope filter or DB unavailable)", sub.scope);
            continue;
        }
        let action = if args.dry_run {
            "candidates"
        } else {
            "updated"
        };
        let count = if args.dry_run {
            sub.candidates
        } else {
            sub.updated
        };
        println!(
            "  {}: total={} {}={} failed={}",
            sub.scope, sub.total, action, count, sub.failed
        );
    }
    println!(
        "[reindex] {} total {} across project + user",
        if args.dry_run {
            report.candidates_total()
        } else {
            report.updated_total()
        },
        if args.dry_run {
            "candidates"
        } else {
            "updated"
        },
    );
    Ok(())
}

// ── Q8: brain compact ────────────────────────────────────────────────────────

// ── Flagship 1 Pass B: session-start-hook + digest command ───────────────────

/// `kimetsu brain session-start-hook`
///
/// Flagship 1 / Pass B / Story 1.5: SessionStart hook that injects the
/// repo digest (1.1) + episodic resume (Pass A) as `additionalContext` so
/// the agent's first turn knows the repo and task without exploratory I/O.
///
/// Output format: Claude Code `additionalContext` JSON.
/// Gated by `[broker] warm_start` (default true).
/// Silent when no digest AND no live episode.
pub(crate) fn brain_session_start_hook(workspace: &Path) -> KimetsuResult<()> {
    // v2.6: session start is the natural upkeep tick — the agent is about to
    // work, so anything overdue should run alongside rather than in front of
    // it. Detached, so this returns immediately. Before the warm-start guard
    // below: a brain with nothing to say still needs its upkeep.
    spawn_maintenance_if_due(workspace);

    let Some(additional_context) = warm_start_context(workspace) else {
        return Ok(());
    };

    // Emit Claude Code SessionStart additionalContext JSON.
    let output = serde_json::json!({
        "continue": true,
        "hookSpecificOutput": {
            "hookEventName": "SessionStart",
            "additionalContext": additional_context,
        },
    });
    println!("{}", serde_json::to_string(&output)?);
    Ok(())
}

/// Assemble the warm-start block: repo digest + episodic resume.
///
/// Thin wrapper over [`kimetsu_brain::digest::warm_start_block`], which the
/// MCP server shares so Cursor — no hooks, no session-start surface — gets the
/// same block on its first `kimetsu_brain_context` call.
pub(crate) fn warm_start_context(workspace: &Path) -> Option<String> {
    kimetsu_brain::digest::warm_start_block(workspace)
}

/// Normalize a user-supplied time into RFC 3339.
///
/// Accepts a bare `YYYY-MM-DD` because that is how people actually name a day,
/// and a full RFC 3339 timestamp for precision. A date alone means midnight
/// UTC — the start of that day, so "what did it know on the 3rd" excludes
/// everything learned during the 3rd, which is the conservative reading.
fn normalize_as_of(when: &str) -> KimetsuResult<String> {
    let trimmed = when.trim();
    if trimmed.len() == 10 && trimmed.matches('-').count() == 2 {
        return Ok(format!("{trimmed}T00:00:00Z"));
    }
    // Anything else must already be a timestamp the DB can compare
    // lexicographically, which for RFC 3339 in UTC is the same as temporally.
    if trimmed.len() >= 20 && trimmed.contains('T') {
        return Ok(trimmed.to_string());
    }
    Err(format!(
        "could not read `{trimmed}` as a time — use YYYY-MM-DD or a full RFC 3339 timestamp \
         like 2026-03-01T00:00:00Z"
    )
    .into())
}

/// `kimetsu brain as-of <WHEN> [--since <WHEN>] [--limit N] [--json]`
pub(crate) fn brain_as_of(args: AsOfArgs) -> KimetsuResult<()> {
    use kimetsu_brain::bitemporal;

    let workspace = args
        .workspace
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());
    let (_paths, _config, conn) = project::load_project_readonly(&workspace)?;
    let when = normalize_as_of(&args.when)?;

    if let Some(since) = args.since.as_deref() {
        let from = normalize_as_of(since)?;
        let delta = bitemporal::belief_delta(&conn, &from, &when)?;
        if args.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "from": from,
                    "to": when,
                    "learned": delta.learned.iter().map(as_of_json).collect::<Vec<_>>(),
                    "retired": delta.retired.iter().map(as_of_json).collect::<Vec<_>>(),
                }))?
            );
            return Ok(());
        }
        println!("Between {from} and {when}:");
        println!();
        println!("  learned ({}):", delta.learned.len());
        for m in delta.learned.iter().take(args.limit.max(1) as usize) {
            println!("    + [{}] {}", m.kind, m.text);
        }
        println!("  retired ({}):", delta.retired.len());
        for m in delta.retired.iter().take(args.limit.max(1) as usize) {
            let why = m.retired_reason.as_deref().unwrap_or("no longer believed");
            println!("    - [{}] {} ({why})", m.kind, m.text);
        }
        return Ok(());
    }

    let memories = bitemporal::memories_as_of(&conn, &when, args.limit)?;
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "as_of": when,
                "count": memories.len(),
                "memories": memories.iter().map(as_of_json).collect::<Vec<_>>(),
            }))?
        );
        return Ok(());
    }

    println!(
        "The brain believed {} memor{} at {when}:",
        memories.len(),
        if memories.len() == 1 { "y" } else { "ies" }
    );
    println!();
    for m in &memories {
        match m.retired_reason.as_deref() {
            // Flag the interesting ones: beliefs that were live then and are
            // not now. That contrast is the reason to run this at all.
            Some(reason) => println!("  [{}] {}  — since {reason}", m.kind, m.text),
            None => println!("  [{}] {}", m.kind, m.text),
        }
    }
    Ok(())
}

fn as_of_json(m: &kimetsu_brain::bitemporal::AsOfMemory) -> serde_json::Value {
    serde_json::json!({
        "memory_id": m.memory_id,
        "scope": m.scope,
        "kind": m.kind,
        "text": m.text,
        "created_at": m.created_at,
        "retired_at": m.retired_at,
        "retired_reason": m.retired_reason,
    })
}

/// `kimetsu brain audit [--json]`
///
/// Where the corpus came from, and how much of it nobody has vetted.
///
/// Deliberately read-only. An automated purge keyed on "many writes in one
/// minute" would delete a legitimate bulk import — a worse outcome than the
/// attack it guards against — so this reports and a human decides.
pub(crate) fn brain_audit(args: AuditArgs) -> KimetsuResult<()> {
    let workspace = args
        .workspace
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());
    let (_paths, _config, conn) = project::load_project_readonly(&workspace)?;
    let report = kimetsu_brain::trust::audit(&conn)?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    println!("Corpus: {} active memories", report.total);
    if report.groups.is_empty() {
        return Ok(());
    }
    println!();
    println!(
        "{:<12} {:>8} {:>14} {:>9}",
        "origin", "total", "corroborated", "unvetted"
    );
    for group in &report.groups {
        println!(
            "{:<12} {:>8} {:>14} {:>9}",
            group.provenance, group.total, group.corroborated, group.unvetted
        );
    }

    let unvetted: usize = report.groups.iter().map(|g| g.unvetted).sum();
    if unvetted > 0 {
        println!();
        println!(
            "{unvetted} memor{} of external origin {} never been cited in a successful run \
             here, so {} still carrying an origin discount in retrieval. Review with \
             `kimetsu brain memory list`.",
            if unvetted == 1 { "y" } else { "ies" },
            if unvetted == 1 { "has" } else { "have" },
            if unvetted == 1 { "it is" } else { "they are" },
        );
    }

    if !report.bursts.is_empty() {
        println!();
        println!(
            "Write bursts (>= {} in one minute):",
            kimetsu_brain::trust::BURST_THRESHOLD
        );
        for burst in &report.bursts {
            println!("  {}  {} writes", burst.minute, burst.writes);
        }
        println!(
            "A burst is the shape a bulk import leaves — and also the shape induced \
             poisoning leaves. Worth confirming you recognise each one."
        );
    }
    Ok(())
}

/// `kimetsu brain drift [--limit N] [--json]`
///
/// Which recent sessions wandered off the task they opened with. See
/// [`kimetsu_brain::drift`] for what this can and cannot see — Kimetsu observes
/// user prompts, not agent actions, so this is a claim about the session's
/// topic and not about the agent's behaviour.
pub(crate) fn brain_drift(args: DriftArgs) -> KimetsuResult<()> {
    let workspace = args
        .workspace
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());
    let (_paths, config, conn) = project::load_project_readonly(&workspace)?;
    let sessions = kimetsu_brain::drift::recent_sessions(&conn, args.limit)?;

    // The signal is cosine against an anchor, so a build with no embedder has
    // nothing to compute. Saying so is the only honest output — a lexical
    // stand-in would report a number on a different scale under the same
    // threshold, which is worse than reporting none.
    let embedder = kimetsu_brain::embeddings::open_embedder_for(config.embedder.enabled);
    if embedder.is_noop() {
        if args.json {
            println!(
                "{}",
                serde_json::json!({
                    "ok": false,
                    "reason": "no embedder: drift is cosine against an anchor",
                    "sessions_available": sessions.len(),
                })
            );
        } else {
            println!(
                "Drift is measured as cosine against the session's opening turn, and this \
                 build has no embedder, so there is nothing to compute. {} recent session{} \
                 would be scorable on an embeddings build.",
                sessions.len(),
                if sessions.len() == 1 { "" } else { "s" }
            );
        }
        return Ok(());
    }

    let mut reports = Vec::new();
    for session in &sessions {
        let mut embedded = Vec::with_capacity(session.queries.len());
        for query in &session.queries {
            match embedder.embed(query) {
                Ok(vector) => embedded.push(vector),
                // One unembeddable turn must not silently shorten the sequence
                // and shift every index after it.
                Err(_) => {
                    embedded.clear();
                    break;
                }
            }
        }
        if embedded.is_empty() {
            continue;
        }
        reports.push(kimetsu_brain::drift::analyze(
            &session.session_id,
            &embedded,
        ));
    }

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ok": true,
                "sessions": reports.iter().map(|r| serde_json::json!({
                    "session_id": r.session_id,
                    "turns": r.similarity.len(),
                    "similarity": r.similarity,
                    "drifted_at": r.drifted_at,
                    "min_similarity": r.min_similarity(),
                })).collect::<Vec<_>>(),
            }))?
        );
        return Ok(());
    }

    if reports.is_empty() {
        println!(
            "No scorable sessions. Drift needs at least two stored prompts from one \
             session; `[learning] store_queries = false` keeps only a hash."
        );
        return Ok(());
    }

    println!(
        "{:<24} {:>6} {:>8} {:>10}",
        "session", "turns", "closest", "turned at"
    );
    for report in &reports {
        println!(
            "{:<24} {:>6} {:>8.2} {:>10}",
            report.session_id,
            report.similarity.len(),
            report.min_similarity(),
            match report.drifted_at {
                Some(idx) => format!("turn {}", idx + 1),
                None => "-".to_string(),
            }
        );
    }

    let drifted = reports.iter().filter(|r| r.drifted()).count();
    if drifted > 0 {
        println!();
        println!(
            "{drifted} session{} moved away from the question {} opened with and stayed \
             there for {} turns. Retrieval still anchors on the whole session, so its \
             opening turns are steering results toward a task nobody is working on.",
            if drifted == 1 { "" } else { "s" },
            if drifted == 1 { "it" } else { "they" },
            kimetsu_brain::drift::SUSTAINED_TURNS
        );
    }
    Ok(())
}

/// `kimetsu brain maintain [--force] [--status] [--only ...] [--json]`
///
/// The brain's upkeep, on a schedule instead of on a human remembering. Fired
/// detached by the session hooks when a pass is overdue; see
/// [`kimetsu_brain::maintain`] for why this is not a resident daemon.
pub(crate) fn brain_maintain(args: MaintainArgs) -> KimetsuResult<()> {
    use kimetsu_brain::maintain::{self, Pass};

    let workspace = args
        .workspace
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());
    let paths = kimetsu_core::paths::ProjectPaths::discover(&workspace)?;
    let now = maintain::now_unix();
    let mut state = maintain::load_state(&paths.kimetsu_dir);

    let selected: Vec<Pass> = if args.only.trim().is_empty() {
        if args.force {
            Pass::ALL.to_vec()
        } else {
            maintain::due_passes(&state, now)
        }
    } else {
        let mut chosen = Vec::new();
        for name in args.only.split(',').filter(|s| !s.trim().is_empty()) {
            chosen.push(name.parse::<Pass>()?);
        }
        chosen
    };

    if args.status {
        if args.json {
            let passes: Vec<serde_json::Value> = Pass::ALL
                .into_iter()
                .map(|pass| {
                    serde_json::json!({
                        "pass": pass.as_str(),
                        "interval_secs": pass.interval_secs(),
                        "last_run_unix": state.last_run(pass),
                        "due": selected.contains(&pass),
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&passes)?);
        } else if selected.is_empty() {
            println!("Maintenance: nothing due.");
        } else {
            println!(
                "Maintenance: {} pass(es) due — {}",
                selected.len(),
                selected
                    .iter()
                    .map(|p| p.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        return Ok(());
    }

    if selected.is_empty() {
        if args.json {
            println!("[]");
        } else {
            println!("Maintenance: nothing due.");
        }
        return Ok(());
    }

    let outcomes = maintain::run_passes(&workspace, &selected);
    for (pass, outcome) in selected.iter().zip(&outcomes) {
        // Only a pass that succeeded counts as having run: a failed pass should
        // be retried on the next tick, not silently skipped for its interval.
        if outcome.ok {
            state.mark_ran(*pass, now);
        }
    }
    // Best-effort: failing to persist the schedule means the next tick repeats
    // the work, which is wasteful but harmless.
    let _ = maintain::save_state(&paths.kimetsu_dir, &state);

    if args.json {
        println!("{}", serde_json::to_string_pretty(&outcomes)?);
    } else {
        for outcome in &outcomes {
            let mark = if outcome.ok { "✓" } else { "✗" };
            println!("{mark} {:<10} {}", outcome.pass, outcome.detail);
        }
    }
    Ok(())
}

/// Fire `kimetsu brain maintain` detached when a pass is overdue.
///
/// Called from the session hooks. Fully detached with null stdio, mirroring the
/// embed daemon and the digest refresh: an inherited stdout pipe would hold the
/// host's hook open until its timeout, which is exactly the failure this design
/// exists to avoid.
pub(crate) fn spawn_maintenance_if_due(workspace: &Path) {
    use kimetsu_brain::maintain;

    let Ok(paths) = kimetsu_core::paths::ProjectPaths::discover(workspace) else {
        return;
    };
    let state = maintain::load_state(&paths.kimetsu_dir);
    if maintain::due_passes(&state, maintain::now_unix()).is_empty() {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["brain", "maintain", "--workspace"])
        .arg(workspace)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP
        cmd.creation_flags(0x0000_0008 | 0x0000_0200);
    }
    let _ = cmd.spawn();
}

/// `kimetsu brain policy [--train] [--reset] [--json]`
///
/// The proactive-injection policy decides whether a mid-task recall is worth
/// interrupting for. This is the surface that makes it inspectable: a linear
/// model whose weights you can read is the reason it is a linear model.
pub(crate) fn brain_policy(args: PolicyArgs) -> KimetsuResult<()> {
    use kimetsu_brain::inject_policy;

    let workspace = args
        .workspace
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());
    let paths = kimetsu_core::paths::ProjectPaths::discover(&workspace)?;

    if args.reset {
        inject_policy::reset(&paths.kimetsu_dir)?;
        println!("Injection policy reset to the legacy threshold rule.");
        return Ok(());
    }

    let (_paths, _config, conn) = project::load_project_readonly(&workspace)?;
    let examples = inject_policy::collect_examples(&conn).unwrap_or_default();
    let current = inject_policy::load(&paths.kimetsu_dir);

    let policy = if args.train {
        let fitted = inject_policy::fit(&examples);
        if fitted.is_prior() {
            println!(
                "Not enough signal to train: {} labelled injection{} \
                 (need {}, with both outcomes present). Keeping the legacy rule.",
                examples.len(),
                if examples.len() == 1 { "" } else { "s" },
                inject_policy::MIN_TRAINING_EXAMPLES
            );
            return Ok(());
        }
        let stamped = kimetsu_brain::inject_policy::Policy {
            trained_at: Some(
                time::OffsetDateTime::now_utc()
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_else(|_| "unknown".to_string()),
            ),
            ..fitted
        };
        inject_policy::save(&paths.kimetsu_dir, &stamped)?;
        stamped
    } else {
        current
    };

    let prior = inject_policy::Policy::prior();
    let policy_accuracy = inject_policy::accuracy(&policy, &examples);
    let prior_accuracy = inject_policy::accuracy(&prior, &examples);
    // v2.6: acceptance per hook surface. `broker.proactive_prefetch` has been
    // default-off since it shipped, waiting on exactly this number, and nothing
    // was recording it — so the flag could never graduate. See
    // `inject_policy::surface_acceptance`.
    let surfaces = inject_policy::surface_acceptance(&conn).unwrap_or_default();

    if args.json {
        let weights: serde_json::Map<String, serde_json::Value> = inject_policy::Features::NAMES
            .iter()
            .zip(&policy.weights)
            .map(|(name, w)| (name.to_string(), serde_json::json!(w)))
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "trained": !policy.is_prior(),
                "trained_on": policy.trained_on,
                "trained_at": policy.trained_at,
                "bias": policy.bias,
                "weights": weights,
                "labelled_examples": examples.len(),
                "useful_examples": examples.iter().filter(|e| e.useful).count(),
                "accuracy": policy_accuracy,
                "legacy_rule_accuracy": prior_accuracy,
                "surfaces": surfaces.iter().map(|s| serde_json::json!({
                    "surface": s.surface,
                    "injected": s.injected,
                    "cited": s.cited,
                    "acceptance": s.acceptance(),
                })).collect::<Vec<_>>(),
            }))?
        );
        return Ok(());
    }

    if policy.is_prior() {
        println!(
            "Injection policy: the legacy threshold rule (inject at score >= {:.2}, \
             {:.2} when looping).",
            inject_policy::LEGACY_MIN_SCORE,
            inject_policy::LEGACY_LOOP_MIN_SCORE
        );
    } else {
        println!(
            "Injection policy: trained on {} injection{}{}.",
            policy.trained_on,
            if policy.trained_on == 1 { "" } else { "s" },
            policy
                .trained_at
                .as_deref()
                .map(|ts| format!(" at {ts}"))
                .unwrap_or_default()
        );
    }
    println!("weights:");
    for (name, weight) in inject_policy::Features::NAMES.iter().zip(&policy.weights) {
        println!("  {name:<20} {weight:>8.3}");
    }
    println!("  {:<20} {:>8.3}", "(bias)", policy.bias);

    let useful = examples.iter().filter(|e| e.useful).count();
    println!(
        "history: {} labelled injection{} ({} cited, {} unused)",
        examples.len(),
        if examples.len() == 1 { "" } else { "s" },
        useful,
        examples.len() - useful
    );
    if !examples.is_empty() {
        println!(
            "accuracy on that history: {:.1}% (legacy rule: {:.1}%)",
            policy_accuracy * 100.0,
            prior_accuracy * 100.0
        );
    }
    if !surfaces.is_empty() {
        println!("acceptance by hook surface (cited / injected):");
        for stats in &surfaces {
            println!(
                "  {:<20} {:>3} / {:<3}  {:>5.1}%",
                stats.surface,
                stats.cited,
                stats.injected,
                stats.acceptance() * 100.0
            );
        }
        // The prefetch surface is the one with a decision riding on it: it
        // predicts from a file path rather than reacting to a failure, and
        // `broker.proactive_prefetch` stays default-off until its acceptance is
        // not materially below the surfaces that react to something observed.
        let prefetch = surfaces
            .iter()
            .find(|s| s.surface == inject_policy::Surface::PreToolPrefetch.as_str());
        let reactive: Vec<_> = surfaces
            .iter()
            .filter(|s| s.surface != inject_policy::Surface::PreToolPrefetch.as_str())
            .collect();
        if let Some(prefetch) = prefetch
            && !reactive.is_empty()
        {
            let reactive_injected: usize = reactive.iter().map(|s| s.injected).sum();
            let reactive_cited: usize = reactive.iter().map(|s| s.cited).sum();
            let reactive_acceptance = reactive_cited as f32 / reactive_injected as f32;
            println!(
                "  prefetch vs reactive: {:.1}% vs {:.1}% — `broker.proactive_prefetch` \
                 graduates to default-on when the gap closes on real history",
                prefetch.acceptance() * 100.0,
                reactive_acceptance * 100.0
            );
        }
    }
    if policy.is_prior() && examples.len() >= inject_policy::MIN_TRAINING_EXAMPLES {
        println!("hint: run `kimetsu brain policy --train` to fit from this history");
    }
    Ok(())
}

/// `kimetsu brain digest [--refresh]`
///
/// Flagship 1 / Pass B / Story 1.1: build (or rebuild) the repo digest.
/// Prints the digest to stdout and writes `.kimetsu/digest.md`.
pub(crate) fn brain_digest_cmd(workspace: &Path, refresh: bool) -> KimetsuResult<()> {
    match kimetsu_brain::digest::build_or_load_digest(workspace, refresh) {
        Some(digest) => {
            println!("{digest}");
        }
        None => {
            eprintln!("[Kimetsu] No digest content: brain may not be initialized or empty.");
        }
    }
    Ok(())
}

/// `kimetsu brain compact [--purge-invalidated] [--trim-events-older-than <dur>] [--json]`
///
/// Reclaims dead space in brain.db via SQLite VACUUM. Optional flags allow
/// purging invalidated memory rows and trimming the durable event log.
pub(crate) fn brain_compact(args: CompactArgs) -> KimetsuResult<()> {
    let workspace = args
        .workspace
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());

    // Parse --trim-events-older-than if provided.
    let trim_dur = args
        .trim_events_older_than
        .as_deref()
        .map(parse_duration)
        .transpose()
        .map_err(|e| format!("--trim-events-older-than: {e}"))?;

    // Print warnings before performing any destructive operations.
    if let Some(ref dur_str) = args.trim_events_older_than {
        eprintln!(
            "WARNING: --trim-events-older-than {dur_str} will delete events older than \
             {dur_str} from the durable event log. Materialized memories are unaffected, \
             but the rebuild history window will be reduced."
        );
    }
    if args.purge_invalidated {
        eprintln!(
            "NOTE: --purge-invalidated will permanently delete retired (invalidated) memory \
             rows. They will no longer appear in audit/blame output."
        );
    }

    let report = project::compact_brain(&workspace, trim_dur, args.purge_invalidated)?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    // Human-readable output.
    let freed = report.bytes_before.saturating_sub(report.bytes_after);
    println!(
        "compacted brain.db: {} → {} (freed {})",
        fmt_bytes(report.bytes_before),
        fmt_bytes(report.bytes_after),
        fmt_bytes(freed),
    );
    if report.invalidated_memories_purged > 0 {
        println!(
            "  purged {} invalidated memor{} (removed from audit trail)",
            report.invalidated_memories_purged,
            if report.invalidated_memories_purged == 1 {
                "y"
            } else {
                "ies"
            }
        );
    }
    if report.events_trimmed > 0 {
        println!(
            "  trimmed {} old event{} (rebuild history reduced)",
            report.events_trimmed,
            if report.events_trimmed == 1 { "" } else { "s" }
        );
    }
    Ok(())
}

// ── Q5: brain export / import ────────────────────────────────────────────────

/// `kimetsu brain export <file> [--scope] [--kind]`
///
/// Dumps active memories as pretty-printed JSON. Writes to stdout when
/// `file` is `-`.  Prints "exported N memories to <file>" on success.
pub(crate) fn brain_export(args: BrainExportArgs) -> KimetsuResult<()> {
    let workspace = args
        .workspace
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());

    // Parse optional scope/kind filters.
    let scope = args
        .scope
        .as_deref()
        .map(|s| {
            s.parse::<MemoryScope>().map_err(|_| {
                format!("unknown scope `{s}`; expected one of: global_user, project, repo, run")
            })
        })
        .transpose()?;
    let kind = args
        .kind
        .as_deref()
        .map(|k| {
            k.parse::<MemoryKind>()
                .map_err(|_| format!("unknown kind `{k}`; expected one of: preference, convention, command, failure_pattern, fact"))
        })
        .transpose()?;

    let (memories, scrub) =
        project::export_memories(&workspace, scope, kind, args.redact, args.redact_tags)?;

    // Security scrub report (always runs). --strict refuses to ship a finding.
    if !scrub.is_clean() {
        if args.strict {
            return Err(format!(
                "brain export --strict: {} — fix the source memories or drop --strict",
                scrub.summary()
            )
            .into());
        }
        eprintln!("kimetsu: export security scrub — {}", scrub.summary());
    }

    let count = memories.len();
    // Pack envelope when any manifest flag is set; else the bare array.
    let is_pack = args.name.is_some() || args.version.is_some() || args.description.is_some();
    let json = if is_pack {
        let pack = project::Pack {
            kimetsu_pack: 1,
            name: args.name.clone(),
            version: args.version.clone(),
            description: args.description.clone(),
            exported_at: Some(
                time::OffsetDateTime::now_utc()
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_default(),
            ),
            memory_count: memories.len(),
            memories,
        };
        serde_json::to_string_pretty(&pack)
    } else {
        serde_json::to_string_pretty(&memories)
    }
    .map_err(|e| format!("brain export: failed to serialize: {e}"))?;

    if args.file == "-" {
        // stdout: emit plain JSON (piping a gzip stream to a terminal is useless).
        println!("{json}");
    } else {
        // Packs are ALWAYS gzip-compressed on disk (JSON can get large).
        let gz =
            gzip_bytes(json.as_bytes()).map_err(|e| format!("brain export: gzip failed: {e}"))?;
        std::fs::write(&args.file, &gz)
            .map_err(|e| format!("brain export: could not write `{}`: {e}", args.file))?;
        println!(
            "exported {count} memories to {} ({} bytes, gzip{})",
            args.file,
            gz.len(),
            if is_pack { ", pack" } else { "" }
        );
    }

    Ok(())
}

/// gzip-compress `data` (flate2, default compression).
pub(crate) fn gzip_bytes(data: &[u8]) -> std::io::Result<Vec<u8>> {
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    enc.write_all(data)?;
    enc.finish()
}

/// gunzip `data` when it carries the gzip magic (`1f 8b`); else return it as-is
/// (back-compat with old plain-JSON exports). Returns the decoded UTF-8 string.
pub(crate) fn maybe_gunzip_to_string(data: &[u8]) -> KimetsuResult<String> {
    if data.len() >= 2 && data[0] == 0x1f && data[1] == 0x8b {
        use flate2::read::GzDecoder;
        use std::io::Read;
        let mut out = String::new();
        GzDecoder::new(data)
            .read_to_string(&mut out)
            .map_err(|e| format!("gunzip failed: {e}"))?;
        Ok(out)
    } else {
        String::from_utf8(data.to_vec()).map_err(|e| format!("pack is not UTF-8: {e}").into())
    }
}

/// `kimetsu brain import <file> [--scope-override]`
///
/// Reads a JSON array of `MemoryExport` records (produced by `brain export`)
/// and imports them into the brain. Prints "imported N (deduped M)".
/// Reads from stdin when `file` is `-`.
pub(crate) fn brain_import(args: BrainImportArgs) -> KimetsuResult<()> {
    let workspace = args
        .workspace
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());

    // Parse optional scope_override.
    let scope_override = args
        .scope_override
        .as_deref()
        .map(|s| {
            s.parse::<MemoryScope>().map_err(|_| {
                format!("unknown scope `{s}`; expected one of: global_user, project, repo, run")
            })
        })
        .transpose()?;

    // Mode.
    let replace = match args.mode.as_str() {
        "merge" => false,
        "replace" => {
            if !args.yes {
                return Err(
                    "brain import --mode replace will SUPERSEDE your current memories \
                            in the pack's scope(s) (reversible) — re-run with --yes to confirm"
                        .into(),
                );
            }
            true
        }
        other => {
            return Err(format!("brain import: unknown --mode `{other}` (merge|replace)").into());
        }
    };

    // v2.6: whether the pack is held for review rather than entering
    // retrieval. The default is decided by where the pack came from, because
    // that is what the threat model turns on: a URL is content authored
    // elsewhere by someone else, fetched over the network, which `trust.rs`
    // names as the widest attack surface Kimetsu has. A local file is one the
    // user chose and can open. Either default is overridable.
    let from_url = args.file.starts_with("http://") || args.file.starts_with("https://");
    let quarantine = if args.quarantine {
        true
    } else if args.no_quarantine {
        false
    } else {
        from_url
    };
    if quarantine && replace {
        return Err(
            "brain import: --mode replace and --quarantine are incompatible — replace \
             supersedes memories you have in favour of content you have not reviewed. \
             Review the pack first, or pass --no-quarantine to accept it outright."
                .into(),
        );
    }

    // Read raw bytes from a path, stdin (`-`), or an http(s):// URL.
    let bytes: Vec<u8> = if args.file == "-" {
        use std::io::Read;
        let mut buf = Vec::new();
        std::io::stdin()
            .read_to_end(&mut buf)
            .map_err(|e| format!("brain import: failed to read stdin: {e}"))?;
        buf
    } else if args.file.starts_with("http://") || args.file.starts_with("https://") {
        let resp = reqwest::blocking::get(&args.file)
            .map_err(|e| format!("brain import: fetch {} failed: {e}", args.file))?;
        if !resp.status().is_success() {
            return Err(format!(
                "brain import: {} returned HTTP {}",
                args.file,
                resp.status()
            )
            .into());
        }
        resp.bytes()
            .map_err(|e| format!("brain import: read body failed: {e}"))?
            .to_vec()
    } else {
        std::fs::read(&args.file)
            .map_err(|e| format!("brain import: could not read `{}`: {e}", args.file))?
    };

    // Decompress if gzip; then parse a Pack envelope OR a bare array (back-compat).
    let json = maybe_gunzip_to_string(&bytes)?;
    let (pack_ref, entries) = project::parse_pack_or_array(&json)
        .map_err(|e| format!("brain import: `{}`: {e}", args.file))?;

    let summary = project::import_pack(
        &workspace,
        &entries,
        scope_override,
        replace,
        Some(&pack_ref),
        quarantine,
    )?;

    let label = match (&pack_ref.name, &pack_ref.version) {
        (Some(n), Some(v)) => format!(" (pack {n}@{v})"),
        (Some(n), None) => format!(" (pack {n})"),
        _ => String::new(),
    };
    if quarantine {
        println!(
            "quarantined {}, deduped {}{label}",
            summary.quarantined, summary.deduped
        );
        if summary.quarantined > 0 {
            println!(
                "Nothing from this pack can reach a session until you accept it. \
                 Review with `kimetsu brain memory proposals`{}.",
                if from_url && !args.quarantine {
                    " (quarantined because it came from a URL; pass --no-quarantine to skip review)"
                } else {
                    ""
                }
            );
        }
    } else if summary.superseded > 0 {
        println!(
            "installed {}, deduped {}, superseded {}{label}",
            summary.imported, summary.deduped, summary.superseded
        );
    } else {
        println!(
            "installed {}, deduped {}{label}",
            summary.imported, summary.deduped
        );
    }

    Ok(())
}

/// `kimetsu brain backup [<file>] [--workspace <p>]`
///
/// Writes a consistent full-DB snapshot of brain.db via the SQLite online
/// backup API. Complements `brain export` (memories-only JSON) and the
/// automatic pre-migrate backup — this is a full-schema snapshot you can
/// copy back as a restore.
pub(crate) fn brain_backup(args: BrainBackupArgs) -> KimetsuResult<()> {
    let workspace = args
        .workspace
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());
    let paths = kimetsu_core::paths::ProjectPaths::discover(&workspace)?;

    if !paths.brain_db.exists() {
        return Err(format!(
            "brain.db not found at {} — run `kimetsu init` first",
            paths.brain_db.display()
        )
        .into());
    }

    let dest = args.file.as_deref();
    let (dest_path, size) = kimetsu_brain::migrate::backup_brain(&paths.brain_db, dest)?;
    println!(
        "backed up brain.db ({}) → {}",
        fmt_bytes_brain(size),
        dest_path.display()
    );
    Ok(())
}

// ── S3: brain sync ────────────────────────────────────────────────────────────

/// Slice B: best-effort full sync at session end (push + pull + converge) when a
/// `[sync] dir` is configured and `[sync] auto` is not disabled. Never returns an
/// error — a sync failure must not break session shutdown.
pub(crate) fn auto_sync_at_session_end(workspace: &Path) {
    use kimetsu_brain::sync as brain_sync_mod;
    use kimetsu_core::paths::ProjectPaths;

    let Ok(paths) = ProjectPaths::discover(workspace) else {
        return;
    };
    let Ok((_paths, config, conn)) = project::load_project(workspace) else {
        return;
    };
    let sync_cfg = &config.sync;
    let Some(dir) = sync_cfg.dir.as_deref() else {
        return; // not configured
    };
    if !sync_cfg.auto {
        return; // explicitly disabled
    }
    let machine_id = resolve_machine_id(&sync_cfg.machine_id);
    let cursors_path = paths.kimetsu_dir.join("sync-cursors.json");
    match brain_sync_mod::sync_dir(&conn, Path::new(dir), &machine_id, &cursors_path, false) {
        Ok(report) => {
            if report.pushed > 0 || report.pulled_applied > 0 {
                eprintln!(
                    "kimetsu: auto-synced (pushed {}, pulled {})",
                    report.pushed, report.pulled_applied
                );
            }
        }
        Err(e) => eprintln!("kimetsu: auto-sync skipped ({e})"),
    }
}

/// `kimetsu brain sync [subcommand] [flags]`
///
/// Dispatches to export / import / full-cycle / status based on `args.subcommand`
/// and flag combination.
pub(crate) fn brain_sync(args: SyncArgs) -> KimetsuResult<()> {
    use kimetsu_brain::sync as brain_sync_mod;
    use kimetsu_core::paths::ProjectPaths;

    let workspace = args
        .workspace
        .clone()
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());
    let paths = ProjectPaths::discover(&workspace)?;

    // Open brain.db (read-write for import/sync; read-only for export/status).
    let (_paths, config, conn) = project::load_project(&workspace)?;

    let sub = args.subcommand.as_deref().unwrap_or("");

    match sub {
        "export" => {
            // kimetsu brain sync export [--since <rowid>] [--out <file>] [--dry-run]
            let out_path = args.out.as_deref().map(std::path::Path::new);
            let (summary, content) =
                brain_sync_mod::export_events(&conn, args.since, out_path, args.dry_run)?;
            if let Some(jsonl) = content {
                println!("{jsonl}");
            } else if args.dry_run {
                println!(
                    "dry-run: would export {} events (next cursor: {})",
                    summary.exported, summary.next_cursor
                );
            } else {
                println!(
                    "exported {} events → {} (next cursor: {})",
                    summary.exported,
                    args.out.as_deref().unwrap_or("<stdout>"),
                    summary.next_cursor
                );
            }
        }
        "import" => {
            // kimetsu brain sync import <batch> [--dry-run]
            let batch_file = args.batch.as_deref().ok_or_else(|| {
                "kimetsu brain sync import: missing <batch> file argument".to_string()
            })?;
            let path = std::path::Path::new(batch_file);
            let summary = brain_sync_mod::import_events_from_file(&conn, path, args.dry_run)?;
            if args.dry_run {
                println!(
                    "dry-run: would apply {} events, skip {} (already present)",
                    summary.applied, summary.skipped
                );
            } else {
                // Slice B: total-order replay so the projection converges in HLC
                // order (the import applied events incrementally in file order).
                if summary.applied > 0 {
                    kimetsu_brain::projector::rebuild_in_place(&conn)?;
                }
                println!(
                    "applied {} events, skipped {}",
                    summary.applied, summary.skipped
                );
            }
        }
        "" => {
            // Full directory-protocol sync, or --status.
            if args.status {
                // 3.3 doctor: show sync state.
                let sync_cfg = &config.sync;
                let sync_dir_opt = sync_cfg.dir.as_deref().map(std::path::Path::new);
                let machine_id = resolve_machine_id(&sync_cfg.machine_id);
                let cursors_path = paths.kimetsu_dir.join("sync-cursors.json");
                let status =
                    brain_sync_mod::sync_status(&conn, sync_dir_opt, &machine_id, &cursors_path)?;
                println!("sync status:");
                println!(
                    "  dir:        {}",
                    status
                        .sync_dir
                        .as_deref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "(not configured)".to_string())
                );
                println!("  machine_id: {machine_id}");
                println!("  local pending (unpushed): {}", status.local_pending);
                let conflicts = brain_sync_mod::sync_conflict_count(&conn).unwrap_or(0);
                if conflicts > 0 {
                    println!(
                        "  ⚠ supersede conflicts: {conflicts} (concurrent edits chose different \
                         survivors; review with `kimetsu brain memory conflicts`)"
                    );
                }
                if status.sources.is_empty() {
                    println!("  peers: (none seen yet)");
                } else {
                    println!("  peers:");
                    for (mid, cursor, pending) in &status.sources {
                        println!("    {mid}: cursor={cursor}, pending_pull={pending}");
                    }
                }
            } else {
                // Full sync cycle.
                let sync_cfg = &config.sync;
                let sync_dir = match sync_cfg.dir.as_deref() {
                    Some(d) if !d.is_empty() => std::path::PathBuf::from(d),
                    _ => {
                        return Err(
                            "kimetsu brain sync: `[sync] dir` is not configured in project.toml.\n\
                             Set it with: kimetsu config set sync.dir /path/to/shared/dir"
                                .to_string()
                                .into(),
                        );
                    }
                };
                let machine_id = resolve_machine_id(&sync_cfg.machine_id);
                let cursors_path = paths.kimetsu_dir.join("sync-cursors.json");
                let report = brain_sync_mod::sync_dir(
                    &conn,
                    &sync_dir,
                    &machine_id,
                    &cursors_path,
                    args.dry_run,
                )?;
                let prefix = if report.dry_run { "dry-run: " } else { "" };
                println!(
                    "{prefix}pushed {pushed}, pulled {applied} (skipped {skipped}) from {n} peer(s)",
                    pushed = report.pushed,
                    applied = report.pulled_applied,
                    skipped = report.pulled_skipped,
                    n = report.machines_pulled.len(),
                    prefix = prefix,
                );
            }
        }
        other => {
            return Err(format!(
                "kimetsu brain sync: unknown subcommand `{other}`; \
                 expected `export`, `import`, or omit for full sync"
            )
            .into());
        }
    }

    Ok(())
}

/// Resolve the effective machine_id: use the configured value if non-empty,
/// otherwise generate a stable ULID-based id.  The generated id is NOT
/// persisted here — the user should run `kimetsu config set sync.machine_id
/// <id>` to make it durable.
/// Resolve this process's event write origin `<machine>/<agent>` from the
/// environment. Machine: `KIMETSU_SYNC_MACHINE_ID`, else `COMPUTERNAME`/`HOSTNAME`.
/// Agent: `KIMETSU_AGENT_ID` (hosts/hooks set it), else the invoked subcommand,
/// else `cli`. Returns `None` when no machine id is resolvable (origin stays
/// unknown/NULL — best-effort, never fatal).
pub(crate) fn resolve_process_origin() -> Option<String> {
    let machine = std::env::var("KIMETSU_SYNC_MACHINE_ID")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("COMPUTERNAME").ok().filter(|s| !s.is_empty()))
        .or_else(|| std::env::var("HOSTNAME").ok().filter(|s| !s.is_empty()))?;
    let agent = std::env::var("KIMETSU_AGENT_ID")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::args().nth(1).filter(|s| !s.starts_with('-')))
        .unwrap_or_else(|| "cli".to_string());
    Some(format!("{machine}/{agent}"))
}

pub(crate) fn resolve_machine_id(configured: &str) -> String {
    if !configured.is_empty() {
        return configured.to_string();
    }
    // Stable fallback: use hostname or a generated ULID.
    std::env::var("KIMETSU_SYNC_MACHINE_ID")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| ulid::Ulid::new().to_string())
}

// ── embed-daemon / warm / daemon subcommand handlers ─────────────────────────

#[cfg(feature = "embeddings")]
pub(crate) fn brain_embed_daemon(args: EmbedDaemonArgs) -> KimetsuResult<()> {
    use embed_daemon::server::{DaemonState, serve_with_listener};
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;
    use std::time::Instant;

    // Bind BEFORE loading any model. A redundant spawn (a live daemon already
    // owns the socket — AddrInUse / PermissionDenied / AlreadyExists are the
    // Windows race variants) must exit in milliseconds, not after a
    // multi-second model load: the doomed child inherits the spawning hook's
    // stdio handles, so while it lives the hook's CALLER (the harness hook
    // runner) is stalled waiting for stdout to close.
    let listener = match embed_daemon::ipc::listen(&args.model) {
        Ok(l) => l,
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::AddrInUse
                    | std::io::ErrorKind::PermissionDenied
                    | std::io::ErrorKind::AlreadyExists
            ) =>
        {
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };

    let t0 = Instant::now();
    let embedder = kimetsu_brain::embeddings::open_embedder_for_model(&args.model);
    let reranker = kimetsu_brain::embeddings::open_reranker_for_model(&args.reranker);
    let loaded_ms = t0.elapsed().as_millis() as u64;
    let state = Arc::new(DaemonState {
        embedder,
        reranker,
        model: args.model,
        started: Instant::now(),
        loaded_ms,
        requests: AtomicU64::new(0),
    });
    serve_with_listener(listener, state).map_err(Into::into)
}

#[cfg(feature = "embeddings")]
pub(crate) fn brain_warm() -> KimetsuResult<()> {
    let workspace = env::current_dir().unwrap_or_default();
    // warm_on_start gate: only PRE-warm at startup when configured to. When
    // false the daemon still warms lazily on the first prompt (via the hook's
    // ensure-spawn path) — this only suppresses the SessionStart pre-warm.
    if let Ok(paths) = kimetsu_core::paths::ProjectPaths::discover(&workspace)
        && let Ok(config) = project::load_config(&paths)
        && !config.embedder.warm_on_start
    {
        return Ok(());
    }
    let Some(model) = resolve_daemon_model(&workspace) else {
        return Ok(());
    };
    let reranker = resolve_daemon_reranker(&workspace);
    embed_daemon::client::ensure_daemon(&model, &reranker);
    Ok(())
}

#[cfg(feature = "embeddings")]
pub(crate) fn brain_daemon(args: DaemonArgs) -> KimetsuResult<()> {
    use embed_daemon::{client, proto};
    let workspace = env::current_dir().unwrap_or_default();
    let model = resolve_daemon_model(&workspace)
        .unwrap_or_else(|| kimetsu_brain::embeddings::resolve_embedder_id(None).to_string());
    match args.command {
        DaemonCommand::Status => match client::request(&model, proto::Request::Ping) {
            Some(proto::Response::Info {
                version,
                model,
                uptime_s,
                requests,
                loaded_ms,
            }) => {
                println!(
                    "running: model={model} version={version} uptime={uptime_s}s requests={requests} load={loaded_ms}ms"
                );
                Ok(())
            }
            _ => {
                println!("not running");
                Ok(())
            }
        },
        DaemonCommand::Stop => {
            let _ = client::request(&model, proto::Request::Shutdown);
            println!("stop requested");
            Ok(())
        }
    }
}

/// Resolve the daemon model id from config, honoring the kill switches.
/// Returns `None` when the daemon must not be used.
#[cfg(feature = "embeddings")]
pub(crate) fn resolve_daemon_model(workspace: &std::path::Path) -> Option<String> {
    if std::env::var("KIMETSU_EMBED_DAEMON").as_deref() == Ok("0") {
        return None;
    }
    let paths = kimetsu_core::paths::ProjectPaths::discover(workspace).ok()?;
    let config = project::load_config(&paths).ok()?;
    if !config.embedder.enabled || !config.embedder.daemon {
        return None;
    }
    Some(
        kimetsu_brain::embeddings::resolve_embedder_id(Some(config.embedder.model.as_str()))
            .to_string(),
    )
}

/// Resolve the reranker id from config. Falls back to `"off"` when config is
/// unreadable so the daemon stays functional without a reranker.
#[cfg(feature = "embeddings")]
pub(crate) fn resolve_daemon_reranker(workspace: &std::path::Path) -> String {
    let Ok(paths) = kimetsu_core::paths::ProjectPaths::discover(workspace) else {
        return "off".to_string();
    };
    let Ok(config) = project::load_config(&paths) else {
        return "off".to_string();
    };
    config.embedder.reranker
}

/// Try semantic retrieval via the warm daemon. Returns `None` (-> FTS fallback)
/// when embeddings aren't built, the daemon is disabled by config/env, or the
/// daemon is unreachable within the client budget. On a miss it also kicks off
/// a detached spawn so the NEXT prompt finds a warm daemon.
#[cfg(feature = "embeddings")]
pub(crate) fn try_daemon_retrieve(
    workspace: &std::path::Path,
    request: &kimetsu_brain::context::ContextRequest,
) -> Option<kimetsu_brain::context::ContextBundle> {
    use embed_daemon::{client, proto};
    // v2.6: an ordering query needs each memory's `created_at`, and the wire
    // protocol carries summary/kind/score only — no handle to look a date up
    // by. Rather than widen the protocol for a rare query shape, decline the
    // daemon entirely and let the caller fall back to the in-process path,
    // which has the dates and renders them. Semantic ranking is worth less
    // here than the dates are: "which came first" is answered by the ordering,
    // not by which memory ranks top.
    if kimetsu_brain::ordering::is_ordering_query(&request.query) {
        return None;
    }
    let model = resolve_daemon_model(workspace)?;
    let args = proto::RetrieveArgs {
        v: proto::PROTOCOL_VERSION,
        brain_root: workspace.to_string_lossy().into_owned(),
        query: request.query.clone(),
        stage: request.stage.clone(),
        budget_tokens: request.budget_tokens,
        max_capsules: request.max_capsules,
        min_score: request.min_score,
        tags: request.tags.clone(),
    };
    match client::request(&model, proto::Request::Retrieve(args)) {
        Some(proto::Response::Capsules {
            capsules,
            skipped,
            top_score,
        }) => Some(daemon_capsules_to_bundle(
            workspace, request, capsules, skipped, top_score,
        )),
        _ => {
            // Unreachable/errored: we already know it didn't answer, so spawn
            // directly (no second ping) to keep within the single 300ms budget.
            // A duplicate spawn loses the OS single-instance race and exits.
            let reranker = resolve_daemon_reranker(workspace);
            let _ = client::spawn_daemon(&model, &reranker);
            None
        }
    }
}

#[cfg(not(feature = "embeddings"))]
pub(crate) fn try_daemon_retrieve(
    _workspace: &std::path::Path,
    _request: &kimetsu_brain::context::ContextRequest,
) -> Option<kimetsu_brain::context::ContextBundle> {
    None
}

/// Adapt the wire capsule list back into a `ContextBundle` for the existing
/// rendering code path.
#[cfg(feature = "embeddings")]
pub(crate) fn daemon_capsules_to_bundle(
    workspace: &std::path::Path,
    request: &kimetsu_brain::context::ContextRequest,
    capsules: Vec<embed_daemon::proto::Capsule>,
    skipped: bool,
    top_score: f32,
) -> kimetsu_brain::context::ContextBundle {
    use kimetsu_brain::context::{ContextBundle, ContextCapsule};
    let capsules: Vec<ContextCapsule> = capsules
        .into_iter()
        .map(|c| ContextCapsule::wire_minimal(c.summary, c.kind, c.score))
        .collect();
    // v2.6: measure coverage here too. The in-process path does it during
    // finalization, which this path skips — so without this the "memory does
    // not cover X" line was dead on exactly the builds that run a daemon.
    // A skipped bundle covers nothing, matching the in-process rule.
    let (evidence_coverage, uncovered_terms) = if skipped {
        (0.0, Vec::new())
    } else {
        project::evidence_coverage_readonly(workspace, &request.query, &capsules)
    };
    ContextBundle {
        stage: request.stage.clone(),
        budget_tokens: request.budget_tokens,
        used_tokens: 0,
        capsules,
        excluded: Vec::new(),
        skipped,
        top_score,
        // The daemon wire protocol does not carry the absolute-evidence
        // signal; 0.0 here means "unreported", not "no evidence".
        top_abs_evidence: 0.0,
        evidence_coverage,
        uncovered_terms,
        // Ordering queries never reach the daemon (`try_daemon_retrieve`
        // declines them), so a bundle from here is never time-ordered.
        chronological: false,
    }
}

// ── Lean (no embeddings) stubs ───────────────────────────────────────────────
#[cfg(not(feature = "embeddings"))]
pub(crate) fn brain_embed_daemon(_args: EmbedDaemonArgs) -> KimetsuResult<()> {
    eprintln!("kimetsu: embeddings not built — no daemon");
    Ok(())
}
#[cfg(not(feature = "embeddings"))]
pub(crate) fn brain_warm() -> KimetsuResult<()> {
    Ok(())
}
#[cfg(not(feature = "embeddings"))]
pub(crate) fn brain_daemon(_args: DaemonArgs) -> KimetsuResult<()> {
    println!("not running (embeddings not built)");
    Ok(())
}

/// Format a byte count as a human-readable string for the brain backup output.
pub(crate) fn fmt_bytes_brain(n: u64) -> String {
    if n < 1_024 {
        format!("{n} B")
    } else if n < 1_024 * 1_024 {
        format!("{:.1} KB", n as f64 / 1_024.0)
    } else {
        format!("{:.1} MB", n as f64 / (1_024.0 * 1_024.0))
    }
}

/// v0.6: `kimetsu brain status` — brain health at a glance.
pub(crate) fn brain_status(json: bool) -> KimetsuResult<()> {
    let cwd = env::current_dir()?;
    let schema_ver = project::schema_version(&cwd)?;
    let memories = project::list_memories(&cwd)?;
    let proposals = project::list_proposals(
        &cwd,
        project::ProposalFilter {
            status: Some("pending".to_string()),
            limit: 200,
            ..Default::default()
        },
    )?;
    let conflicts = project::list_conflicts(&cwd, 200)?;

    let healthy: Vec<_> = memories
        .iter()
        .filter(|m| m.usefulness_score >= 0.2)
        .collect();
    let fading: Vec<_> = memories
        .iter()
        .filter(|m| m.usefulness_score >= 0.0 && m.usefulness_score < 0.2)
        .collect();
    let stale: Vec<_> = memories
        .iter()
        .filter(|m| m.usefulness_score < 0.0)
        .collect();

    // Domain grouping: extract first [tags: ...] prefix from text
    let mut domain_counts: std::collections::BTreeMap<String, usize> = Default::default();
    for m in &memories {
        let domain = if let Some(rest) = m.text.strip_prefix("[tags: ") {
            rest.split(']')
                .next()
                .unwrap_or("other")
                .split_whitespace()
                .next()
                .unwrap_or("other")
                .to_string()
        } else {
            m.kind.clone()
        };
        *domain_counts.entry(domain).or_insert(0) += 1;
    }
    let mut domain_list: Vec<(String, usize)> = domain_counts.into_iter().collect();
    domain_list.sort_by_key(|b| std::cmp::Reverse(b.1));
    let top_domains: Vec<String> = domain_list
        .iter()
        .take(6)
        .map(|(d, n)| format!("{} ({})", d, n))
        .collect();

    // F3 Stories 3.2 & 3.4: regret-flagged memories + invalidations by reason.
    // v2.6: also report the resolved tier — which pipeline the numbers above
    // were produced by is not something a user should have to infer.
    let (regret_flagged, inv_by_reason, tier, tier_downgraded) = match project::load_project(&cwd) {
        Ok((_paths, config, conn)) => {
            let threshold = config.lifecycle.regret_flag_threshold;
            let regret = kimetsu_brain::lifecycle::regret_flagged_memories(&conn, threshold)
                .map(|v| v.len())
                .unwrap_or(0);
            let inv = kimetsu_brain::lifecycle::invalidations_by_reason(&conn).unwrap_or_default();
            (regret, inv, config.tier(), config.tier_downgraded())
        }
        Err(_) => (0, vec![], kimetsu_core::config::Tier::Free, false),
    };

    if json {
        let inv_json: serde_json::Value = inv_by_reason
            .iter()
            .map(|r| (r.reason.clone(), serde_json::json!(r.count)))
            .collect::<serde_json::Map<_, _>>()
            .into();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "schema_version": schema_ver,
                "memories": memories.len(),
                "pending_proposals": proposals.len(),
                "open_conflicts": conflicts.len(),
                "healthy": healthy.len(),
                "fading": fading.len(),
                "stale": stale.len(),
                "top_domains": top_domains,
                "regret_flagged": regret_flagged,
                "invalidations_by_reason": inv_json,
                "tier": tier.as_str(),
                "tier_downgraded": tier_downgraded,
            }))?
        );
    } else {
        println!(
            "brain: {} memories active, {} pending proposals, {} conflicts",
            memories.len(),
            proposals.len(),
            conflicts.len()
        );
        println!("schema version: {schema_ver}");
        match tier {
            kimetsu_core::config::Tier::Free => {
                println!("tier:    free (no LLM calls in the memory pipeline)")
            }
            kimetsu_core::config::Tier::Deep => {
                println!("tier:    deep (a local/cheap model assists ingest and retrieval)")
            }
        }
        if tier_downgraded {
            println!(
                "hint: tier = \"deep\" is configured but no cheap model is reachable — \
                 running free. See `kimetsu doctor`."
            );
        }
        if !top_domains.is_empty() {
            println!("domains: {}", top_domains.join(", "));
        }
        println!("health:  {} healthy (usefulness >= 0.2)", healthy.len());
        println!("         {} fading  (0 <= usefulness < 0.2)", fading.len());
        println!(
            "         {} stale   (usefulness < 0, candidate for prune)",
            stale.len()
        );
        if stale.len() > 3 {
            println!("hint: run `kimetsu brain memory prune` to clean stale entries");
        }
        if regret_flagged > 0 {
            println!(
                "regret:  {} memor{} flagged for review (cited despite being dropped)",
                regret_flagged,
                if regret_flagged == 1 { "y" } else { "ies" }
            );
            println!("hint: run `kimetsu brain forget --dry-run` to review lifecycle candidates");
        }
        if !inv_by_reason.is_empty() {
            let parts: Vec<String> = inv_by_reason
                .iter()
                .map(|r| format!("{}: {}", r.reason, r.count))
                .collect();
            println!("invalidations by reason: {}", parts.join(", "));
        }
    }
    Ok(())
}

/// v1.0 (C5): `kimetsu brain insights` — effectiveness analytics.
pub(crate) fn brain_insights(
    json: bool,
    last_n_runs: u32,
    since: Option<String>,
    top: u32,
) -> KimetsuResult<()> {
    use kimetsu_brain::analytics::{self, InsightsOptions};

    let cwd = env::current_dir()?;
    let opts = InsightsOptions {
        last_n_runs,
        since,
        top_n: top,
    };
    let report = analytics::compute_insights(&cwd, opts)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        // --- Retrieval ---
        let hit_rate = report
            .retrieval
            .hit_rate
            .map(|v| format!("{:.1}%", v * 100.0))
            .unwrap_or_else(|| "n/a".to_string());
        let avg_score = report
            .retrieval
            .avg_top_score
            .map(|v| format!("{:.3}", v))
            .unwrap_or_else(|| "n/a".to_string());
        println!("── Retrieval ──────────────────────────────────");
        println!("  served:       {}", report.retrieval.served);
        println!("  hit-rate:     {hit_rate}");
        println!("  avg-top-score:{avg_score}");

        // --- Citation ---
        let citation_rate = report
            .citation
            .citation_rate
            .map(|v| format!("{:.1}%", v * 100.0))
            .unwrap_or_else(|| "n/a".to_string());
        println!("── Citation ───────────────────────────────────");
        println!("  runs-considered: {}", report.citation.runs_considered);
        println!("  retrieved:       {}", report.citation.retrieved_total);
        println!("  cited:           {}", report.citation.cited_total);
        println!("  citation-rate:   {citation_rate}");

        // --- Proposals ---
        let acceptance_rate = report
            .proposals
            .acceptance_rate
            .map(|v| format!("{:.1}%", v * 100.0))
            .unwrap_or_else(|| "n/a".to_string());
        println!("── Proposals ──────────────────────────────────");
        println!("  accepted:        {}", report.proposals.accepted);
        println!("  rejected:        {}", report.proposals.rejected);
        println!("  pending:         {}", report.proposals.pending);
        println!("  acceptance-rate: {acceptance_rate}");

        // --- Usefulness ---
        let avg_ratio = report
            .usefulness
            .avg_ratio
            .map(|v| format!("{:.3}", v))
            .unwrap_or_else(|| "n/a".to_string());
        println!("── Usefulness Trend ───────────────────────────");
        println!(
            "  sum-usefulness:      {:.3}",
            report.usefulness.sum_usefulness
        );
        println!("  avg-ratio:           {avg_ratio}");
        println!(
            "  window-finished:     {}",
            report.usefulness.window_finished
        );
        println!(
            "  window-failed(non-gate): {}",
            report.usefulness.window_failed_nongate
        );
        println!("  window-net:          {}", report.usefulness.window_net);

        // --- Harvest ---
        let yield_per_run = report
            .harvest
            .yield_per_run
            .map(|v| format!("{:.2}", v))
            .unwrap_or_else(|| "n/a".to_string());
        println!("── Harvest ────────────────────────────────────");
        println!("  created-in-window: {}", report.harvest.created_in_window);
        println!("  yield-per-run:     {yield_per_run}");
        if !report.harvest.by_source.is_empty() {
            let sources: Vec<String> = report
                .harvest
                .by_source
                .iter()
                .map(|(src, n)| format!("{src}={n}"))
                .collect();
            println!("  by-source:         {}", sources.join(", "));
        }

        // --- Corpus ---
        println!("── Corpus Health ──────────────────────────────");
        println!("  active:           {}", report.corpus.active);
        println!("  invalidated:      {}", report.corpus.invalidated);
        println!("  open-conflicts:   {}", report.corpus.open_conflicts);
        println!("  pending-proposals:{}", report.corpus.pending_proposals);
        if !report.corpus.by_scope.is_empty() {
            let scopes: Vec<String> = report
                .corpus
                .by_scope
                .iter()
                .map(|(s, n)| format!("{s}={n}"))
                .collect();
            println!("  by-scope:         {}", scopes.join(", "));
        }
        if !report.corpus.by_kind.is_empty() {
            let kinds: Vec<String> = report
                .corpus
                .by_kind
                .iter()
                .map(|(k, n)| format!("{k}={n}"))
                .collect();
            println!("  by-kind:          {}", kinds.join(", "));
        }
        if !report.corpus.top_useful.is_empty() {
            println!("  top-useful:");
            for m in &report.corpus.top_useful {
                println!(
                    "    [{:.2}] {} — {}",
                    m.usefulness_score, m.memory_id, m.text_preview
                );
            }
        }
        if !report.corpus.prune_candidates.is_empty() {
            println!(
                "  prune-candidates ({}):",
                report.corpus.prune_candidates.len()
            );
            for m in &report.corpus.prune_candidates {
                println!(
                    "    [{:.2}] {} — {}",
                    m.usefulness_score, m.memory_id, m.text_preview
                );
            }
        }

        // --- Token Economy ---
        let avg_tokens = report
            .token_economy
            .avg_injected_tokens
            .map(|v| format!("{:.0}", v))
            .unwrap_or_else(|| "n/a".to_string());
        let avg_capsules = report
            .token_economy
            .avg_capsules
            .map(|v| format!("{:.2}", v))
            .unwrap_or_else(|| "n/a".to_string());
        let skip_rate = report
            .token_economy
            .skip_rate
            .map(|v| format!("{:.1}%", v * 100.0))
            .unwrap_or_else(|| "n/a".to_string());
        println!("── Token Economy ──────────────────────────────");
        println!("  avg-injected-tokens: {avg_tokens}");
        println!("  avg-capsules:        {avg_capsules}");
        println!("  skip-rate:           {skip_rate}");
    }
    Ok(())
}

/// v1.5 / S2.4: `kimetsu brain roi` — ROI ledger.
pub(crate) fn brain_roi(args: RoiArgs) -> KimetsuResult<()> {
    use kimetsu_brain::roi::{RoiWindow, per_memory_roi, roi_report};

    let workspace = args
        .workspace
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());

    let window = RoiWindow::parse(&args.window)
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;

    let (_paths, config, conn) = kimetsu_brain::project::load_project_readonly(&workspace)?;
    let report = roi_report(
        &conn,
        window,
        &config.model.model,
        config.model.price_per_mtok,
    )?;

    // S2.4(a): --top mode.
    if let Some(top_n) = args.top {
        let limit = if top_n == 0 { 10 } else { top_n };
        let entries = per_memory_roi(&conn, window, limit)?;

        if args.json {
            println!("{}", serde_json::to_string_pretty(&entries)?);
            return Ok(());
        }

        let window_label = match report.window_days {
            Some(d) => format!("last {d} days"),
            None => "all time".to_string(),
        };
        println!("── ROI Top Memories ({window_label}, top {limit}) ─────");
        if entries.is_empty() {
            println!("  No citations recorded yet.");
        } else {
            for (i, e) in entries.iter().enumerate() {
                println!(
                    "  #{:>2}  [{:>15}]  cites={:>3}  saved={:>6} tok  {}",
                    i + 1,
                    e.kind,
                    e.citation_count,
                    format_token_count(e.estimated_saved_tokens),
                    if e.text_head.len() >= 60 {
                        format!("{}…", &e.text_head[..60])
                    } else {
                        e.text_head.clone()
                    },
                );
            }
        }
        println!("──────────────────────────────────────────────");
        println!("  (Use without --top for the full ROI summary)");
        return Ok(());
    }

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    // Human output.
    let window_label = match report.window_days {
        Some(d) => format!("last {d} days"),
        None => "all time".to_string(),
    };
    println!("── ROI Ledger ({window_label}) ────────────────────────");
    println!("  served events:        {}", report.served_events);
    // S2.4(c): show warm-start events.
    if report.digest_served_events > 0 || report.resume_served_events > 0 {
        println!("  digest_served:        {}", report.digest_served_events);
        println!("  resume_served:        {}", report.resume_served_events);
        println!(
            "  warmstart saved tok:  {}",
            format_token_count(report.warmstart_saved_tokens)
        );
    }
    println!("  citations:            {}", report.citations);
    println!(
        "  injected tokens:      {}",
        format_token_count(report.injected_tokens)
    );
    // S2.4(b): output token estimate.
    println!(
        "  est. output tokens:   {} (ratio est.)",
        format_token_count(report.estimated_output_tokens)
    );
    println!(
        "  est. saved tokens:    {}",
        format_token_count(report.estimated_saved_tokens)
    );
    let net_sign = if report.net_tokens >= 0 { "+" } else { "" };
    println!("  net tokens:           {net_sign}{}", report.net_tokens);

    if let Some(ref usd) = report.usd {
        println!(
            "── USD ({} $/MTok) ─────────────────────────────",
            {
                // Reverse-lookup the price to show it.
                kimetsu_brain::roi::resolve_price_per_mtok(
                    &config.model.model,
                    config.model.price_per_mtok,
                )
                .map(|p| format!("{p:.2}"))
                .unwrap_or_else(|| "?".to_string())
            }
        );
        println!("  saved:  ${:.4}", usd.saved);
        println!("  spent:  ${:.4}", usd.spent);
        let net_usd_sign = if usd.net >= 0.0 { "+" } else { "" };
        println!("  net:    {net_usd_sign}${:.4}", usd.net);
    }

    // Verdict line.
    println!("──────────────────────────────────────────────");
    if report.citations == 0 && report.warmstart_saved_tokens == 0 {
        println!(
            "  No retrieval activity recorded yet — the ledger starts \
             counting as you work."
        );
    } else if report.net_tokens >= 0 {
        match &report.usd {
            Some(u) if u.net >= 0.0 => println!(
                "  Net positive: kimetsu saved you ~{} tokens (~${:.4}) this window.",
                format_token_count(report.estimated_saved_tokens),
                u.net,
            ),
            _ => println!(
                "  Net positive: kimetsu saved you ~{} tokens this window.",
                format_token_count(report.estimated_saved_tokens),
            ),
        }
    } else {
        // Honest negative.
        match &report.usd {
            Some(u) => println!(
                "  Net negative: brain overhead exceeded savings by ~{} tokens (~${:.4}) this window.",
                format_token_count(
                    report
                        .injected_tokens
                        .saturating_sub(report.estimated_saved_tokens)
                ),
                (u.spent - u.saved).abs(),
            ),
            None => println!(
                "  Net negative: brain overhead exceeded savings by ~{} tokens this window.",
                format_token_count(
                    report
                        .injected_tokens
                        .saturating_sub(report.estimated_saved_tokens)
                ),
            ),
        }
    }

    Ok(())
}

/// v1.5 / S2: `kimetsu brain tune` — personal eval readiness + optional sweep.
pub(crate) fn brain_tune(args: TuneArgs) -> KimetsuResult<()> {
    use kimetsu_brain::tune::{compute_model_advisor, compute_retune_trigger};
    use kimetsu_brain::tuneset::build_personal_eval;

    let workspace = args
        .workspace
        .clone()
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());
    let paths = kimetsu_core::paths::ProjectPaths::discover(&workspace)?;
    let (_paths2, config, conn) = kimetsu_brain::project::load_project_readonly(&workspace)?;

    if args.revert {
        return brain_tune_revert(&workspace);
    }

    // S2.2: --models only (no sweep).
    if args.models && !args.status {
        let trigger = compute_retune_trigger(&conn, &paths.kimetsu_dir)
            .map_err(|e| format!("compute_retune_trigger: {e}"))?;
        let advisor = compute_model_advisor(&config.embedder.model, &trigger);
        print_model_advisor(&advisor);
        return Ok(());
    }

    let eval = build_personal_eval(&conn, 1800).map_err(|e| format!("build_personal_eval: {e}"))?;

    let positive_count = eval.cases.len();
    let noise_count = eval.noise_count;

    let readiness = if positive_count >= 30 {
        "READY — enough cases for a meaningful sweep."
    } else {
        "accumulating — synthetic fixture will be used for the sweep (< 30 positive cases)."
    };

    // Coverage by memory kind (from relevant memory ids).
    let kind_coverage = kind_coverage_from_eval(&conn, &eval.cases);

    println!("=== kimetsu brain tune --status ===");
    println!("Positive cases (query + ≥1 cited memory): {positive_count}");
    println!("Noise entries  (served, no citation):     {noise_count}");
    if let Some(o) = &eval.oldest {
        println!("Oldest positive case: {o}");
    }
    if let Some(n) = &eval.newest {
        println!("Newest positive case: {n}");
    }
    println!();
    println!("Coverage by memory kind:");
    for (kind, count) in &kind_coverage {
        println!("  {kind:<22} {count}");
    }
    println!();
    println!("Readiness: {readiness}");

    // S2.1: always show trigger state in --status, or when --triggers flag used.
    if args.status || args.triggers {
        println!();
        let trigger = compute_retune_trigger(&conn, &paths.kimetsu_dir)
            .map_err(|e| format!("compute_retune_trigger: {e}"))?;
        print_retune_trigger_state(&trigger);

        // S2.2: show model advisor when --models is also set with --status.
        if args.models {
            println!();
            let advisor = compute_model_advisor(&config.embedder.model, &trigger);
            print_model_advisor(&advisor);
        }

        if args.status {
            return Ok(());
        }
    }

    // Sweep (or dry-run report).
    brain_tune_sweep(&workspace, &paths, args, eval)
}

pub(crate) fn kind_coverage_from_eval(
    conn: &rusqlite::Connection,
    cases: &[kimetsu_brain::eval::EvalCase],
) -> Vec<(String, usize)> {
    use std::collections::HashMap;
    let mut counts: HashMap<String, usize> = HashMap::new();
    for case in cases {
        for mid in &case.relevant {
            let kind: Option<String> = conn
                .query_row(
                    "SELECT kind FROM memories WHERE memory_id = ?1",
                    rusqlite::params![mid],
                    |r| r.get(0),
                )
                .ok();
            let kind = kind.unwrap_or_else(|| "unknown".to_string());
            *counts.entry(kind).or_default() += 1;
        }
    }
    let mut vec: Vec<(String, usize)> = counts.into_iter().collect();
    vec.sort_by_key(|a| std::cmp::Reverse(a.1));
    vec
}

/// S2.1: Print the re-tune trigger state in a human-readable format.
pub(crate) fn print_retune_trigger_state(trigger: &kimetsu_brain::tune::RetuneTriggerState) {
    println!("=== S2.1 Re-tune Triggers ===");
    if let Some(ts) = &trigger.last_tuned_at {
        println!("  Last tuned at:           {ts}");
        println!(
            "  Memory count at tune:    {}",
            trigger.memory_count_at_last_tune
        );
    } else {
        println!("  Last tuned at:           (never)");
    }
    println!(
        "  Current memory count:    {}",
        trigger.current_memory_count
    );
    println!(
        "  Added since last tune:   {}",
        trigger.memories_added_since_tune
    );
    println!(
        "  Corpus milestone (≥{}): {}",
        kimetsu_brain::tune::RETUNE_CORPUS_MILESTONE,
        if trigger.corpus_milestone_triggered {
            "TRIGGERED"
        } else {
            "not reached"
        }
    );
    println!(
        "  Regret rate (24h):       {:.1}% ({}/{} events)",
        trigger.regret_rate * 100.0,
        trigger.recent_regret_count,
        trigger.recent_served_count
    );
    println!(
        "  Drift threshold (≥{:.0}%): {}",
        kimetsu_brain::tune::RETUNE_REGRET_RATE_THRESHOLD * 100.0,
        if trigger.drift_triggered {
            "TRIGGERED"
        } else {
            "within normal"
        }
    );
    println!();
    if trigger.should_retune {
        println!("  → Re-tune PROPOSED: run `kimetsu brain tune` to run the sweep.");
    } else {
        println!("  → No re-tune needed at this time.");
    }
}

/// S2.2: Print the model re-selection advisor report.
pub(crate) fn print_model_advisor(advisor: &kimetsu_brain::tune::ModelAdvisorReport) {
    println!("=== S2.2 Model Re-selection Advisor ===");
    println!("  Current embedder:  {}", advisor.current_embedder);
    println!("  Memories to reindex: {}", advisor.memories_to_reindex);
    println!(
        "  Est. reindex cost:   ~{} tokens (conservative lower-bound)",
        format_token_count(advisor.estimated_reindex_tokens)
    );
    println!();
    println!("  {}", advisor.reason);
    println!();
    println!("  Candidate models (for grid sweep):");
    for m in &advisor.candidate_models {
        println!(
            "    {:<40} ~{} MiB download",
            m.model_id, m.approx_download_mib
        );
        println!("      {}", m.description);
    }
    println!();
    if advisor.recommend_grid_run {
        println!("  → Grid run RECOMMENDED. Re-run with the full sweep after downloading models.");
        println!("    NOTE: This advisor NEVER auto-switches the model. Apply changes manually.");
    } else {
        println!("  → Grid run optional. Current model appears sufficient.");
    }
}

pub(crate) fn brain_tune_sweep(
    workspace: &std::path::Path,
    paths: &kimetsu_core::paths::ProjectPaths,
    args: TuneArgs,
    eval: kimetsu_brain::tuneset::PersonalEval,
) -> KimetsuResult<()> {
    use kimetsu_brain::context::{ContextRequest, rerank_capsules};
    use kimetsu_brain::embeddings::{open_embedder_for, open_reranker_for_model};
    use kimetsu_brain::eval::{mean, mrr};
    use kimetsu_brain::project::BrainSession;
    use kimetsu_brain::tune::{
        ComboResult, TuneCombo, TuneHistoryEntry, append_tune_history,
        compute_objective_with_regret, count_regret_events, select_winner, train_holdout_split,
    };
    use std::collections::HashMap;
    use time::format_description::well_known::Rfc3339;

    let config = project::load_config(paths)?;
    // Tune against the PRODUCTION retrieval pipeline: the same embedder
    // resolution as retrieve_context_with_request. On embeddings builds this
    // loads the real model (semantic floors only discriminate with real
    // cosines); lean builds degrade to Noop and sweep FTS-only — the status
    // output should make that visible to the user.
    let embedder = open_embedder_for(config.embedder.enabled);
    if embedder.is_noop() {
        println!(
            "note: lean build/embedder disabled — sweeping FTS-only retrieval \
             (semantic floor values will not differentiate)"
        );
    }
    let current_combo = TuneCombo {
        min_lexical_coverage: config.broker.min_lexical_coverage,
        min_semantic_score: config.broker.min_semantic_score,
        reranker_id: config.embedder.reranker.clone(),
        fusion: config.broker.fusion.clone(),
    };

    // Choose eval cases: personal if READY, else fall back to fixture.
    let fallback_fixture_path = std::path::Path::new("fixtures/eval-retrieval.json");
    let (cases, using_personal) = if eval.cases.len() >= 30 {
        (eval.cases.clone(), true)
    } else {
        // Load the committed fixture.
        if !fallback_fixture_path.exists() {
            println!(
                "note: fewer than 30 personal eval cases ({}) and no fixture at {}. \
                 Sweep skipped. Accumulate more sessions with store_queries=true.",
                eval.cases.len(),
                fallback_fixture_path.display()
            );
            return Ok(());
        }
        let text = std::fs::read_to_string(fallback_fixture_path)
            .map_err(|e| format!("read fixture: {e}"))?;
        let fixture: kimetsu_brain::eval::EvalFixture =
            serde_json::from_str(&text).map_err(|e| format!("parse fixture: {e}"))?;
        // Fixture uses key-based relevance, not memory_ids. For the sweep
        // we need memory_ids. We cannot map them here (fixture is hermetic).
        // Instead: use fixture cases as-is for MRR calculation but note that
        // relevant ids won't match real DB memories → MRR will be 0.
        // The sweep is still meaningful for comparing COMBOS relatively.
        let eval_cases: Vec<kimetsu_brain::eval::EvalCase> = fixture
            .cases
            .into_iter()
            .map(|c| kimetsu_brain::eval::EvalCase {
                query: c.query,
                relevant: c.relevant,
                kind: Default::default(),
                stale: Vec::new(),
            })
            .collect();
        (eval_cases, false)
    };

    if !using_personal {
        println!(
            "note: fewer than 30 personal eval cases ({}). Using fixture file for relative sweep.",
            eval.cases.len()
        );
        // Fix 3: guard --apply behind personal data.
        // In fixture mode MRR≡0 for every combo (fixture IDs don't match real
        // memories), so the objective degenerates to pure token-minimisation.
        // Applying the resulting floors would optimise for fewer tokens at the
        // cost of recall.  Refuse --apply until the user has ≥30 cited cases.
        if args.apply {
            println!(
                "note: fixture mode is relative-only — --apply refused. \
                 Accumulate ≥30 cited cases first (see `kimetsu brain tune --status`)."
            );
            return Ok(());
        }
    }

    let n = cases.len();
    if n == 0 {
        println!("No eval cases available. Run more sessions with store_queries=true.");
        return Ok(());
    }

    let (train_idx, holdout_idx) = train_holdout_split(n);
    let train_cases: Vec<&kimetsu_brain::eval::EvalCase> =
        train_idx.iter().map(|&i| &cases[i]).collect();
    let holdout_cases: Vec<&kimetsu_brain::eval::EvalCase> =
        holdout_idx.iter().map(|&i| &cases[i]).collect();

    println!(
        "Sweep: {} combos × {} train / {} holdout cases",
        kimetsu_brain::tune::TuneCombo::all_combos().len(),
        train_cases.len(),
        holdout_cases.len()
    );

    // Cache reranker handles (load once, reuse).
    let mut reranker_cache: HashMap<String, Option<Box<dyn kimetsu_brain::embeddings::Reranker>>> =
        HashMap::new();
    for rr_id in kimetsu_brain::tune::RERANKER_IDS {
        let rr: Option<Box<dyn kimetsu_brain::embeddings::Reranker>> = if *rr_id == "off" {
            None
        } else {
            open_reranker_for_model(rr_id)
        };
        reranker_cache.insert(rr_id.to_string(), rr);
    }

    // Helper: evaluate one combo over a slice of cases.
    let evaluate_cases =
        |combo: &TuneCombo, case_slice: &[&kimetsu_brain::eval::EvalCase]| -> (f64, f64) {
            let session = match BrainSession::open_readonly(workspace) {
                Ok(s) => s,
                Err(_) => return (0.0, 0.0),
            };
            let rr_ref = reranker_cache
                .get(&combo.reranker_id)
                .and_then(|r| r.as_deref());
            let rerank_floor = 0.30f32;
            let rerank_cap = 4usize;
            let pool = 8usize;

            let mut mrr_vals: Vec<f64> = Vec::new();
            let mut token_vals: Vec<f64> = Vec::new();

            for case in case_slice {
                let request = ContextRequest {
                    stage: "localization".to_string(),
                    query: case.query.clone(),
                    budget_tokens: 6000,
                    max_capsules: pool,
                    min_semantic_score: combo.min_semantic_score,
                    min_lexical_coverage: combo.min_lexical_coverage,
                    fusion: combo.fusion.clone(),
                    ..Default::default()
                };
                let mut bundle =
                    match session.retrieve_context_with_injected_embedder(request, embedder) {
                        Ok(b) => b,
                        Err(_) => continue,
                    };
                if let Some(rr) = rr_ref {
                    bundle.capsules =
                        rerank_capsules(&case.query, bundle.capsules, rr, rerank_floor, rerank_cap);
                }

                let ranked_ids: Vec<String> = bundle
                    .capsules
                    .iter()
                    .filter_map(|c| {
                        c.expansion_handle
                            .strip_prefix("memory:")
                            .map(str::to_string)
                    })
                    .collect();

                let mrr_val = mrr(&ranked_ids, &case.relevant);
                mrr_vals.push(mrr_val);

                let tokens: f64 = bundle
                    .capsules
                    .iter()
                    .map(|c| c.token_estimate as f64)
                    .sum();
                token_vals.push(tokens);
            }

            (mean(&mrr_vals), mean(&token_vals))
        };

    // S2.3: Compute global regret rate from the DB for the objective penalty.
    // We use the ALL-TIME regret / served ratio here (the sweep window is the
    // full personal eval set, which spans all time).
    // Best-effort: if the DB cannot be opened, regret_rate and memory_count
    // degrade gracefully to 0 (objective falls back to v1.5 formula).
    let (global_regret_rate, current_memory_count) = {
        match kimetsu_brain::project::load_project_readonly(workspace) {
            Ok((_paths_ro, _cfg_ro, conn_ro)) => {
                let total_regrets = count_regret_events(&conn_ro, None, None).unwrap_or(0);
                let total_served: u64 = conn_ro
                    .query_row(
                        "SELECT COUNT(*) FROM events WHERE kind = 'context.served'",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap_or(0);
                let regret_rate = if total_served > 0 {
                    total_regrets as f64 / total_served as f64
                } else {
                    0.0
                };
                let mem_count: u64 = conn_ro
                    .query_row(
                        "SELECT COUNT(*) FROM memories WHERE invalidated_at IS NULL",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap_or(0);
                (regret_rate, mem_count)
            }
            Err(_) => (0.0_f64, 0_u64),
        }
    };

    // Evaluate current config on holdout for baseline.
    let (baseline_holdout_mrr, baseline_holdout_tokens) =
        evaluate_cases(&current_combo, &holdout_cases);
    let baseline_holdout_obj = compute_objective_with_regret(
        baseline_holdout_mrr,
        baseline_holdout_tokens,
        args.cost_weight,
        global_regret_rate,
    );

    // Sweep all combos on TRAIN set.
    let all_combos = TuneCombo::all_combos();
    let mut combo_results: Vec<ComboResult> = Vec::new();

    for (i, combo) in all_combos.iter().enumerate() {
        if i % 10 == 0 {
            print!("\r  sweeping combo {}/{} ...", i + 1, all_combos.len());
            use std::io::Write;
            let _ = std::io::stdout().flush();
        }
        let (mmrr, mtok) = evaluate_cases(combo, &train_cases);
        // S2.3: include regret penalty in the objective.
        let obj = compute_objective_with_regret(mmrr, mtok, args.cost_weight, global_regret_rate);
        combo_results.push(ComboResult {
            combo: combo.clone(),
            mean_mrr: mmrr,
            mean_tokens: mtok,
            objective: obj,
        });
    }
    println!();

    let winner = match select_winner(&combo_results) {
        Some(w) => w,
        None => {
            println!("No combos evaluated. Nothing to tune.");
            return Ok(());
        }
    };

    // Evaluate winner on HOLDOUT (with regret penalty for consistency).
    let (holdout_mrr, holdout_tokens) = evaluate_cases(&winner.combo, &holdout_cases);
    let holdout_obj = compute_objective_with_regret(
        holdout_mrr,
        holdout_tokens,
        args.cost_weight,
        global_regret_rate,
    );
    let improvement = holdout_obj - baseline_holdout_obj;

    println!();
    println!("=== Tune Sweep Results ===");
    println!(
        "Current config:  lex={:.2} sem={:.3} rr={}",
        current_combo.min_lexical_coverage,
        current_combo.min_semantic_score,
        current_combo.reranker_id
    );
    println!(
        "Best combo:      lex={:.2} sem={:.3} rr={}",
        winner.combo.min_lexical_coverage,
        winner.combo.min_semantic_score,
        winner.combo.reranker_id
    );
    println!(
        "Train objective: {:.4}  (MRR {:.4}, avg_tokens {:.1})",
        winner.objective, winner.mean_mrr, winner.mean_tokens
    );
    println!(
        "Holdout objective: {:.4} vs baseline {:.4} (improvement: {:+.4})",
        holdout_obj, baseline_holdout_obj, improvement
    );

    if improvement < 0.01 {
        println!();
        println!(
            "verdict: no change recommended (holdout improvement {improvement:+.4} < 0.01 threshold)"
        );
        return Ok(());
    }

    println!();
    // Reranker change recommendation (never auto-applied).
    if winner.combo.reranker_id != current_combo.reranker_id {
        println!(
            "note: reranker change recommended ({} → {}) — apply manually after \
             downloading the model and restarting the MCP daemon.",
            current_combo.reranker_id, winner.combo.reranker_id
        );
    }

    if !args.apply {
        if !using_personal {
            println!(
                "note: fixture mode — results are relative only; \
                 --apply is disabled until you have ≥30 cited cases."
            );
        }
        println!(
            "DRY RUN — to apply: kimetsu brain tune --apply\n\
             (lex {:.2}→{:.2}, sem {:.3}→{:.3}, fusion {}→{})",
            current_combo.min_lexical_coverage,
            winner.combo.min_lexical_coverage,
            current_combo.min_semantic_score,
            winner.combo.min_semantic_score,
            current_combo.fusion,
            winner.combo.fusion,
        );
        return Ok(());
    }

    // --apply: write floors to project.toml using surgical toml_edit so that
    // user comments and unknown keys are preserved (S4.2).
    let disk_text = std::fs::read_to_string(&paths.project_toml)
        .map_err(|e| format!("tune --apply: could not read project.toml: {e}"))?;
    let mut doc: toml_edit::DocumentMut = disk_text
        .parse()
        .map_err(|e| format!("tune --apply: project.toml is invalid TOML: {e}"))?;
    set_toml_edit_path(
        &mut doc,
        "broker.min_lexical_coverage",
        &toml::Value::Float(winner.combo.min_lexical_coverage as f64),
    )
    .map_err(|e| format!("tune --apply: {e}"))?;
    set_toml_edit_path(
        &mut doc,
        "broker.min_semantic_score",
        &toml::Value::Float(winner.combo.min_semantic_score as f64),
    )
    .map_err(|e| format!("tune --apply: {e}"))?;
    // v2.6: the fusion rule is swept alongside the floors, so --apply writes
    // it too. Kimetsu ships `linear`; this is the path by which a corpus that
    // prefers rank fusion actually gets it.
    set_toml_edit_path(
        &mut doc,
        "broker.fusion",
        &toml::Value::String(winner.combo.fusion.clone()),
    )
    .map_err(|e| format!("tune --apply: {e}"))?;
    std::fs::write(&paths.project_toml, doc.to_string())?;

    // Snapshot to tune-history.
    let now_str = time::OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "unknown".to_string());
    let history_entry = TuneHistoryEntry {
        timestamp: now_str,
        before: current_combo,
        after: winner.combo.clone(),
        train_objective: winner.objective,
        holdout_objective: holdout_obj,
        holdout_mrr,
        baseline_holdout_objective: baseline_holdout_obj,
        // S2.1: record corpus size so re-tune trigger can detect growth.
        memory_count_at_tune: Some(current_memory_count),
    };
    append_tune_history(&paths.kimetsu_dir, history_entry)?;

    println!(
        "Applied: lex_coverage={:.2}, sem_score={:.3} → project.toml updated.",
        winner.combo.min_lexical_coverage, winner.combo.min_semantic_score
    );
    println!("Snaphotted to .kimetsu/tune-history.json");

    Ok(())
}

pub(crate) fn brain_tune_revert(workspace: &std::path::Path) -> KimetsuResult<()> {
    use kimetsu_brain::tune::latest_tune_history;

    let paths = kimetsu_core::paths::ProjectPaths::discover(workspace)?;
    let Some(entry) = latest_tune_history(&paths.kimetsu_dir)? else {
        println!("No tune history found — nothing to revert.");
        return Ok(());
    };

    // S4.2: surgical write via toml_edit preserves user comments.
    let disk_text = std::fs::read_to_string(&paths.project_toml)
        .map_err(|e| format!("tune revert: could not read project.toml: {e}"))?;
    let mut doc: toml_edit::DocumentMut = disk_text
        .parse()
        .map_err(|e| format!("tune revert: project.toml is invalid TOML: {e}"))?;
    set_toml_edit_path(
        &mut doc,
        "broker.min_lexical_coverage",
        &toml::Value::Float(entry.before.min_lexical_coverage as f64),
    )
    .map_err(|e| format!("tune revert: {e}"))?;
    set_toml_edit_path(
        &mut doc,
        "broker.min_semantic_score",
        &toml::Value::Float(entry.before.min_semantic_score as f64),
    )
    .map_err(|e| format!("tune revert: {e}"))?;
    set_toml_edit_path(
        &mut doc,
        "broker.fusion",
        &toml::Value::String(entry.before.fusion.clone()),
    )
    .map_err(|e| format!("tune revert: {e}"))?;
    std::fs::write(&paths.project_toml, doc.to_string())?;

    println!(
        "Reverted: lex_coverage={:.2}, sem_score={:.3}, fusion={} (from tune at {})",
        entry.before.min_lexical_coverage,
        entry.before.min_semantic_score,
        entry.before.fusion,
        entry.timestamp
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Story 3.1 + 3.2: kimetsu brain consolidate
// ---------------------------------------------------------------------------

pub(crate) fn brain_consolidate(args: ConsolidateArgs) -> KimetsuResult<()> {
    use kimetsu_brain::consolidate::{
        ConsolidateOptions, DistillOptions, find_distill_clusters, load_embeddable_rows,
        run_consolidation,
    };

    let workspace = args
        .workspace
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());

    let (paths, _config, conn) = kimetsu_brain::project::load_project(&workspace)?;

    let stdout = io::stdout();
    let mut out = stdout.lock();

    // --- Story 3.1: near-duplicate merge ---
    // --distill is additive; 3.1 merge always runs alongside it.
    {
        let opts = ConsolidateOptions {
            threshold: args.threshold,
            dry_run: args.dry_run,
        };

        if !args.dry_run && !args.yes {
            // Check TTY requirement.
            if !io::stdin().is_terminal() {
                return Err(
                    "stdin is not a TTY; pass --yes to confirm consolidation non-interactively"
                        .into(),
                );
            }
            // Interactive prompt.
            write!(
                out,
                "Consolidate near-duplicate memories (threshold={:.2})? [y/N] ",
                args.threshold
            )?;
            out.flush()?;
            let mut line = String::new();
            io::stdin().lock().read_line(&mut line)?;
            let answer = line.trim().to_ascii_lowercase();
            if answer != "y" && answer != "yes" {
                writeln!(out, "Aborted.")?;
                return Ok(());
            }
        }

        run_consolidation(&conn, &opts, &mut out)?;
    }

    // --- Story 3.2: cluster distillation (--distill flag) ---
    if args.distill {
        let dopts = DistillOptions::default();
        let by_model = load_embeddable_rows(&conn)?;
        let all_rows: Vec<_> = by_model.into_values().flatten().collect();
        let clusters = find_distill_clusters(&all_rows, &dopts);

        if clusters.is_empty() {
            writeln!(
                out,
                "\nNo distillable clusters found (lo={:.2} hi={:.2}, min_size={}).",
                dopts.lo, dopts.hi, dopts.min_cluster_size
            )?;
            return Ok(());
        }

        // Try to resolve a distiller.
        let resolved = distiller::resolve_distiller(&workspace);

        if resolved.is_none() || args.dry_run {
            writeln!(
                out,
                "\nDistillable clusters ({} found — lo={:.2} hi={:.2}):",
                clusters.len(),
                dopts.lo,
                dopts.hi
            )?;
            for (i, cluster) in clusters.iter().enumerate() {
                writeln!(
                    out,
                    "\nCluster {} [tags: {}]:",
                    i + 1,
                    cluster.shared_tags.join(", ")
                )?;
                for m in &cluster.memories {
                    writeln!(
                        out,
                        "  [{}] {}",
                        m.memory_id,
                        &m.text[..m.text.len().min(80)]
                    )?;
                }
            }
            if resolved.is_none() {
                writeln!(
                    out,
                    "\nNo distiller configured — printed clusters above. Configure [learning.distiller] to auto-distil."
                )?;
            }
            return Ok(());
        }

        // Distiller is available — generate proposals.
        let distiller_resolved = resolved.unwrap();
        let mut proposals_created = 0usize;
        for cluster in &clusters {
            let cluster_text = cluster
                .memories
                .iter()
                .enumerate()
                .map(|(i, m)| format!("{}. {}", i + 1, m.text))
                .collect::<Vec<_>>()
                .join("\n");
            let prompt = format!(
                "Distill these {} related lessons into ONE general principle \
                 (2-4 sentences, imperative, no project-specific context):\n\n{cluster_text}",
                cluster.memories.len()
            );
            let mut provider = distiller::make_provider_for_resolved(&distiller_resolved);
            if let Some(ref mut p) = provider {
                let lessons = distiller::distill_lessons(&prompt, p.as_mut());
                for lesson in lessons {
                    let result = kimetsu_brain::project::propose_memory(
                        &distiller_resolved.record_start,
                        distiller_resolved.scope,
                        MemoryKind::Convention,
                        &lesson.lesson,
                        lesson.confidence.clamp(0.0, 1.0),
                        &format!(
                            "distilled from cluster [tags: {}]",
                            cluster.shared_tags.join(", ")
                        ),
                    );
                    if result.is_ok() {
                        proposals_created += 1;
                    }
                }
            }
        }

        writeln!(
            out,
            "\nCreated {proposals_created} distillation proposal(s). Review with: kimetsu brain memory proposals"
        )?;
        drop(paths);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Flagship 2 / Story 2.3: kimetsu brain reflect
// ---------------------------------------------------------------------------

/// Adapter: wraps a `kimetsu_agent` `ModelProvider` so it can be used as the
/// `kimetsu_brain::consolidate::ModelProvider` the `run_reflection` function
/// expects.
pub(crate) struct ReflectionModelAdapter<'a> {
    inner: &'a mut dyn kimetsu_agent::model::ModelProvider,
}

impl<'a> kimetsu_brain::consolidate::ModelProvider for ReflectionModelAdapter<'a> {
    fn complete_text(&mut self, prompt: &str) -> Option<String> {
        use kimetsu_agent::model::{ModelMessage, ModelRequest, ToolChoice};
        let req = ModelRequest {
            messages: vec![ModelMessage::user_text(prompt)],
            tools: Vec::new(),
            tool_choice: ToolChoice::None,
            max_output_tokens: 512,
            temperature: 0.2,
            metadata: serde_json::Value::Null,
        };
        self.inner.complete(req).ok()?.text
    }
}

pub(crate) fn brain_reflect(args: ReflectArgs) -> KimetsuResult<()> {
    use kimetsu_brain::consolidate::{ReflectionOptions, run_reflection};

    let workspace = args
        .workspace
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());

    let (_paths, _config, conn) = kimetsu_brain::project::load_project(&workspace)?;

    let stdout = io::stdout();
    let mut out = stdout.lock();

    let opts = ReflectionOptions {
        dry_run: args.dry_run,
        ..Default::default()
    };

    // Try to resolve a cheap model.
    let resolved = distiller::resolve_distiller(&workspace);

    if resolved.is_none() || args.dry_run {
        run_reflection(&conn, &opts, None, &mut out)?;
        if resolved.is_none() && !args.dry_run {
            writeln!(
                out,
                "\nNo cheap model configured — printed clusters above.\n\
                 Configure [cheap_model] in project.toml to auto-synthesize principles."
            )?;
        }
        return Ok(());
    }

    // Build the model provider from the resolved distiller.
    let distiller_resolved = resolved.unwrap();
    let mut provider_box = distiller::make_provider_for_resolved(&distiller_resolved);
    let summary = if let Some(ref mut p) = provider_box {
        let mut adapter = ReflectionModelAdapter { inner: p.as_mut() };
        run_reflection(&conn, &opts, Some(&mut adapter), &mut out)?
    } else {
        run_reflection(&conn, &opts, None, &mut out)?
    };

    if summary.proposals_created > 0 {
        writeln!(
            out,
            "\nCreated {} reflection proposal(s). Review with: kimetsu brain memory proposals",
            summary.proposals_created
        )?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Story 3.3: kimetsu brain triage
// ---------------------------------------------------------------------------

pub(crate) fn brain_triage(args: TriageArgs) -> KimetsuResult<()> {
    let workspace = args
        .workspace
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());

    let (_paths, _config, conn) = kimetsu_brain::project::load_project_readonly(&workspace)?;

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let stdin = io::stdin();
    let mut sin = stdin.lock();

    let candidates = triage_candidates(&conn, args.score_floor, args.age_days)?;

    if candidates.is_empty() {
        writeln!(
            out,
            "No fading memories found (score_floor={:.2}, age_days={}).",
            args.score_floor, args.age_days
        )?;
        return Ok(());
    }

    writeln!(
        out,
        "{} fading memor{} (score < {:.2}, age > {}d):",
        candidates.len(),
        if candidates.len() == 1 { "y" } else { "ies" },
        args.score_floor,
        args.age_days
    )?;

    if args.prune_all {
        if !args.yes {
            if !io::stdin().is_terminal() {
                return Err(
                    "stdin is not a TTY; pass --yes to confirm --prune-all non-interactively"
                        .into(),
                );
            }
            write!(out, "Prune all {} candidates? [y/N] ", candidates.len())?;
            out.flush()?;
            let mut line = String::new();
            sin.read_line(&mut line)?;
            let answer = line.trim().to_ascii_lowercase();
            if answer != "y" && answer != "yes" {
                writeln!(out, "Aborted.")?;
                return Ok(());
            }
        }
        let mut pruned = 0usize;
        for c in &candidates {
            let reason = format!(
                "triage_prune score={:.2} age_days={}",
                c.usefulness_score, c.age_days
            );
            if kimetsu_brain::project::invalidate_memory(&workspace, &c.memory_id, Some(&reason))
                .is_ok()
            {
                pruned += 1;
            }
        }
        writeln!(
            out,
            "Pruned {pruned} memor{}.",
            if pruned == 1 { "y" } else { "ies" }
        )?;
        return Ok(());
    }

    // Interactive per-item loop.
    if !io::stdin().is_terminal() {
        // Non-TTY with no --prune-all: just print the list.
        for c in &candidates {
            writeln!(
                out,
                "[{}] {}/{} age={}d score={:.2} — {}",
                c.memory_id,
                c.scope,
                c.kind,
                c.age_days,
                c.usefulness_score,
                &c.text[..c.text.len().min(80)]
            )?;
        }
        writeln!(out, "\nPass --prune-all --yes to prune non-interactively.")?;
        return Ok(());
    }

    triage_interactive_loop(&workspace, &candidates, &mut sin, &mut out)
}

// ---------------------------------------------------------------------------
// F3 Story 3.1 + 3.3: brain forget
// ---------------------------------------------------------------------------

pub(crate) fn brain_forget(args: ForgetArgs) -> KimetsuResult<()> {
    use kimetsu_brain::lifecycle::{ForgetOptions, ProposalGcOptions, forget_brain, gc_proposals};

    let workspace = args
        .workspace
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());

    // Load config to read lifecycle defaults.
    let (_paths, config, _conn) = kimetsu_brain::project::load_project_readonly(&workspace)?;
    let lc = &config.lifecycle;

    // Respect the opt-in gate unless --force-enabled or --dry-run.
    if !args.dry_run && !args.force_enabled && !lc.forget_enabled {
        eprintln!(
            "Forgetting is disabled in project.toml (lifecycle.forget_enabled = false).\n\
             Pass --force-enabled to override for this run, or set it in project.toml."
        );
        return Ok(());
    }

    // --json is report-only: never write, so it composes safely with harnesses.
    let report_only = args.dry_run || args.json;

    let opts = ForgetOptions {
        dry_run: report_only,
        usefulness_floor: args.usefulness_floor.unwrap_or(lc.forget_usefulness_floor),
        min_age_days: args.min_age_days.unwrap_or(lc.forget_min_age_days),
        protect_use_count: args
            .protect_use_count
            .unwrap_or(lc.forget_protect_use_count),
    };

    // -- Forget pass --
    let summary = forget_brain(&workspace, opts)?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&summary)?);
        return Ok(());
    }

    if report_only {
        if summary.candidates.is_empty() {
            println!("dry-run: no memories matched the forget criteria.");
        } else {
            println!(
                "dry-run: {} memor{} would be forgotten:",
                summary.candidates.len(),
                if summary.candidates.len() == 1 {
                    "y"
                } else {
                    "ies"
                }
            );
            for c in &summary.candidates {
                println!(
                    "  [{}] {}/{} use_count={} usefulness={:.3} age={:.0}d — {}",
                    &c.memory_id[..c.memory_id.len().min(12)],
                    c.scope,
                    c.kind,
                    c.use_count,
                    c.usefulness_score,
                    c.age_days,
                    c.text_preview
                );
            }
        }
    } else {
        // Confirm unless --yes.
        if !args.yes && !summary.candidates.is_empty() {
            if !io::stdin().is_terminal() {
                return Err(
                    "stdin is not a TTY; pass --yes to confirm forgetting non-interactively".into(),
                );
            }
            let stdout = io::stdout();
            let mut out = stdout.lock();
            let stdin = io::stdin();
            let mut sin = stdin.lock();
            write!(
                out,
                "Forget {} memor{}? [y/N] ",
                summary.candidates.len(),
                if summary.candidates.len() == 1 {
                    "y"
                } else {
                    "ies"
                }
            )?;
            out.flush()?;
            let mut line = String::new();
            sin.read_line(&mut line)?;
            let answer = line.trim().to_ascii_lowercase();
            if answer != "y" && answer != "yes" {
                println!("Aborted.");
                return Ok(());
            }
        }

        if summary.archived == 0 {
            println!("No memories matched the forget criteria. Brain is already lean.");
        } else {
            println!(
                "Forgot {} memor{} (archived via invalidation events).",
                summary.archived,
                if summary.archived == 1 { "y" } else { "ies" }
            );
        }
        if summary.failed > 0 {
            eprintln!(
                "Warning: {} memor{} could not be archived (check logs).",
                summary.failed,
                if summary.failed == 1 { "y" } else { "ies" }
            );
        }
    }

    // -- Proposal GC hygiene pass (Story 3.3) --
    if !args.no_proposal_gc {
        let gc_opts = ProposalGcOptions {
            dry_run: args.dry_run,
            expiry_days: lc.proposal_expiry_days,
            auto_accept_confidence: lc.proposal_auto_accept_confidence,
        };
        match gc_proposals(&workspace, gc_opts) {
            Ok(gc) => {
                if gc.expired > 0 {
                    let verb = if args.dry_run {
                        "would expire"
                    } else {
                        "expired"
                    };
                    println!(
                        "Proposal GC: {verb} {} stale proposal{}.",
                        gc.expired,
                        if gc.expired == 1 { "" } else { "s" }
                    );
                }
                if gc.auto_accepted > 0 {
                    let verb = if args.dry_run {
                        "would auto-accept"
                    } else {
                        "auto-accepted"
                    };
                    println!(
                        "Proposal GC: {verb} {} high-confidence proposal{}.",
                        gc.auto_accepted,
                        if gc.auto_accepted == 1 { "" } else { "s" }
                    );
                }
            }
            Err(e) => {
                // Non-fatal — just warn.
                eprintln!("Warning: proposal GC encountered an error: {e}");
            }
        }
    }

    Ok(())
}

pub(crate) fn brain_cite(args: CiteArgs) -> KimetsuResult<()> {
    let workspace = args
        .workspace
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());
    project::record_citations(
        &workspace,
        &args.memory_id,
        args.note.as_deref(),
        args.query.as_deref(),
    )?;
    println!(
        "Cited {} memor{} as one group (memory.cited recorded).",
        args.memory_id.len(),
        if args.memory_id.len() == 1 {
            "y"
        } else {
            "ies"
        }
    );
    Ok(())
}

pub(crate) fn brain_reinforce(args: ReinforceArgs) -> KimetsuResult<()> {
    let workspace = args
        .workspace
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());
    let (staple, routes) = if !args.staple && !args.routes {
        (true, true) // no flags = run both
    } else {
        (args.staple, args.routes)
    };
    let summary = kimetsu_brain::reinforce::reinforce(&workspace, staple, routes)?;
    println!(
        "reinforce: {} staple candidate(s), {} staple(s) created, {} route(s) built ({} embedded)",
        summary.staple_candidates,
        summary.staples_created,
        summary.routes_built,
        summary.routes_embedded
    );
    Ok(())
}

pub(crate) fn brain_benchmark_credit(args: BenchmarkCreditArgs) -> KimetsuResult<()> {
    let workspace = args
        .workspace
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());
    let credited = kimetsu_brain::reinforce::credit_benchmark_outcome(
        &workspace,
        &args.task,
        args.passed,
        args.top_k,
    )?;
    println!(
        "benchmark-credit: {} memor{} cited for task \"{}\" ({})",
        credited,
        if credited == 1 { "y" } else { "ies" },
        args.task,
        if args.passed {
            "passed"
        } else {
            "not passed — no citation"
        }
    );
    Ok(())
}

pub(crate) fn brain_regret(args: RegretArgs) -> KimetsuResult<()> {
    let workspace = args
        .workspace
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());
    project::record_regret(&workspace, &args.memory_id)?;
    println!(
        "Flagged memory {} as regretted (retrieval.regret recorded).",
        args.memory_id
    );
    Ok(())
}

pub(crate) fn brain_distill(args: DistillArgs) -> KimetsuResult<()> {
    let workspace = args
        .workspace
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());

    let resolved = distiller::resolve_distiller(&workspace).ok_or_else(|| {
        "no cheap model configured: set [cheap_model] provider + model in \
         .kimetsu/project.toml (e.g. provider = \"ollama\", model = \"qwen2.5:3b\")"
            .to_string()
    })?;
    let mut provider = distiller::make_provider_for_resolved(&resolved).ok_or_else(|| {
        format!(
            "could not construct the '{}' model provider for distillation",
            resolved.provider
        )
    })?;

    let transcript = args.transcript.to_string_lossy();
    let view = distiller::build_transcript_view(&transcript, distiller::MAX_VIEW_CHARS);
    if view.trim().is_empty() {
        if args.json {
            println!("[]");
        } else {
            eprintln!("transcript is empty or unreadable: {transcript}");
        }
        return Ok(());
    }

    let lessons = distiller::distill_lessons(&view, provider.as_mut());

    if args.json {
        let rows: Vec<serde_json::Value> = lessons
            .iter()
            .map(|l| {
                serde_json::json!({
                    "lesson": l.lesson,
                    "tags": l.tags,
                    "kind": l.kind,
                    "confidence": l.confidence,
                    "valid_from": l.valid_from,
                    "valid_to": l.valid_to,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else if lessons.is_empty() {
        println!("no lessons distilled from this transcript.");
    } else {
        println!(
            "distilled {} lesson{} (not recorded):",
            lessons.len(),
            if lessons.len() == 1 { "" } else { "s" }
        );
        for l in &lessons {
            println!(
                "  [{}] {} (confidence {:.2}; tags: {})",
                l.kind,
                l.lesson,
                l.confidence,
                l.tags.join(", ")
            );
        }
    }
    Ok(())
}

/// #2 knowledge graph dispatch.
pub(crate) fn brain_graph(command: GraphCommand) -> KimetsuResult<()> {
    match command {
        GraphCommand::Build(args) => brain_graph_build(args),
    }
}

/// `kimetsu brain graph build`: derive `relates_to` edges (rule layer) over the
/// active memories and persist them as rebuild-safe `memory.edge` events. With
/// `--enrich`, additionally ask the cheap model for typed edges. With `--dry-run`,
/// preview counts without writing. With `--json`, emit a machine-readable summary.
pub(crate) fn brain_graph_build(args: GraphBuildArgs) -> KimetsuResult<()> {
    let workspace = args
        .workspace
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());

    // Optional LLM enrichment: typed edges proposed by the cheap model. Best
    // effort — a missing model or unparseable response yields zero extra edges.
    let extra_edges: Vec<(String, String, String)> = if args.enrich {
        match project::active_memory_texts(&workspace) {
            Ok(mems) => enrich_typed_edges(&workspace, &mems),
            Err(e) => {
                eprintln!("kimetsu: graph enrich skipped (could not read memories: {e})");
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    let summary = project::build_graph(&workspace, &extra_edges, args.max_fan_out, args.dry_run)?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&summary)?);
        return Ok(());
    }

    let verb = if summary.dry_run {
        "would write"
    } else {
        "wrote"
    };
    println!(
        "Graph build: {} active memories, {} rule + {} enrichment edges proposed; {} {} edge(s).",
        summary.active_memories,
        summary.rule_edges,
        summary.enrich_edges,
        verb,
        if summary.dry_run {
            summary.by_type.values().sum::<usize>()
        } else {
            summary.written
        }
    );
    for (ty, n) in &summary.by_type {
        println!("  {ty}: {n}");
    }
    if summary.dry_run {
        println!("(dry-run: nothing persisted; re-run without --dry-run to write)");
    }
    Ok(())
}

/// LLM enrichment for the knowledge graph: ask the configured cheap model, for
/// each active memory, which OTHER memory it most directly refines or derives
/// from, and with what typed relation. Returns `(src_id, dst_id, edge_type)`
/// tuples restricted to the reserved typed-edge vocabulary and to ids that exist
/// in `memories`. Best-effort and bounded: returns an empty vec when no cheap
/// model is configured. Small local models are weak at this (documented).
pub(crate) fn enrich_typed_edges(
    workspace: &Path,
    memories: &[(String, String)],
) -> Vec<(String, String, String)> {
    const ALLOWED: [&str; 3] = ["refines", "lesson_from", "decision_touches"];
    // Bound the work: enrichment is opt-in and model-bottlenecked.
    const MAX_MEMORIES: usize = 200;

    let Some(resolved) = distiller::resolve_distiller(workspace) else {
        eprintln!("kimetsu: --enrich requested but no [cheap_model] configured; rule edges only.");
        return Vec::new();
    };
    let Some(mut provider) = distiller::make_provider_for_resolved(&resolved) else {
        eprintln!(
            "kimetsu: --enrich could not construct the cheap-model provider; rule edges only."
        );
        return Vec::new();
    };

    let ids: std::collections::HashSet<&str> = memories.iter().map(|(id, _)| id.as_str()).collect();
    // A compact catalog the model can reference by id.
    let catalog: String = memories
        .iter()
        .take(MAX_MEMORIES)
        .map(|(id, text)| {
            format!(
                "{id}\t{}",
                text.replace('\n', " ")
                    .chars()
                    .take(160)
                    .collect::<String>()
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    const SYSTEM: &str = "You connect software-engineering memories into a knowledge graph. \
        Given a SOURCE memory and a CATALOG of other memories (id<TAB>text), pick AT MOST ONE \
        catalog memory the SOURCE most directly relates to, and the relation type. Allowed types: \
        refines (source narrows/refines target), lesson_from (source is a lesson learned from \
        target), decision_touches (source is a decision touching target). Reply with ONE line of \
        strict JSON: {\"dst\":\"<id or empty>\",\"type\":\"<type or empty>\"}. If nothing relates, \
        reply {\"dst\":\"\",\"type\":\"\"}. Output only the JSON.";

    let mut out: Vec<(String, String, String)> = Vec::new();
    for (id, text) in memories.iter().take(MAX_MEMORIES) {
        let user = format!(
            "SOURCE ({id}): {src}\n\nCATALOG:\n{catalog}",
            id = id,
            src = text
                .replace('\n', " ")
                .chars()
                .take(240)
                .collect::<String>(),
            catalog = catalog,
        );
        let Some(reply) = distiller::complete_simple(SYSTEM, &user, 64, provider.as_mut()) else {
            continue;
        };
        let reply = reply.trim();
        // Extract the first {...} object from the reply.
        let (Some(s), Some(e)) = (reply.find('{'), reply.rfind('}')) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&reply[s..=e]) else {
            continue;
        };
        let dst = v.get("dst").and_then(|x| x.as_str()).unwrap_or("").trim();
        let ty = v.get("type").and_then(|x| x.as_str()).unwrap_or("").trim();
        if dst.is_empty() || ty.is_empty() || dst == id {
            continue;
        }
        if ALLOWED.contains(&ty) && ids.contains(dst) {
            out.push((id.clone(), dst.to_string(), ty.to_string()));
        }
    }
    out
}

/// HyDE query expansion: append a hypothetical answer passage (from the cheap
/// model) to `query`, so semantic retrieval matches the answer's vector rather
/// than the question's. Falls back to the raw query when the tier forbids model
/// calls, when no cheap model is configured, or when the model call fails
/// (graceful, never errors retrieval).
///
/// HyDE is a model call *inside retrieval*, so it resolves through the tier
/// gate: on Free the raw query is used.
pub(crate) fn hyde_augment_query(workspace: &Path, query: &str) -> String {
    let Some(resolved) = distiller::resolve_pipeline_distiller(workspace) else {
        eprintln!(
            "kimetsu: HyDE needs the deep tier and a [cheap_model]; using the raw query. \
             (`kimetsu config set kimetsu.tier deep`)"
        );
        return query.to_string();
    };
    let Some(mut provider) = distiller::make_provider_for_resolved(&resolved) else {
        return query.to_string();
    };
    match distiller::hyde_expand(query, provider.as_mut()) {
        Some(hyp) => format!("{query}\n{hyp}"),
        None => query.to_string(),
    }
}

/// A fading memory candidate for triage.
#[derive(Debug)]
pub(crate) struct TriageCandidate {
    memory_id: String,
    scope: String,
    kind: String,
    text: String,
    age_days: i64,
    usefulness_score: f32,
}

/// Query the DB for triage candidates.
pub(crate) fn triage_candidates(
    conn: &rusqlite::Connection,
    score_floor: f32,
    age_days: u32,
) -> KimetsuResult<Vec<TriageCandidate>> {
    use rusqlite::params;
    use time::OffsetDateTime;

    // Compute the cutoff timestamp.
    let now = OffsetDateTime::now_utc();
    let cutoff = now - time::Duration::days(i64::from(age_days));
    let cutoff_str = cutoff
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default();

    let mut stmt = conn.prepare(
        "SELECT memory_id, scope, kind, text, usefulness_score,
                COALESCE(last_useful_at, created_at) AS ref_ts
         FROM memories
         WHERE invalidated_at IS NULL
           AND superseded_by IS NULL
           AND usefulness_score < ?1
           AND COALESCE(last_useful_at, created_at) < ?2
         ORDER BY usefulness_score ASC, COALESCE(last_useful_at, created_at) ASC
         LIMIT 200",
    )?;

    let rows = stmt.query_map(params![score_floor as f64, cutoff_str], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, f64>(4)?,
            row.get::<_, String>(5)?,
        ))
    })?;

    let mut candidates = Vec::new();
    for row in rows {
        let (memory_id, scope, kind, text, score, ref_ts) = row?;
        let age = {
            use time::format_description::well_known::Rfc3339;
            OffsetDateTime::parse(&ref_ts, &Rfc3339)
                .map(|t| (now - t).whole_days().max(0))
                .unwrap_or(0)
        };
        candidates.push(TriageCandidate {
            memory_id,
            scope,
            kind,
            text,
            age_days: age,
            usefulness_score: score as f32,
        });
    }
    Ok(candidates)
}

/// Interactive decision loop — mirrors the `decide_preflight_action` pattern
/// in update.rs. Generic over BufRead + Write for testability.
pub(crate) fn triage_interactive_loop<R: io::BufRead, W: io::Write>(
    workspace: &std::path::Path,
    candidates: &[TriageCandidate],
    reader: &mut R,
    writer: &mut W,
) -> KimetsuResult<()> {
    let mut pruned = 0usize;
    let mut kept = 0usize;
    let mut skipped = 0usize;

    for c in candidates {
        writeln!(
            writer,
            "\n[{}] {}/{} age={}d score={:.2}",
            c.memory_id, c.scope, c.kind, c.age_days, c.usefulness_score
        )?;
        writeln!(writer, "  {}", &c.text[..c.text.len().min(120)])?;
        write!(writer, "  [k]eep / [p]rune / [s]kip: ")?;
        writer.flush()?;

        let mut line = String::new();
        reader.read_line(&mut line)?;
        match line.trim().to_ascii_lowercase().as_str() {
            "p" | "prune" => {
                let reason = format!(
                    "triage_prune score={:.2} age_days={}",
                    c.usefulness_score, c.age_days
                );
                if kimetsu_brain::project::invalidate_memory(workspace, &c.memory_id, Some(&reason))
                    .is_ok()
                {
                    pruned += 1;
                    writeln!(writer, "  → pruned.")?;
                } else {
                    writeln!(writer, "  → prune failed.")?;
                }
            }
            "k" | "keep" => {
                kept += 1;
                writeln!(writer, "  → kept.")?;
            }
            _ => {
                skipped += 1;
                writeln!(writer, "  → skipped.")?;
            }
        }
    }

    writeln!(
        writer,
        "\nTriage complete: {} pruned, {} kept, {} skipped.",
        pruned, kept, skipped
    )?;
    Ok(())
}

/// Format a token count with thousands separator (space).
pub(crate) fn format_token_count(n: u64) -> String {
    if n < 1_000 {
        return n.to_string();
    }
    let s = n.to_string();
    let mut out = String::new();
    let rem = s.len() % 3;
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (i % 3 == rem) {
            out.push(' ');
        }
        out.push(ch);
    }
    out
}

/// Flagship 3.1 — `kimetsu brain ask "<question>"`.
///
/// Retrieves brain context for the question and composes a grounded, cited
/// answer using the configured cheap model (local preferred). Degrades
/// gracefully: verbatim capsule dump when no model is configured, refusal
/// when retrieval is empty. Never hard-fails.
pub(crate) fn brain_ask(args: AskArgs) -> KimetsuResult<()> {
    let workspace = args
        .workspace
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());

    // --helpful mode: mark a prior answer helpful by citing its memories.
    if let Some(citations_raw) = &args.helpful {
        let handles: Vec<String> = citations_raw
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if handles.is_empty() {
            eprintln!("--helpful requires at least one memory handle (e.g. memory:01ABC)");
            return Ok(());
        }
        ask::record_helpful_mark(&workspace, &handles);
        if args.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "ok": true,
                    "marked_helpful": handles,
                }))?
            );
        } else {
            println!("Marked {} citation(s) helpful.", handles.len());
        }
        return Ok(());
    }

    let question = args.question.trim();
    if question.is_empty() {
        eprintln!("Usage: kimetsu brain ask \"<question>\"");
        return Ok(());
    }

    let result = ask::compose_answer(&workspace, question);

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ok": true,
                "question": question,
                "answer": result.answer,
                "citations": result.citations,
                "grounded": result.grounded,
                "model_used": result.model_used,
                "verbatim": result.verbatim,
            }))?
        );
        return Ok(());
    }

    // Human-readable output.
    println!("{}", result.answer);
    if !result.citations.is_empty() {
        println!();
        println!("Sources: {}", result.citations.join(", "));
    }
    if !result.grounded {
        // Already printed refusal text; nothing more to do.
    } else if result.verbatim {
        println!();
        println!(
            "Tip: configure a cheap model in project.toml \
             ([cheap_model] provider = \"ollama\" …) for composed answers."
        );
    } else {
        // Hint for the helpful-mark workflow.
        if !result.citations.is_empty() {
            let handles = result.citations.join(",");
            println!();
            println!("If this helped, run: kimetsu brain ask --helpful {handles} \"\"",);
        }
    }

    Ok(())
}

/// Flagship 2: `kimetsu brain skills` — Memory → Skill synthesis.
pub(crate) fn brain_skills(args: SkillsArgs) -> KimetsuResult<()> {
    let workspace = args
        .workspace
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());

    // --accept: install a specific pending proposal.
    if let Some(ref proposal_id) = args.accept {
        match skill_synth::install_skill_proposal(&workspace, proposal_id) {
            Ok(path) => {
                println!("Skill installed: {}", path.display());
                println!("Run `kimetsu brain skills --status` to check for future staleness.");
            }
            Err(e) => {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }

    // --reject: reject a specific pending proposal.
    if let Some(ref proposal_id) = args.reject {
        let (_paths, _config, conn) = project::load_project(&workspace)?;
        kimetsu_brain::skill_synthesis::reject_skill_proposal(&conn, proposal_id)?;
        println!("Proposal {proposal_id} rejected.");
        return Ok(());
    }

    // --status: show staleness for accepted skills.
    if args.status {
        let (_paths, _config, conn) = project::load_project(&workspace)?;
        skill_synth::print_staleness_status(&conn)?;
        return Ok(());
    }

    // --review: list proposals for review.
    if args.review {
        let (_paths, _config, conn) = project::load_project(&workspace)?;
        skill_synth::print_skill_review(&conn)?;
        return Ok(());
    }

    // Default (--detect or no flag): detect candidates + create proposals.
    let report = skill_synth::run_skill_synthesis(&workspace)?;
    skill_synth::print_synthesis_report(&report);
    Ok(())
}
