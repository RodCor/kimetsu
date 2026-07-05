//! memory subcommands: list/add/top/blame/prune/review/etc.
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

pub(crate) fn stats() -> KimetsuResult<()> {
    let memories = project::list_memories(&env::current_dir()?)?;
    let runs = project::list_runs(&env::current_dir()?)?;
    println!("memories: {}", memories.len());
    println!("runs: {}", runs.len());
    Ok(())
}

pub(crate) fn memory(command: MemoryCommand) -> KimetsuResult<()> {
    match command {
        MemoryCommand::Add(args) => {
            // Validate scope/kind locally regardless of target.
            let scope = MemoryScope::from_str(&args.scope)?;
            let kind = MemoryKind::from_str(&args.kind)?;
            if let Some(base_url) = args.remote.remote.as_deref() {
                // Slice C: write to a remote team brain over HTTP MCP.
                let repo = args.remote.repo.as_deref().ok_or_else(|| {
                    "kimetsu brain memory add --remote requires --repo <id>".to_string()
                })?;
                let token = remote_client::resolve_token(args.remote.token.as_deref())?;
                let result = remote_client::remote_call(
                    base_url,
                    repo,
                    &token,
                    "kimetsu_brain_memory_add",
                    serde_json::json!({
                        "scope": args.scope,
                        "kind": args.kind,
                        "text": args.text,
                    }),
                )?;
                println!("{}", remote_client::render_result(&result));
                Ok(())
            } else {
                let id = project::add_memory(&env::current_dir()?, scope, kind, &args.text)?;
                println!("memory_id: {id}");
                Ok(())
            }
        }
        MemoryCommand::AddBatch(args) => memory_add_batch(args),
        MemoryCommand::List { json } => {
            let memories = project::list_memories(&env::current_dir()?)?;
            if json {
                let rows: Vec<serde_json::Value> = memories
                    .iter()
                    .map(|m| {
                        serde_json::json!({
                            "memory_id": m.memory_id,
                            "scope": m.scope,
                            "kind": m.kind,
                            "confidence": m.confidence,
                            "use_count": m.use_count,
                            "usefulness_score": m.usefulness_score,
                            "text": m.text,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&rows)?);
                return Ok(());
            }
            if memories.is_empty() {
                println!("no memories");
                return Ok(());
            }

            for memory in memories {
                let usefulness_ratio = if memory.use_count > 0 {
                    format!(
                        " ratio={:+.2}",
                        memory.usefulness_score / memory.use_count as f32
                    )
                } else {
                    String::new()
                };
                println!(
                    "{} [{}:{} confidence={:.2} uses={} usefulness={:+.1}{}] {}",
                    memory.memory_id,
                    memory.scope,
                    memory.kind,
                    memory.confidence,
                    memory.use_count,
                    memory.usefulness_score,
                    usefulness_ratio,
                    memory.text
                );
            }
            Ok(())
        }
        MemoryCommand::Proposals(args) => {
            let proposals = project::list_proposals(
                &env::current_dir()?,
                project::ProposalFilter {
                    scope: args.scope,
                    kind: args.kind,
                    from_run: args.from_run,
                    min_confidence: args.min_confidence,
                    status: Some(args.status),
                    limit: args.limit,
                    offset: 0,
                },
            )?;
            if proposals.is_empty() {
                println!("no memory proposals");
                return Ok(());
            }

            for proposal in proposals {
                println!(
                    "{} [{}:{} status={} confidence={:.2} run={}] {}",
                    proposal.proposal_id,
                    proposal.scope,
                    proposal.kind,
                    proposal.status,
                    proposal.proposed_confidence,
                    proposal.run_id,
                    proposal.text
                );
                if !proposal.rationale.is_empty() {
                    println!("  rationale: {}", proposal.rationale);
                }
                if let Some(reason) = proposal.decided_reason.as_deref()
                    && !reason.is_empty()
                {
                    println!("  decided_reason: {reason}");
                }
            }
            Ok(())
        }
        MemoryCommand::Accept(args) => {
            let memory_id = project::accept_proposal(
                &env::current_dir()?,
                &args.proposal_id,
                project::AcceptOverrides {
                    scope: args.scope,
                    confidence: args.confidence,
                },
            )?;
            println!("memory_id: {memory_id}");
            Ok(())
        }
        MemoryCommand::Reject(args) => {
            project::reject_proposal(
                &env::current_dir()?,
                &args.proposal_id,
                args.reason.as_deref(),
            )?;
            if let Some(reason) = args.reason.as_deref() {
                println!("rejected proposal: {} (reason: {reason})", args.proposal_id);
            } else {
                println!("rejected proposal: {}", args.proposal_id);
            }
            Ok(())
        }
        MemoryCommand::Invalidate(args) => {
            project::invalidate_memory(
                &env::current_dir()?,
                &args.memory_id,
                args.reason.as_deref(),
            )?;
            if let Some(reason) = args.reason.as_deref() {
                println!("invalidated memory: {} (reason: {reason})", args.memory_id);
            } else {
                println!("invalidated memory: {}", args.memory_id);
            }
            Ok(())
        }
        MemoryCommand::Review(args) => review_proposals(args),
        MemoryCommand::Top(args) => memory_top(args),
        MemoryCommand::Prune(args) => memory_prune(args),
        MemoryCommand::Blame(args) => memory_blame(args),
        MemoryCommand::Conflicts(args) => memory_conflicts(args),
        MemoryCommand::Edit(args) => memory_edit(args),
        MemoryCommand::Undo(args) => memory_undo(args),
        MemoryCommand::SetAge(args) => {
            let workspace = args
                .workspace
                .unwrap_or_else(|| env::current_dir().unwrap_or_default());
            project::record_set_age(&workspace, &args.memory_id, args.days_ago)?;
            println!(
                "Backdated memory {} by {} days.",
                args.memory_id, args.days_ago
            );
            Ok(())
        }
    }
}

/// MP-6: pretty-print `list_memories_top`. Surfaces ratio + use_count
/// alongside the text so the user can quickly judge which entries to
/// keep and which to invalidate.
pub(crate) fn memory_top(args: TopArgs) -> KimetsuResult<()> {
    let cwd = env::current_dir()?;
    let rows = project::list_memories_top(
        &cwd,
        project::TopOptions {
            scope: args.scope.clone(),
            min_uses: args.min_uses,
            limit: args.limit,
        },
    )?;
    if rows.is_empty() {
        println!(
            "no memories meet the min-uses threshold ({})",
            args.min_uses
        );
        return Ok(());
    }
    println!(
        "top memories (min_uses>={}, limit={}{}):",
        args.min_uses,
        args.limit,
        args.scope
            .as_deref()
            .map(|s| format!(", scope={s}"))
            .unwrap_or_default()
    );
    for m in rows {
        let ratio = m.usefulness_score as f64 / m.use_count.max(1) as f64;
        println!(
            "  {} [{}:{} uses={} usefulness={:+.1} ratio={:+.2}] {}",
            m.memory_id, m.scope, m.kind, m.use_count, m.usefulness_score, ratio, m.text
        );
    }
    Ok(())
}

/// v0.5.1: `kimetsu brain memory blame <run-id>` — print the per-memory
/// attribution for a single run. Cited memories show the model's
/// rationale + turn; silent passengers show that they were retrieved but
/// never reached for. `--json` emits the full BlameReport for CI / hooks.
pub(crate) fn memory_blame(args: BlameArgs) -> KimetsuResult<()> {
    let cwd = env::current_dir()?;
    let report = project::blame_run(&cwd, args.run_id.trim())?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    println!("[blame] run {}", report.run_id);
    print!("[blame] outcome: {}", report.outcome);
    if let Some(cat) = report.failure_category.as_deref() {
        print!(" (category: {cat})");
    }
    println!();

    if report.cited.is_empty() && report.silent_passengers.is_empty() {
        println!(
            "[blame] no memories were retrieved or cited for this run. \
             Either the run pre-dates v0.5.1, the brain was off \
             (`--project` unset), or no `context.injected` events fired."
        );
        return Ok(());
    }

    if !report.cited.is_empty() {
        println!(
            "\n  cited memories ({} total) — earned strong ±1.0 signal:",
            report.cited.len()
        );
        for c in &report.cited {
            let rationale = c
                .rationale
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .map(|s| format!("  // {s}"))
                .unwrap_or_default();
            println!(
                "    {} [{}:{}] turn={}{}",
                c.memory_id, c.scope, c.kind, c.turn, rationale
            );
            println!("      {}", c.text_preview);
        }
    }

    if !report.silent_passengers.is_empty() {
        println!(
            "\n  silent passengers ({} total) — earned weak ±0.1 signal (model didn't cite):",
            report.silent_passengers.len()
        );
        for s in &report.silent_passengers {
            println!("    {} [{}:{}]", s.memory_id, s.scope, s.kind);
            println!("      {}", s.text_preview);
        }
    }
    println!();
    Ok(())
}

/// v0.5.2: `kimetsu brain memory conflicts` — list or resolve
/// conflict-detection hits surfaced at ingest. Without `--resolve` it
/// lists open conflicts (project + user brains merged), with the
/// origin brain shown per row so the operator knows where the
/// resolution will land. `--resolve <id> <resolution>` settles one
/// conflict and (for `kept_new` / `kept_existing`) invalidates the
/// losing side.
pub(crate) fn memory_conflicts(args: ConflictsArgs) -> KimetsuResult<()> {
    let cwd = env::current_dir()?;

    if let Some(resolve_args) = args.resolve.as_ref() {
        // num_args = 2 ensures clap delivers exactly 2 values.
        let conflict_id = resolve_args[0].trim();
        let resolution = resolve_args[1].trim();
        let updated = project::resolve_conflict(&cwd, conflict_id, resolution)?;
        if args.json {
            println!(
                "{}",
                serde_json::json!({
                    "conflict_id": conflict_id,
                    "resolution": resolution,
                    "updated": updated,
                })
            );
            return Ok(());
        }
        if updated {
            println!(
                "[conflicts] resolved {conflict_id} as {resolution} (losing side, if any, invalidated)"
            );
        } else {
            println!(
                "[conflicts] no open conflict with id {conflict_id} (already resolved, or unknown id)"
            );
        }
        return Ok(());
    }

    let open = project::list_conflicts(&cwd, args.limit)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&open)?);
        return Ok(());
    }

    if open.is_empty() {
        println!(
            "[conflicts] no open conflicts. \
             Either no contradictory memories have been ingested, \
             the embedder is the lean NoopEmbedder (build with \
             `--features embeddings` to enable detection), or all \
             prior conflicts have been resolved."
        );
        return Ok(());
    }

    println!("[conflicts] {} open conflict(s):", open.len());
    for scoped in &open {
        let c = &scoped.report;
        println!(
            "  {} [{}] {} <-> {} (similarity {:.3}, scope={}, kind={}, detected {})",
            c.conflict_id,
            scoped.source,
            c.new_memory_id,
            c.existing_memory_id,
            c.similarity,
            c.scope,
            c.kind,
            c.detected_at,
        );
        println!("    new:      {}", preview_inline(&c.new_text));
        println!("    existing: {}", preview_inline(&c.existing_text));
    }
    println!(
        "\nResolve with: kimetsu brain memory conflicts --resolve <id> <kept_new|kept_existing|kept_both>"
    );
    Ok(())
}

/// Q6: `kimetsu brain memory edit <id> [--text …] [--kind …]`
///
/// Edits an existing active memory in place — corrects the text and/or
/// changes the kind while KEEPING the learned history (use_count,
/// usefulness_score, confidence, created_at). The FTS index and embedding
/// are refreshed so semantic/keyword retrieval reflects the new text.
pub(crate) fn memory_edit(args: MemoryEditArgs) -> KimetsuResult<()> {
    if args.text.is_none() && args.kind.is_none() {
        return Err("memory edit: at least one of --text or --kind must be provided".into());
    }

    let cwd = env::current_dir()?;
    let new_kind = args.kind.as_deref().map(MemoryKind::from_str).transpose()?;

    project::edit_memory(&cwd, &args.memory_id, args.text.as_deref(), new_kind)?;
    println!("updated memory {}", args.memory_id);
    Ok(())
}

/// Q6: `kimetsu brain memory undo [--yes]`
///
/// Previews the most-recently-recorded active memory in the project brain,
/// confirms (unless `--yes`), then invalidates it. The row is retained for
/// audit purposes — it simply stops being surfaced in retrieval.
pub(crate) fn memory_undo(args: MemoryUndoArgs) -> KimetsuResult<()> {
    let cwd = env::current_dir()?;

    // Peek at the most-recent active memory before asking the user.
    let peek = project::peek_last_memory(&cwd)?;
    let preview = match peek {
        None => {
            println!("no active memories to undo");
            return Ok(());
        }
        Some(m) => m,
    };

    println!(
        "most recent memory: {} [{}:{}] {}",
        preview.memory_id, preview.scope, preview.kind, preview.text
    );

    // Confirm unless --yes or non-TTY.
    if !args.yes && io::stdin().is_terminal() {
        print!("invalidate this memory? [y/N] ");
        io::stdout().flush().ok();
        let mut line = String::new();
        io::stdin().lock().read_line(&mut line).ok();
        if !matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            println!("aborted");
            return Ok(());
        }
    }

    match project::undo_last_memory(&cwd)? {
        Some(undone) => {
            println!(
                "invalidated memory {} (row kept for audit; no longer retrieved)",
                undone.memory_id
            );
        }
        None => {
            // Edge case: someone invalidated the memory between our peek and
            // the undo call (concurrent write). Report gracefully.
            println!("no active memories to undo");
        }
    }

    Ok(())
}

/// `kimetsu brain memory add-batch` — ingest many memories in one process.
///
/// Reads a JSONL file (one JSON object per line) or a JSON array from FILE
/// (or stdin when FILE is `-`).  Processes all entries with the project and
/// embedder opened exactly once — far cheaper than spawning one
/// `memory add` subprocess per entry.
///
/// Each JSON object must have a `"text"` field.  Optional fields:
///   `"scope"` — overrides --scope for this entry
///   `"kind"`  — overrides --kind for this entry
///   `"valid_from"` / `"valid_to"` — RFC 3339 temporal bounds (Flagship 1)
pub(crate) fn memory_add_batch(args: MemoryAddBatchArgs) -> KimetsuResult<()> {
    use kimetsu_brain::project::BatchMemoryEntry;

    let default_scope = MemoryScope::from_str(&args.scope)?;
    let default_kind = MemoryKind::from_str(&args.kind)?;

    // Read raw bytes from file or stdin.
    let raw: String = if args.file == "-" {
        let stdin = io::stdin();
        let mut s = String::new();
        for line in stdin.lock().lines() {
            let line = line.map_err(|e| format!("stdin read error: {e}"))?;
            s.push_str(&line);
            s.push('\n');
        }
        s
    } else {
        std::fs::read_to_string(&args.file)
            .map_err(|e| format!("cannot read '{}': {e}", args.file))?
    };

    // Parse as JSON array first; fall back to JSONL (one object per line).
    // This handles both `[{...},{...}]` and `{...}\n{...}` formats.
    #[derive(serde::Deserialize)]
    struct RawEntry {
        text: String,
        #[serde(default)]
        scope: Option<String>,
        #[serde(default)]
        kind: Option<String>,
        #[serde(default)]
        valid_from: Option<String>,
        #[serde(default)]
        valid_to: Option<String>,
    }

    let raw_entries: Vec<RawEntry> = {
        let trimmed = raw.trim();
        if trimmed.starts_with('[') {
            // JSON array format.
            serde_json::from_str(trimmed).map_err(|e| format!("failed to parse JSON array: {e}"))?
        } else {
            // JSONL format: parse each non-empty line.
            let mut entries = Vec::new();
            for (line_no, line) in trimmed.lines().enumerate() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let entry: RawEntry = serde_json::from_str(line)
                    .map_err(|e| format!("failed to parse JSONL line {}: {e}", line_no + 1))?;
                entries.push(entry);
            }
            entries
        }
    };

    if raw_entries.is_empty() {
        if args.json {
            println!("{{\"added\":0,\"ids\":[]}}");
        } else {
            println!("added 0 memories");
        }
        return Ok(());
    }

    // Convert to BatchMemoryEntry, resolving scope/kind per entry.
    let mut entries: Vec<BatchMemoryEntry> = Vec::with_capacity(raw_entries.len());
    for (i, re) in raw_entries.into_iter().enumerate() {
        let scope = match re.scope.as_deref() {
            Some(s) => {
                MemoryScope::from_str(s).map_err(|e| format!("entry {i}: invalid scope: {e}"))?
            }
            None => default_scope,
        };
        let kind = match re.kind.as_deref() {
            Some(k) => {
                MemoryKind::from_str(k).map_err(|e| format!("entry {i}: invalid kind: {e}"))?
            }
            None => default_kind,
        };
        entries.push(BatchMemoryEntry {
            text: re.text,
            scope,
            kind,
            valid_from: re.valid_from,
            valid_to: re.valid_to,
        });
    }

    let n = entries.len();
    let ids = project::add_memories_batch(&env::current_dir()?, entries)?;

    if args.json {
        let out = serde_json::json!({"added": ids.len(), "ids": ids});
        println!("{}", serde_json::to_string(&out)?);
    } else {
        println!(
            "added {} memor{}",
            ids.len(),
            if ids.len() == 1 { "y" } else { "ies" }
        );
        if ids.len() < n {
            // Some were deduped — note the difference.
            let deduped = n - ids.len();
            // Actually ids.len() == n always; deduped entries still return an id.
            // This branch is unreachable but kept for clarity.
            eprintln!(
                "kimetsu-brain: {deduped} entr{} were duplicates (existing id returned)",
                if deduped == 1 { "y" } else { "ies" }
            );
        }
    }

    Ok(())
}

/// One-line truncate-and-collapse for CLI rendering of memory text.
/// Keeps the conflict listing scannable when capsules are long-form.
pub(crate) fn preview_inline(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let truncated: String = collapsed.chars().take(140).collect();
    if collapsed.chars().count() > 140 {
        format!("{truncated}…")
    } else {
        truncated
    }
}

/// MP-6: dry-run by default. Without `--apply` it prints the prune list
/// and exits 0; with `--apply` it invalidates each match via the same
/// `invalidate_memory` path used by `memory invalidate`.
pub(crate) fn memory_prune(args: PruneArgs) -> KimetsuResult<()> {
    let cwd = env::current_dir()?;
    let summary = project::prune_low_usefulness(
        &cwd,
        project::PruneOptions {
            scope: args.scope.clone(),
            min_uses: args.min_uses,
            max_ratio: args.max_ratio,
            apply: args.apply,
        },
    )?;

    if summary.candidates.is_empty() {
        println!(
            "no memories match the prune criteria (min_uses>={}, max_ratio<={:+.2}{})",
            args.min_uses,
            args.max_ratio,
            args.scope
                .as_deref()
                .map(|s| format!(", scope={s}"))
                .unwrap_or_default()
        );
        return Ok(());
    }

    let action = if args.apply { "pruning" } else { "would prune" };
    println!(
        "{action} {} memorie(s) (min_uses>={}, max_ratio<={:+.2}{}):",
        summary.candidates.len(),
        args.min_uses,
        args.max_ratio,
        args.scope
            .as_deref()
            .map(|s| format!(", scope={s}"))
            .unwrap_or_default()
    );
    for c in &summary.candidates {
        let ratio = c.usefulness_score as f64 / c.use_count.max(1) as f64;
        println!(
            "  {} [{}:{} uses={} usefulness={:+.1} ratio={:+.2}] {}",
            c.memory_id, c.scope, c.kind, c.use_count, c.usefulness_score, ratio, c.text
        );
    }
    if !args.apply {
        println!("dry-run; pass --apply to invalidate these memories");
    } else {
        println!(
            "summary: invalidated={} failed={}",
            summary.invalidated, summary.failed
        );
    }
    Ok(())
}

/// MP-5a/b: review handler. Three modes:
///
/// * `--accept-all` / `--reject-all` â€” non-interactive batch (MP-5a).
/// * No flags + stdin is a TTY â€” interactive walkthrough (MP-5b): one
///   proposal at a time, prompt `[a]ccept [r]eject [s]kip [q]uit`.
/// * No flags + stdin is NOT a TTY â€” error, so a misconfigured CI script
///   never silently hangs on a stdin read.
pub(crate) fn review_proposals(args: ReviewArgs) -> KimetsuResult<()> {
    if args.accept_all && args.reject_all {
        // clap's conflicts_with should already block this, but guard in
        // case it's bypassed via internal callers.
        return Err("--accept-all and --reject-all are mutually exclusive".into());
    }

    let cwd = env::current_dir()?;
    let pending = project::list_proposals(
        &cwd,
        project::ProposalFilter {
            scope: args.scope.clone(),
            kind: args.kind.clone(),
            from_run: args.from_run.clone(),
            min_confidence: args.min_confidence,
            status: Some("pending".to_string()),
            limit: args.limit,
            offset: 0,
        },
    )?;

    if pending.is_empty() {
        println!("no pending proposals matched the filters");
        return Ok(());
    }

    // MP-5b: no batch flag -> interactive walkthrough when stdin is a TTY.
    if !args.accept_all && !args.reject_all {
        if !io::stdin().is_terminal() {
            return Err(
                "memory review requires --accept-all / --reject-all when stdin is not a TTY".into(),
            );
        }
        return interactive_review_loop(&cwd, pending);
    }

    let action = if args.accept_all { "accept" } else { "reject" };
    println!(
        "review: would {action} {} pending proposal(s){}",
        pending.len(),
        if args.dry_run { " (dry-run)" } else { "" }
    );
    for p in &pending {
        println!(
            "  {} [{}:{} confidence={:.2} run={}] {}",
            p.proposal_id, p.scope, p.kind, p.proposed_confidence, p.run_id, p.text
        );
    }
    if args.dry_run {
        return Ok(());
    }

    let mut accepted = 0u32;
    let mut rejected = 0u32;
    let mut failed = 0u32;
    let resolved_reason = args
        .reason
        .clone()
        .unwrap_or_else(|| "batch_reject".to_string());

    for proposal in pending {
        if args.accept_all {
            match project::accept_proposal(
                &cwd,
                &proposal.proposal_id,
                project::AcceptOverrides::default(),
            ) {
                Ok(memory_id) => {
                    accepted += 1;
                    println!("accepted {} -> memory {memory_id}", proposal.proposal_id);
                }
                Err(err) => {
                    failed += 1;
                    eprintln!("skipped accept on {}: {err}", proposal.proposal_id);
                }
            }
        } else {
            match project::reject_proposal(&cwd, &proposal.proposal_id, Some(&resolved_reason)) {
                Ok(()) => {
                    rejected += 1;
                    println!(
                        "rejected {} (reason: {resolved_reason})",
                        proposal.proposal_id
                    );
                }
                Err(err) => {
                    failed += 1;
                    eprintln!("skipped reject on {}: {err}", proposal.proposal_id);
                }
            }
        }
    }

    println!("summary: accepted={accepted} rejected={rejected} failed={failed}");
    Ok(())
}

/// MP-5b: walk pending proposals one at a time, prompting the user for
/// each. Decisions persist immediately (idempotent via the existing
/// brain APIs), so `[q]uit` partway through leaves an accurate state.
///
/// Prompt vocabulary kept intentionally small for v0.2:
///   `a` accept | `r` reject | `s` skip | `q` quit | `?` re-print help
/// On `r` we ask for an optional reason on a follow-up line; empty input
/// keeps the default `reviewed_rejected_interactive`. Edits to scope /
/// kind / text are deferred to MP-5c â€” for now [s]kip + the existing
/// `memory accept --scope X` / `memory reject` commands cover that path.
pub(crate) fn interactive_review_loop(
    cwd: &Path,
    pending: Vec<project::ProposalRow>,
) -> KimetsuResult<()> {
    let stdin = io::stdin();
    let mut stdin_lock = stdin.lock();
    let stdout = io::stdout();
    let mut stdout_lock = stdout.lock();
    interactive_review_loop_inner(cwd, pending, &mut stdin_lock, &mut stdout_lock)
}

/// Pure plumbing for `interactive_review_loop`: takes injected I/O so the
/// loop can be driven from tests with scripted input. Production wiring
/// passes stdin/stdout locks; tests pass `Cursor::new(b"a\n...")` and a
/// `Vec<u8>` writer.
pub(crate) fn interactive_review_loop_inner<R: BufRead, W: Write>(
    cwd: &Path,
    pending: Vec<project::ProposalRow>,
    reader: &mut R,
    writer: &mut W,
) -> KimetsuResult<()> {
    let total = pending.len();
    let mut input = String::new();
    let mut accepted = 0u32;
    let mut rejected = 0u32;
    let mut skipped = 0u32;
    let mut failed = 0u32;

    writeln!(
        writer,
        "interactive review: {total} pending proposal(s). [a]ccept [r]eject [s]kip [q]uit [?]help"
    )?;

    for (idx, proposal) in pending.into_iter().enumerate() {
        writeln!(writer)?;
        writeln!(
            writer,
            "[{idx_one}/{total}] {pid}  scope={scope}  kind={kind}  confidence={conf:.2}  run={run}",
            idx_one = idx + 1,
            pid = proposal.proposal_id,
            scope = proposal.scope,
            kind = proposal.kind,
            conf = proposal.proposed_confidence,
            run = proposal.run_id,
        )?;
        writeln!(writer, "  text: {}", proposal.text)?;
        if !proposal.rationale.is_empty() {
            writeln!(writer, "  rationale: {}", proposal.rationale)?;
        }

        loop {
            write!(writer, "  > ")?;
            writer.flush().ok();
            input.clear();
            let read = reader.read_line(&mut input)?;
            if read == 0 {
                let processed = accepted + rejected + skipped + failed;
                let unprocessed = (total as u32).saturating_sub(processed);
                skipped += unprocessed;
                writeln!(writer, "(stdin closed; {unprocessed} proposal(s) skipped)")?;
                print_interactive_summary(writer, accepted, rejected, skipped, failed)?;
                return Ok(());
            }
            let choice = input.trim().to_ascii_lowercase();
            match choice.as_str() {
                "a" | "accept" => {
                    match project::accept_proposal(
                        cwd,
                        &proposal.proposal_id,
                        project::AcceptOverrides::default(),
                    ) {
                        Ok(memory_id) => {
                            accepted += 1;
                            writeln!(writer, "  -> accepted: memory {memory_id}")?;
                        }
                        Err(err) => {
                            failed += 1;
                            writeln!(writer, "  -> accept failed: {err}")?;
                        }
                    }
                    break;
                }
                "r" | "reject" => {
                    write!(writer, "  reason (enter to use default): ")?;
                    writer.flush().ok();
                    let mut reason_buf = String::new();
                    reader.read_line(&mut reason_buf)?;
                    let reason = reason_buf.trim();
                    let resolved = if reason.is_empty() {
                        "reviewed_rejected_interactive"
                    } else {
                        reason
                    };
                    match project::reject_proposal(cwd, &proposal.proposal_id, Some(resolved)) {
                        Ok(()) => {
                            rejected += 1;
                            writeln!(writer, "  -> rejected (reason: {resolved})")?;
                        }
                        Err(err) => {
                            failed += 1;
                            writeln!(writer, "  -> reject failed: {err}")?;
                        }
                    }
                    break;
                }
                "s" | "skip" | "" => {
                    skipped += 1;
                    writeln!(writer, "  -> skipped (still pending)")?;
                    break;
                }
                "q" | "quit" | "exit" => {
                    let processed = accepted + rejected + skipped + failed;
                    let unprocessed = (total as u32).saturating_sub(processed);
                    skipped += unprocessed;
                    writeln!(
                        writer,
                        "(quit; {} proposal(s) remain pending)",
                        unprocessed.saturating_sub(1)
                    )?;
                    print_interactive_summary(writer, accepted, rejected, skipped, failed)?;
                    return Ok(());
                }
                "?" | "h" | "help" => {
                    writeln!(
                        writer,
                        "  commands: [a]ccept  [r]eject  [s]kip (default)  [q]uit  [?]help"
                    )?;
                }
                other => {
                    writeln!(writer, "  unrecognized command '{other}'; try ? for help")?;
                }
            }
        }
    }

    print_interactive_summary(writer, accepted, rejected, skipped, failed)?;
    Ok(())
}

pub(crate) fn print_interactive_summary<W: Write>(
    writer: &mut W,
    accepted: u32,
    rejected: u32,
    skipped: u32,
    failed: u32,
) -> io::Result<()> {
    writeln!(writer)?;
    writeln!(
        writer,
        "summary: accepted={accepted} rejected={rejected} skipped={skipped} failed={failed}"
    )
}
