//! host hooks: context, stop, session, proactive.
//! Split out of main.rs (v2.5.1); implementations only — the clap
//! surface stays in main.rs.

#![allow(unused_imports)]
use std::env;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use kimetsu_brain::inject_policy;
use kimetsu_brain::project;
use kimetsu_core::KimetsuResult;
use kimetsu_core::memory::{MemoryKind, MemoryScope};

use crate::*;

pub(crate) fn brain_context_hook(args: ContextHookArgs) -> KimetsuResult<()> {
    use kimetsu_brain::context::ContextRequest;
    use std::io::Read;

    let workspace = args
        .workspace
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());

    // Read hook JSON from stdin
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).unwrap_or(0);

    // Parse the full hook payload once so we can extract both `prompt`
    // and `session_id` (Change A + Change B).
    let hook_payload: Option<serde_json::Value> = if input.trim().is_empty() {
        None
    } else {
        serde_json::from_str(input.trim()).ok()
    };

    // Change B: extract session_id — present in Claude Code's
    // UserPromptSubmit payload; absent in Codex / plain-text fallbacks.
    let session_id: Option<String> = hook_payload
        .as_ref()
        .and_then(|v| v.get("session_id"))
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string);

    // Extract the prompt text from the hook payload
    let prompt = match &hook_payload {
        Some(v) => v
            .get("prompt")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
        None if !input.trim().is_empty() => input.trim().to_string(), // plain-text fallback
        None => String::new(),
    };

    // v1.5 (Story 2.3): session-scoped cross-turn state, loaded here rather
    // than just before rendering because the warm-start fallback below has to
    // consult it ahead of every early return.
    let state_path = kimetsu_core::paths::ProjectPaths::discover(&workspace)
        .ok()
        .map(|p| {
            let cache_dir = kimetsu_core::paths::user_cache_dir_for(&p.repo_root);
            proactive_state::session_path(&cache_dir, session_id.as_deref())
        });
    let mut state = state_path
        .as_deref()
        .map(proactive_state::load)
        .unwrap_or_default();

    // v2.6: warm-start fallback for hosts with no session-start event (Codex,
    // Pi, OpenClaw). Those harnesses only expose a per-turn hook, so the repo
    // digest and episodic resume ride along with the session's first prompt
    // instead. Claude Code does not pass `--warm-on-first-prompt`: it already
    // gets the identical block from `brain session-start-hook`.
    let warm_start_block = if args.warm_on_first_prompt && state.warm_started_unix == 0 {
        warm_start_context(&workspace)
    } else {
        None
    };

    // Too short for retrieval to mean anything — but a first turn still
    // deserves its warm start, so hand that over before bailing.
    if prompt.len() < 10 {
        return flush_warm_start(warm_start_block, &mut state, state_path.as_deref());
    }

    let request = ContextRequest {
        stage: "localization".to_string(),
        query: prompt,
        budget_tokens: 2000,
        min_score: args.min_score,
        max_capsules: args.max_capsules,
        ..Default::default()
    };

    // Retrieval: try the warm daemon first (semantic); fall back to
    // floored-FTS on any miss (daemon disabled / unreachable / cold).
    let (bundle, retrieval_path) = match try_daemon_retrieve(&workspace, &request) {
        Some(b) => (b, "daemon"),
        None => match project::retrieve_context_lexical_readonly(&workspace, request.clone()) {
            Ok(b) => (b, "fts_fallback"),
            Err(_) => return Ok(()), // Brain not initialized — silent fail
        },
    };

    // C7: emit a context.served event BEFORE the early-return so misses are
    // logged. Best-effort (let _ =) — telemetry must never break the hook.
    // Gate behind KIMETSU_BRAIN_LOG_RETRIEVAL=0 opt-out (default ON).
    if std::env::var("KIMETSU_BRAIN_LOG_RETRIEVAL").as_deref() != Ok("0") {
        // Change A: load store_queries from project config best-effort.
        // Telemetry must never break the hook, so any config error just
        // falls through to the safe default (true = store the query).
        let store_queries = kimetsu_core::paths::ProjectPaths::discover(&workspace)
            .ok()
            .and_then(|paths| project::load_config(&paths).ok())
            .map(|cfg| cfg.learning.store_queries)
            .unwrap_or(true);

        let payload = build_served_event_payload(ServedEventArgs {
            query: &request.query,
            capsule_count: bundle.capsules.len(),
            top_score: bundle.top_score,
            skipped: bundle.skipped,
            stage: &request.stage,
            retrieval_path,
            store_queries,
            session_id: session_id.as_deref(),
        });
        let _ = project::log_telemetry_event(&workspace, "context.served", payload);
    }

    // Change C1: capture top-10 dropped MEMORY capsules to the rolling
    // sidecar. Best-effort — telemetry must never break the hook.
    // We capture AFTER the telemetry event so a slow sidecar write
    // doesn't block the event. Only capsules whose expansion_handle
    // starts with "memory:" are interesting for regret detection.
    {
        use kimetsu_brain::dropped_capsule;
        let cache_dir = kimetsu_core::paths::ProjectPaths::discover(&workspace)
            .ok()
            .map(|p| kimetsu_core::paths::user_cache_dir_for(&p.repo_root));
        if let Some(cache_dir) = cache_dir {
            let dropped_ids = bundle
                .excluded
                .iter()
                .filter(|c| c.expansion_handle.starts_with("memory:"))
                .filter_map(|c| {
                    c.expansion_handle
                        .strip_prefix("memory:")
                        .map(str::to_string)
                })
                .take(10);
            let now = dropped_capsule::now_secs();
            dropped_capsule::append_dropped(&cache_dir, dropped_ids, now);
        }
    }

    if bundle.skipped || bundle.capsules.is_empty() {
        // Nothing relevant to retrieve — zero output, except that the
        // warm-start block is not conditional on retrieval finding anything.
        return flush_warm_start(warm_start_block, &mut state, state_path.as_deref());
    }

    // v1.5 / F3 Pass B: load broker render-flags best-effort.
    // The hook must never fail on config errors — fallback to safe defaults.
    let (compress_capsules, session_dedupe, answer_grade_min_score) =
        kimetsu_core::paths::ProjectPaths::discover(&workspace)
            .ok()
            .and_then(|paths| project::load_config(&paths).ok())
            .map(|cfg| {
                (
                    cfg.broker.compress_capsules,
                    cfg.broker.session_dedupe,
                    cfg.broker.answer_grade_min_score,
                )
            })
            .unwrap_or((true, true, 0.92));

    // v1.5 (Story 2.3): session-scoped cross-turn dedupe, using the
    // proactive-state sidecar loaded above (also used by the proactive hooks)
    // to track which capsule handles were injected earlier this session.
    //
    // Apply soft dedupe: filter already-surfaced handles, but fall back to the
    // full set if filtering would leave nothing (a repeated top memory may still
    // be the right context). Uses the pure `dedupe_filter` function.
    let capsules_to_render: Vec<_> = if session_dedupe {
        let handles: Vec<&str> = bundle
            .capsules
            .iter()
            .map(|c| c.expansion_handle.as_str())
            .collect();
        let indices = proactive_state::dedupe_filter(&handles, &state);
        indices.into_iter().map(|i| &bundle.capsules[i]).collect()
    } else {
        bundle.capsules.iter().collect()
    };

    // F3 Pass B (3.3): pre-compute the answer-grade marker for the top capsule
    // (the first capsule in capsules_to_render after dedupe). The marker signals
    // to the model that it can act in one turn rather than re-verifying.
    //
    // STRICTLY ADDITIVE: this only changes the rendered prefix of the already-
    // top capsule. Ranking, floors, and which capsules were selected are never
    // touched. Suppressed (guard = None) when:
    //   a) the top capsule's score is below answer_grade_min_score (conservative
    //      default 0.92 — roughly the top 10% of scores on a well-populated brain),
    //   b) answer_grade_min_score > 1.0 (operator disabled the feature), or
    //   c) REGRET GUARD: the capsule's memory_id appears in the recent dropped
    //      sidecar — meaning the same memory was excluded by floors in a different
    //      recent retrieval context, indicating inconsistent scoring that makes
    //      the "verified answer" label overconfident. Read-only peek (best-effort).
    //
    // Note: the dropped sidecar tracks EXCLUDED capsules (those that did not
    // make the bundle). A capsule in bundle.capsules cannot be in the sidecar
    // for THIS retrieval pass, but it might appear there from a PRIOR retrieval
    // within the 2-hour window — that is the overconfidence signal we guard.
    let answer_grade_handle: Option<&str> = capsules_to_render
        .first()
        .filter(|top| top.score >= answer_grade_min_score && answer_grade_min_score <= 1.0)
        .and_then(|top| {
            // Regret guard: read-only peek at the dropped sidecar.
            // If the memory was recently dropped by floors (in any prior retrieval
            // this session window), do NOT label it answer-grade — the floors
            // gave conflicting signals, which means the confidence marker would
            // be misleading. Best-effort: any I/O error skips the guard (allows
            // the label) rather than breaking the hook.
            let memory_id = top.expansion_handle.strip_prefix("memory:").unwrap_or("");
            if memory_id.is_empty() {
                return None; // Non-memory capsules (repo_file, manifest) — skip guard
            }
            let in_dropped_sidecar = kimetsu_core::paths::ProjectPaths::discover(&workspace)
                .ok()
                .map(|paths| {
                    let cache_dir = kimetsu_core::paths::user_cache_dir_for(&paths.repo_root);
                    let sidecar_path = kimetsu_brain::dropped_capsule::sidecar_path(&cache_dir);
                    let state = kimetsu_brain::dropped_capsule::load(&sidecar_path);
                    state.entries.iter().any(|e| e.memory_id == memory_id)
                })
                .unwrap_or(false);
            if in_dropped_sidecar {
                None // Regret guard suppresses the answer-grade label
            } else {
                Some(top.expansion_handle.as_str())
            }
        });

    let mut additional_context = String::new();
    // v2.6: on hosts without a session-start event, the warm-start block leads
    // — the agent needs to know the repo before it is told what to recall.
    if let Some(block) = &warm_start_block {
        additional_context.push_str(block);
        additional_context.push_str("\n\n");
    }
    // v2.6: memory arrives looking like ground truth unless the framing says
    // otherwise, and MemSyco-Bench finds that deference is what makes most
    // memory systems score worse than no memory at all. See
    // `kimetsu_brain::framing`.
    additional_context.push_str(kimetsu_brain::framing::CONTEXT_HEADER);
    // v2.6: a time-ordered bundle looks like a relevance-ranked one whose
    // ranking has gone wrong unless the reader is told. Goes above the capsules
    // because it describes how to read them.
    if bundle.chronological {
        additional_context.push('\n');
        additional_context.push_str(kimetsu_brain::ordering::CHRONOLOGICAL_NOTE);
    }
    for (idx, capsule) in capsules_to_render.iter().enumerate() {
        // v1.5 (Story 2.1): render-time compression — runs AFTER retrieval and
        // reranking, purely on the injected text. Full summary untouched in DB.
        let rendered: String = if compress_capsules {
            kimetsu_brain::context::compress_for_render(&capsule.summary, 3)
        } else {
            capsule.summary.clone()
        };
        // Strip the "scope:kind - " prefix from the summary for readability
        let text = rendered
            .split(" - ")
            .nth(1)
            .map(str::to_string)
            .unwrap_or(rendered);
        additional_context.push('\n');
        // F3 Pass B (3.3): prepend the answer-grade marker to the first capsule
        // when it cleared the high-confidence threshold AND passed the regret
        // guard. Only the first rendered capsule (idx == 0) can be answer-grade
        // (it's the top-ranked capsule); subsequent capsules are never marked.
        if idx == 0 && answer_grade_handle.is_some() {
            additional_context.push_str("Verified answer from project memory: ");
        }
        additional_context.push_str(&text);
    }

    // v2.6: when the bundle collectively covers only part of the question, say
    // so and name what is missing. Without this a reader handed three capsules
    // that touch half the query cannot tell that from three that answer it,
    // and fills the gap by inference — which is what BEAM's abstention track
    // measures, and where Kimetsu scores worst.
    if let Some(notice) = kimetsu_brain::context::partial_evidence_notice(&bundle) {
        additional_context.push('\n');
        additional_context.push_str(&notice);
    }

    print_user_prompt_submit_context(&additional_context)?;

    // v1.5 (Story 2.3): persist newly surfaced handles so subsequent prompts
    // in the same session skip them. Best-effort — state write must never
    // break the hook's primary output.
    if session_dedupe {
        for capsule in &capsules_to_render {
            if !capsule.expansion_handle.is_empty() {
                state.mark_surfaced(&capsule.expansion_handle);
            }
        }
    }
    if warm_start_block.is_some() {
        state.warm_started_unix = proactive_state::now_unix();
    }
    if session_dedupe || warm_start_block.is_some() {
        if let Some(ref path) = state_path {
            proactive_state::save(path, &state);
        }
    }

    Ok(())
}

/// Emit a warm-start-only injection and mark the session warmed.
///
/// Used by [`brain_context_hook`]'s early returns: a short prompt, or a
/// retrieval that found nothing, must not swallow the first-turn warm start on
/// hosts that have no session-start event. A `None` block (not a first turn, or
/// the host has its own session-start hook) makes this a silent no-op, which is
/// the pre-v2.6 behaviour.
fn flush_warm_start(
    block: Option<String>,
    state: &mut proactive_state::SessionState,
    state_path: Option<&Path>,
) -> KimetsuResult<()> {
    let Some(block) = block else {
        return Ok(());
    };
    print_user_prompt_submit_context(&block)?;
    state.warm_started_unix = proactive_state::now_unix();
    if let Some(path) = state_path {
        proactive_state::save(path, state);
    }
    Ok(())
}

/// v1.5: inputs for the `context.served` telemetry payload builder.
///
/// Grouped into a struct to keep [`build_served_event_payload`] under
/// the clippy `too_many_arguments` threshold and to make call-sites
/// self-documenting.
pub struct ServedEventArgs<'a> {
    /// Raw retrieval query text.
    pub query: &'a str,
    /// How many capsules were included in the bundle.
    pub capsule_count: usize,
    /// Best composite score before the skip check.
    pub top_score: f32,
    /// True when the top score was below `min_score` (no injection).
    pub skipped: bool,
    /// Retrieval stage tag (e.g. `"localization"`).
    pub stage: &'a str,
    /// `"daemon"` or `"fts_fallback"`.
    pub retrieval_path: &'a str,
    /// When true, include the raw query text in the payload.
    /// When false, only the hash is stored (pre-v1.5 behavior).
    pub store_queries: bool,
    /// Claude Code session id from the hook payload, when available.
    /// Codex and plain-text fallbacks may omit it.
    pub session_id: Option<&'a str>,
}

/// v1.5: pure builder for the `context.served` telemetry payload.
///
/// Extracted so the logic can be unit-tested without hitting the FS or
/// spawning hooks. Always emits `query_hash` for backward compatibility;
/// adds `query` only when `args.store_queries` is true; adds `session_id`
/// only when present.
pub fn build_served_event_payload(args: ServedEventArgs<'_>) -> serde_json::Value {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut h = DefaultHasher::new();
    args.query.hash(&mut h);
    let query_hash = format!("{:016x}", h.finish());

    let mut map = serde_json::Map::new();
    map.insert("query_hash".into(), serde_json::json!(query_hash));
    if args.store_queries {
        map.insert("query".into(), serde_json::json!(args.query));
    }
    map.insert(
        "capsule_count".into(),
        serde_json::json!(args.capsule_count),
    );
    map.insert("top_score".into(), serde_json::json!(args.top_score));
    map.insert("skipped".into(), serde_json::json!(args.skipped));
    map.insert("stage".into(), serde_json::json!(args.stage));
    map.insert(
        "retrieval_path".into(),
        serde_json::json!(args.retrieval_path),
    );
    if let Some(sid) = args.session_id {
        map.insert("session_id".into(), serde_json::json!(sid));
    }
    serde_json::Value::Object(map)
}

pub(crate) fn print_user_prompt_submit_context(additional_context: &str) -> KimetsuResult<()> {
    let output = user_prompt_submit_context_output(additional_context);
    println!("{}", serde_json::to_string(&output)?);
    Ok(())
}

pub(crate) fn user_prompt_submit_context_output(additional_context: &str) -> serde_json::Value {
    serde_json::json!({
        "continue": true,
        "hookSpecificOutput": {
            "hookEventName": "UserPromptSubmit",
            "additionalContext": additional_context,
        },
    })
}

/// v0.7: Claude Code Stop hook. Reads the session JSON from stdin,
/// counts `kimetsu_brain_record` calls in the transcript, and prints a
/// summary banner. v0.8.5: reads the real `transcript_path` (a JSONL
/// file Claude Code writes) instead of a non-existent inline array, and
/// — when nothing was recorded in a non-trivial session — points at the
/// memory-harvester subagent. Silent exit for short sessions.
/// v1.5: when the session had ≥1 citation, appends a savings sentence to
/// the `systemMessage` banner.
pub(crate) fn brain_stop_hook(args: StopHookArgs) -> KimetsuResult<()> {
    use std::io::Read;

    let workspace = args
        .workspace
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());

    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).unwrap_or(0);

    // Parse the session JSON from Claude Code's Stop hook payload.
    let session: serde_json::Value =
        serde_json::from_str(input.trim()).unwrap_or(serde_json::Value::Null);

    // Count transcript messages + recorded lessons. Claude Code's Stop
    // hook sends a `transcript_path` to a JSONL file (one message per
    // line), NOT an inline array — stream it line-by-line so a long
    // session's transcript (tens of MB) never lands in memory at once.
    // Fall back to an inline `transcript` array for other harnesses/tests.
    let transcript_path = session
        .get("transcript_path")
        .and_then(|v| v.as_str())
        .filter(|p| !p.trim().is_empty())
        .map(str::to_string);
    let (turn_count, recorded) = match transcript_path.as_deref() {
        Some(path) => count_transcript_jsonl(path),
        None => {
            let messages: Vec<serde_json::Value> = session
                .get("transcript")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            (messages.len(), count_brain_record_calls(&messages))
        }
    };

    // v1.5: compute per-session ROI (best-effort; errors are silently ignored).
    let sid = session.get("session_id").and_then(|v| v.as_str());
    let session_savings = compute_stop_hook_savings(&workspace, sid);
    // S2.1: compute re-tune trigger cue (best-effort; never blocks the hook).
    let retune_cue = compute_stop_hook_retune_cue(&workspace);

    if recorded > 0 {
        return emit_stop_hook_json(stop_lessons_recorded_json_with_savings_and_tune(
            recorded,
            session_savings.as_deref(),
            retune_cue.as_deref(),
        ));
    }
    // Short sessions exit silently — no nagging for quick lookups. The
    // count is transcript *lines* (user/assistant/tool messages), so the
    // bar is set above a trivial lookup exchange.
    const MIN_TRANSCRIPT_LINES: usize = 12;
    if turn_count < MIN_TRANSCRIPT_LINES {
        return Ok(());
    }

    // Non-trivial session, nothing recorded. When auto-harvest is on and
    // we haven't already cued a harvest this session (e.g. via the
    // PostToolUse resolution cue), point at the harvester subagent.
    // `stop_hook_active` means we're already in a stop continuation —
    // don't re-cue.
    let stop_active = session
        .get("stop_hook_active")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let paths = kimetsu_core::paths::ProjectPaths::discover(&workspace).ok();
    let auto_harvest = paths
        .as_ref()
        .and_then(|p| project::load_config(p).ok())
        .map(|c| c.learning.auto_harvest)
        .unwrap_or(true);
    let distiller_enabled = distiller::resolve_pipeline_distiller(&workspace).is_some();
    let state_path = paths.as_ref().map(|p| {
        let cache_dir = kimetsu_core::paths::user_cache_dir_for(&p.repo_root);
        proactive_state::session_path(&cache_dir, sid)
    });

    if args.distill_on_stop
        && distiller_enabled
        && !stop_active
        && let Some(path) = transcript_path.as_deref()
    {
        let mut state = state_path
            .as_ref()
            .map(|path| proactive_state::load(path))
            .unwrap_or_default();
        if !state.harvest_cued() {
            let _ = distiller::run_distiller_for_transcript(&workspace, path);
            if let Some(state_path) = state_path.as_ref() {
                state.note_harvest_cue(proactive_state::now_unix());
                proactive_state::save(state_path, &state);
            }
            return Ok(());
        }
    }

    if should_emit_stop_harvest_cue(auto_harvest, distiller_enabled)
        && !stop_active
        && let Some(paths) = paths.as_ref()
    {
        let state_path = state_path.unwrap_or_else(|| {
            let cache_dir = kimetsu_core::paths::user_cache_dir_for(&paths.repo_root);
            proactive_state::session_path(&cache_dir, sid)
        });
        let mut state = proactive_state::load(&state_path);
        if !state.harvest_cued() {
            emit_stop_hook_json(stop_harvest_cue_json())?;
            state.note_harvest_cue(proactive_state::now_unix());
            proactive_state::save(&state_path, &state);
            return Ok(());
        }
    }

    emit_stop_hook_json(stop_no_lessons_json_with_savings_and_tune(
        session_savings.as_deref(),
        retune_cue.as_deref(),
    ))
}

/// v1.5: Compute a per-session savings sentence for the Stop hook.
///
/// Best-effort: returns `None` on any error (DB not found, no data, etc.)
/// so the hook never fails due to ROI computation.
///
/// Returns `None` also when there are zero citations this session (silence
/// is the correct behavior — we don't dilute the harvest cue).
pub(crate) fn compute_stop_hook_savings(
    workspace: &Path,
    session_id: Option<&str>,
) -> Option<String> {
    use kimetsu_brain::roi::session_roi;

    let (paths, config, conn) = kimetsu_brain::project::load_project_readonly(workspace).ok()?;
    let _ = paths; // suppress unused warning
    let sr = session_roi(
        &conn,
        session_id,
        &config.model.model,
        config.model.price_per_mtok,
    )?;
    Some(sr.savings_sentence())
}

/// S2.1: Compute a re-tune proposal one-liner for the Stop hook.
///
/// Returns `Some(line)` when a re-tune is proposed (corpus milestone or drift
/// trigger), `None` otherwise.  Best-effort — any error returns `None` so the
/// stop hook is never disrupted.
pub(crate) fn compute_stop_hook_retune_cue(workspace: &Path) -> Option<String> {
    use kimetsu_brain::tune::compute_retune_trigger;

    let (paths, _, conn) = kimetsu_brain::project::load_project_readonly(workspace).ok()?;
    let trigger = compute_retune_trigger(&conn, &paths.kimetsu_dir).ok()?;
    if !trigger.should_retune {
        return None;
    }
    let reason = if trigger.corpus_milestone_triggered {
        format!(
            "Brain grew +{} memories since last tune — run `kimetsu brain tune`",
            trigger.memories_added_since_tune
        )
    } else {
        format!(
            "Retrieval regret rate {:.0}% (24 h) — run `kimetsu brain tune`",
            trigger.regret_rate * 100.0
        )
    };
    Some(reason)
}

/// Emit a Claude Code `Stop`-hook result on stdout. Claude Code validates a
/// Stop hook's stdout as JSON (the advanced control object), so the hook must
/// never print bare text — doing so trips "hook returned invalid stop hook
/// JSON output". A `Null` value prints nothing (silent allow-stop).
pub(crate) fn emit_stop_hook_json(value: serde_json::Value) -> KimetsuResult<()> {
    if !value.is_null() {
        println!("{}", serde_json::to_string(&value)?);
    }
    Ok(())
}

/// User-facing banner confirming how many lessons were recorded. Surfaced via
/// `systemMessage` (shown to the user; it does not re-enter the model).
/// Kept for test compatibility; production code uses `_with_savings` directly.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn stop_lessons_recorded_json(recorded: usize) -> serde_json::Value {
    stop_lessons_recorded_json_with_savings(recorded, None)
}

/// v1.5: lessons-recorded banner with optional savings sentence appended.
/// When `savings` is `Some`, it is appended after the lessons line.
/// The original `stop_lessons_recorded_json` delegates here so existing tests
/// continue to pass unchanged.
pub(crate) fn stop_lessons_recorded_json_with_savings(
    recorded: usize,
    savings: Option<&str>,
) -> serde_json::Value {
    stop_lessons_recorded_json_with_savings_and_tune(recorded, savings, None)
}

/// The end-of-session harvest cue. Uses `decision: "block"` so the cue text
/// actually re-enters the model (plain stdout never reaches it in a Stop
/// hook), prompting it to dispatch the harvester before the turn ends. The
/// `stop_hook_active` + persisted `harvest_cued` guards keep this to one cue
/// per session, so blocking cannot loop.
pub(crate) fn stop_harvest_cue_json() -> serde_json::Value {
    serde_json::json!({
        "decision": "block",
        "reason": "[kimetsu-harvest] No lessons recorded this non-trivial session. If anything \
                   durable was learned, run the kimetsu-memory-harvester agent in the background \
                   to capture it — otherwise call kimetsu_brain_record.",
    })
}

/// User-facing fallback nudge when nothing was recorded and the harvest cue
/// path did not fire. Informational only, so it uses `systemMessage`.
/// Kept for test compatibility; production code uses `_with_savings` directly.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn stop_no_lessons_json() -> serde_json::Value {
    stop_no_lessons_json_with_savings(None)
}

/// v1.5: no-lessons nudge with optional savings sentence appended.
pub(crate) fn stop_no_lessons_json_with_savings(savings: Option<&str>) -> serde_json::Value {
    stop_no_lessons_json_with_savings_and_tune(savings, None)
}

/// S2.1: no-lessons nudge with optional savings + re-tune cue.
pub(crate) fn stop_no_lessons_json_with_savings_and_tune(
    savings: Option<&str>,
    retune_cue: Option<&str>,
) -> serde_json::Value {
    let base =
        "[Kimetsu] No lessons recorded. After non-trivial solutions, call kimetsu_brain_record.";
    let mut parts: Vec<&str> = vec![base];
    if let Some(s) = savings {
        parts.push(s);
    }
    // S2.1: append re-tune cue if triggered.
    let retune_owned;
    if let Some(cue) = retune_cue {
        retune_owned = format!("[Tune] {cue}.");
        parts.push(&retune_owned);
    }
    let msg = parts.join(" ");
    serde_json::json!({ "systemMessage": msg })
}

/// S2.1: lessons-recorded banner with optional savings + re-tune cue.
pub(crate) fn stop_lessons_recorded_json_with_savings_and_tune(
    recorded: usize,
    savings: Option<&str>,
    retune_cue: Option<&str>,
) -> serde_json::Value {
    let base = format!(
        "[Kimetsu] {} lesson{} recorded.",
        recorded,
        if recorded == 1 { "" } else { "s" }
    );
    let mut parts: Vec<String> = vec![base];
    if let Some(s) = savings {
        parts.push(s.to_string());
    }
    if let Some(cue) = retune_cue {
        parts.push(format!("[Tune] {cue}."));
    }
    let msg = parts.join(" ");
    serde_json::json!({ "systemMessage": msg })
}

/// The end-of-session harvest cue fires only when auto-harvest is on AND
/// the credentialed distiller is not handling end-of-session itself.
pub(crate) fn should_emit_stop_harvest_cue(auto_harvest: bool, distiller_enabled: bool) -> bool {
    auto_harvest && !distiller_enabled
}

/// Count `kimetsu_brain_record` tool-use blocks across transcript
/// messages. Tolerates both the inline message shape (`content` array)
/// and Claude Code's JSONL shape (`message.content` array). The tool
/// name is matched against both the bare `kimetsu_brain_record` and the
/// MCP-namespaced `mcp__kimetsu__kimetsu_brain_record` form that real
/// Claude Code transcripts actually carry.
pub(crate) fn count_brain_record_calls(messages: &[serde_json::Value]) -> usize {
    messages
        .iter()
        .map(|m| {
            let content = m
                .get("content")
                .or_else(|| m.get("message").and_then(|msg| msg.get("content")))
                .and_then(|c| c.as_array());
            content
                .map(|blocks| {
                    blocks
                        .iter()
                        .filter(|b| {
                            b.get("name")
                                .and_then(|n| n.as_str())
                                .is_some_and(is_brain_record_tool)
                        })
                        .count()
                })
                .unwrap_or(0)
        })
        .sum()
}

/// True for the `kimetsu_brain_record` tool under either the bare name
/// or any MCP namespace prefix (`mcp__<server>__kimetsu_brain_record`).
pub(crate) fn is_brain_record_tool(name: &str) -> bool {
    name == "kimetsu_brain_record" || name.ends_with("__kimetsu_brain_record")
}

/// Stream a transcript JSONL file, returning `(message_count,
/// brain_record_count)` without loading the whole file into memory.
/// Best-effort: an unreadable file or malformed line is skipped, never
/// fatal (a hook must not break the agent's turn). A leading UTF-8 BOM on
/// the first line is tolerated.
pub(crate) fn count_transcript_jsonl(path: &str) -> (usize, usize) {
    use std::io::BufRead;
    let Ok(file) = std::fs::File::open(path) else {
        return (0, 0);
    };
    let mut turns = 0usize;
    let mut records = 0usize;
    for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
        let line = line.trim().trim_start_matches('\u{feff}');
        if line.is_empty() {
            continue;
        }
        turns += 1;
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(line) {
            records += count_brain_record_calls(std::slice::from_ref(&value));
        }
    }
    (turns, records)
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum ProactiveEvent {
    PreTool,
    PostTool,
}

impl ProactiveEvent {
    fn hook_event_name(self) -> &'static str {
        match self {
            ProactiveEvent::PreTool => "PreToolUse",
            ProactiveEvent::PostTool => "PostToolUse",
        }
    }
}

/// Harness-agnostic fields pulled from a PreToolUse/PostToolUse hook
/// payload. Both Claude Code and Codex send this superset; parse
/// defensively so a missing/odd field just disables the relevant path.
pub(crate) struct HookToolInput {
    session_id: Option<String>,
    tool_name: Option<String>,
    command: Option<String>,
    tool_response: Option<String>,
    /// v2.6: the tool's exit code when the harness reports one. Authoritative
    /// for failure detection — see [`crate::tool_outcome`].
    exit_code: Option<i64>,
    /// F3 Pass B (3.5): file path from `tool_input.file_path` (ReadFile,
    /// EditFile, etc.). Absent for Bash and other non-file tools. Used by
    /// the proactive pre-fetch path when `broker.proactive_prefetch = true`
    /// to augment the retrieval query with the file being operated on.
    tool_file_path: Option<String>,
}

pub(crate) fn parse_hook_tool_input(raw: &str) -> HookToolInput {
    let v: serde_json::Value = serde_json::from_str(raw.trim()).unwrap_or(serde_json::Value::Null);
    let str_field = |key: &str| {
        v.get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .filter(|s| !s.trim().is_empty())
    };
    let command = v
        .get("tool_input")
        .and_then(|ti| ti.get("command"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty());
    // F3 Pass B (3.5): extract file_path from tool_input for pre-fetch query
    // augmentation. Covers ReadFile, EditFile, WriteFile, and similar tools
    // whose Claude Code / Codex tool_input carries a `file_path` field.
    let tool_file_path = v
        .get("tool_input")
        .and_then(|ti| ti.get("file_path"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty());
    // tool_response may be a string or a structured object; stringify
    // objects so failure detection still has something to scan.
    let tool_response = match v.get("tool_response") {
        Some(serde_json::Value::String(s)) if !s.trim().is_empty() => Some(s.clone()),
        Some(serde_json::Value::Null) | None => None,
        Some(other) => Some(other.to_string()),
    };
    // v2.6: an exit code, when the harness reports one, settles the
    // did-it-fail question outright. Harnesses spell it differently and may
    // nest it under `tool_response`, so check the spellings we have seen.
    let exit_code = ["exit_code", "exitCode", "returncode", "status"]
        .iter()
        .find_map(|key| {
            v.get(key)
                .or_else(|| v.get("tool_response").and_then(|tr| tr.get(key)))
                .and_then(serde_json::Value::as_i64)
        });
    HookToolInput {
        session_id: str_field("session_id"),
        tool_name: str_field("tool_name"),
        command,
        tool_response,
        tool_file_path,
        exit_code,
    }
}

/// v0.8: proactive PreToolUse / PostToolUse hook. Shared by both
/// events. Lexical-FTS-only retrieval, very high score floor, one
/// capsule, per-session dedupe + refractory + loop detection. Always
/// exits 0; emits hook JSON only on a confident, novel match.
pub(crate) fn proactive_hook(event: ProactiveEvent, args: ProactiveHookArgs) -> KimetsuResult<()> {
    use kimetsu_brain::context::ContextRequest;
    use std::io::Read;

    let workspace = args
        .workspace
        .clone()
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());

    // Resolve the .kimetsu dir; if there's no brain here, stay silent.
    let Ok(paths) = kimetsu_core::paths::ProjectPaths::discover(&workspace) else {
        return Ok(());
    };
    // Honor the configured embedder id for consistency (proactive
    // retrieval is lexical-only, but this keeps labels coherent). Also
    // capture the auto-harvest toggle, render flags, and F3 Pass B toggles.
    let (auto_harvest, compress_capsules, proactive_prefetch) = match project::load_config(&paths) {
        Ok(config) => {
            kimetsu_brain::embeddings::apply_embedder_selection(Some(&config.embedder.model));
            (
                config.learning.auto_harvest,
                config.broker.compress_capsules,
                config.broker.proactive_prefetch,
            )
        }
        // Fallback: safe defaults — proactive_prefetch OFF (zero behaviour change)
        Err(_) => (true, true, false),
    };

    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).unwrap_or(0);
    if input.trim().is_empty() {
        return Ok(());
    }
    let hook = parse_hook_tool_input(&input);

    // Defensive tool-name gate (the hook matcher should already scope
    // to Bash, but be safe across harness quirks).
    //
    // F3 Pass B (3.5): when proactive_prefetch is ON, relax the Bash-only gate
    // so file-tool PreToolUse calls (ReadFile, EditFile, WriteFile, …) can also
    // trigger a lightweight file-path-based pre-fetch. The PostToolUse path is
    // unchanged (still Bash-only — file tools don't produce failure output).
    // When proactive_prefetch is OFF (default), the gate is unchanged: only
    // Bash tool calls are processed (zero behaviour change).
    let is_bash = hook
        .tool_name
        .as_deref()
        .map(|n| n.eq_ignore_ascii_case("bash"))
        .unwrap_or(true); // no tool_name → assume Bash (old harness compat)
    let allow_non_bash = proactive_prefetch && matches!(event, ProactiveEvent::PreTool);
    if !is_bash && !allow_non_bash {
        return Ok(());
    }

    let now = proactive_state::now_unix();
    let proactive_cache_dir = kimetsu_core::paths::user_cache_dir_for(&paths.repo_root);
    proactive_state::gc(&proactive_cache_dir, now);

    let state_path =
        proactive_state::session_path(&proactive_cache_dir, hook.session_id.as_deref());
    let mut state = proactive_state::load(&state_path);

    // v0.8.5: PostToolUse success — if this command failed earlier this
    // session and just succeeded, that's a resolved failure (a learnable
    // moment). Cue the agent (throttled) to harvest the lesson, then exit.
    if matches!(event, ProactiveEvent::PostTool) {
        let resp = hook.tool_response.as_deref().unwrap_or("");
        if !crate::tool_outcome::classify(resp, hook.exit_code).failed {
            let norm = proactive_state::normalize_command(hook.command.as_deref().unwrap_or(""));
            if auto_harvest
                && !norm.is_empty()
                && state.had_prior_failure(&norm)
                && !state.harvest_in_refractory(now, proactive_state::HARVEST_REFRACTORY_SECS)
            {
                let cmd = hook.command.as_deref().unwrap_or("the command");
                let cue = format!(
                    "[kimetsu-harvest] You just resolved a previously failing command (`{cmd}`). \
                     If this revealed a durable, generalizable lesson, run the \
                     kimetsu-memory-harvester agent in the background \
                     to record it via kimetsu_brain_record."
                );
                print_tool_use_context(event, &cue)?;
                state.note_harvest_cue(now);
                state.clear_failure(&norm);
            }
            proactive_state::save(&state_path, &state);
            return Ok(());
        }
    }

    // Build the retrieval query + actionable kinds per event.
    //
    // F3 Pass B (3.5): when `broker.proactive_prefetch = true`, the PreToolUse
    // query is augmented with the tool's `file_path` (e.g. the file being read
    // or edited). This lightweight warm surfaces memories relevant to the file
    // BEFORE the agent operates on it, rather than waiting for a failure.
    //
    // When `proactive_prefetch = false` (default), no augmentation happens and
    // PreToolUse behaviour is identical to before this flag existed. The same
    // floors (min_score, refractory, dedupe) gate the result — this is strictly
    // additive. Default-on graduation waits for regret data (Epic S2).
    // How strong the evidence of failure was, as a policy feature. PreToolUse
    // is a prediction rather than an observation, so it carries none.
    let mut evidence = 0.0f32;
    // v2.6: which surface this injection came from, recorded so the surfaces
    // can be judged separately. See `inject_policy::surface_acceptance`.
    let mut surface = inject_policy::Surface::PreToolCommand;
    let (query, kinds, error_sig): (String, &[&str], Option<String>) = match event {
        ProactiveEvent::PreTool => {
            // F3 Pass B (3.5): build the PreToolUse query from command and/or
            // file_path depending on the proactive_prefetch flag.
            //
            // proactive_prefetch OFF (default):
            //   - No command → silent exit (identical to pre-F3 behaviour).
            //   - Command present → use command as query (identical to pre-F3).
            //   - file_path is NEVER consulted (zero behaviour change).
            //
            // proactive_prefetch ON:
            //   - No command AND no file_path → silent exit.
            //   - No command but file_path present → file_path-only query.
            //   - Command present → command + file_path (if any) concatenated.
            let cmd_opt = hook.command.as_deref();
            let fp_opt = if proactive_prefetch {
                hook.tool_file_path.as_deref().filter(|s| s.len() > 4)
            } else {
                None
            };
            // The file path is what makes this a *prediction* rather than a
            // reaction, so any query it contributed to is a prefetch — that is
            // the surface whose noise the flag's graduation turns on.
            if fp_opt.is_some() {
                surface = inject_policy::Surface::PreToolPrefetch;
            }
            let query = match (cmd_opt, fp_opt) {
                (Some(cmd), Some(fp)) => format!("{cmd} {fp}"),
                (Some(cmd), None) => cmd.to_string(),
                (None, Some(fp)) => fp.to_string(),
                (None, None) => return Ok(()), // nothing to query on
            };
            (query, &["failure_pattern", "convention"], None)
        }
        ProactiveEvent::PostTool => {
            surface = inject_policy::Surface::PostTool;
            let resp = hook.tool_response.as_deref().unwrap_or("");
            // v2.6: exit code > toolchain parser > substring scan. The old
            // ten-word scan fired on every passing test suite, because
            // "0 failed" contains "failed".
            let outcome = crate::tool_outcome::classify(resp, hook.exit_code);
            if !outcome.failed {
                return Ok(()); // only react to failures
            }
            let cmd = hook.command.as_deref().unwrap_or("");
            // A toolchain parser extracts the actual diagnostic, which is a far
            // better retrieval query than the first line containing "error".
            let query = match outcome.signature.as_deref() {
                Some(sig) => format!("{sig} {cmd}"),
                None => format!("{resp} {cmd}"),
            };
            evidence = match outcome.evidence {
                crate::tool_outcome::Evidence::Heuristic => 0.0,
                crate::tool_outcome::Evidence::Toolchain => 0.5,
                crate::tool_outcome::Evidence::ExitCode => 1.0,
            };
            (
                query,
                &["failure_pattern", "command", "convention"],
                outcome.signature,
            )
        }
    };

    // Record this command, decide loop mode (state loaded above).
    let norm = proactive_state::normalize_command(hook.command.as_deref().unwrap_or(&query));
    let seen_count = state.note_command(&norm, error_sig.as_deref(), now);
    let loop_mode = seen_count >= proactive_state::LOOP_THRESHOLD;

    // Refractory throttle — unless the agent is clearly looping, stay
    // quiet for a window after the last injection. Persist the loop
    // counter increment even on a silent exit.
    if !loop_mode && state.in_refractory(now, args.refractory_secs) {
        proactive_state::save(&state_path, &state);
        return Ok(());
    }

    // v2.6: retrieve down to a permissive recall floor and let the injection
    // policy make the call, instead of hard-coding the threshold into
    // retrieval. The floor still abstains on obvious noise, so the cheap path
    // stays cheap; what changes is that the *decision* is now learned rather
    // than a constant. The CLI flags remain the floor's lower bound so an
    // operator who raises them still gets a stricter hook.
    let recall_floor = args
        .min_score
        .min(args.loop_min_score)
        .min(inject_policy::POLICY_RECALL_FLOOR);

    let request = ContextRequest {
        stage: "localization".to_string(),
        query,
        budget_tokens: 600,
        min_score: recall_floor,
        max_capsules: args.max_capsules.max(1),
        kinds: kinds.iter().map(|k| k.to_string()).collect(),
        ..Default::default()
    };

    let bundle = match project::retrieve_proactive_readonly(&workspace, request) {
        Ok(b) => b,
        Err(_) => {
            proactive_state::save(&state_path, &state);
            return Ok(());
        }
    };

    let Some(capsule) = bundle
        .capsules
        .iter()
        .find(|c| !state.is_surfaced(&c.expansion_handle))
    else {
        // Nothing relevant, or the only match already surfaced this
        // session (it's already in working memory).
        proactive_state::save(&state_path, &state);
        return Ok(());
    };

    // Is this worth interrupting for? An untrained brain answers exactly as
    // the old fixed threshold did; a trained one has learned from which of its
    // past injections the agent actually went on to cite.
    let features = inject_policy::Features {
        score: capsule.score,
        loop_mode: if loop_mode { 1.0 } else { 0.0 },
        is_failure_pattern: if capsule.summary.contains("failure_pattern") {
            1.0
        } else {
            0.0
        },
        novelty: if state.injection_count() == 0 {
            1.0
        } else {
            1.0 / (1.0 + state.injection_count() as f32)
        },
        repeat_count: (seen_count.min(5) as f32) / 5.0,
        recovery: state.recovery_fraction(now, args.refractory_secs),
        evidence,
    };
    let policy = inject_policy::load(&paths.kimetsu_dir);
    let should_inject = policy.should_inject(&features);

    // Record the decision either way: a suppressed injection is as much a
    // training sample as one that fired, provided it is labelled as such.
    inject_policy::record_injection(
        &workspace,
        capsule
            .expansion_handle
            .strip_prefix("memory:")
            .unwrap_or(&capsule.expansion_handle),
        &features,
        should_inject,
        surface,
    );

    if !should_inject {
        proactive_state::save(&state_path, &state);
        return Ok(());
    }

    // v1.5 (Story 2.1): render-time compression for the proactive hook.
    // Runs AFTER retrieval — ranking and stored text are unaffected.
    let rendered: String = if compress_capsules {
        kimetsu_brain::context::compress_for_render(&capsule.summary, 3)
    } else {
        capsule.summary.clone()
    };
    let body = rendered
        .split(" - ")
        .nth(1)
        .map(str::to_string)
        .unwrap_or(rendered);
    let header = proactive_header(event, loop_mode);
    // The suffix rather than a second line: this hook interrupts work already
    // underway on a one-capsule budget, and a preamble longer than the memory
    // reads as noise.
    let additional_context = format!(
        "{header}{}\n{body}",
        kimetsu_brain::framing::PROACTIVE_SUFFIX
    );

    print_tool_use_context(event, &additional_context)?;

    state.mark_surfaced(&capsule.expansion_handle);
    state.record_injection(now);
    proactive_state::save(&state_path, &state);
    Ok(())
}

pub(crate) fn proactive_header(event: ProactiveEvent, loop_mode: bool) -> &'static str {
    match (event, loop_mode) {
        (_, true) => {
            "You appear to be repeating a failing command. Kimetsu brain recalls a relevant lesson:"
        }
        (ProactiveEvent::PreTool, false) => {
            "Kimetsu brain — a relevant prior failure for this command:"
        }
        (ProactiveEvent::PostTool, false) => "Kimetsu brain — a known fix for this failure:",
    }
}

pub(crate) fn print_tool_use_context(
    event: ProactiveEvent,
    additional_context: &str,
) -> KimetsuResult<()> {
    // Non-blocking inject on both harnesses: hookSpecificOutput.
    // additionalContext with the matching hookEventName. We never set
    // permissionDecision / decision:block — proactive recall informs,
    // it does not gate.
    let output = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": event.hook_event_name(),
            "additionalContext": additional_context,
        },
    });
    println!("{}", serde_json::to_string(&output)?);
    Ok(())
}
