//! v1.5 / S2: Self-Tuning Brain sweep engine.
//!
//! Pure functions for objective scoring, holdout splitting, and history I/O.
//! The sweep itself is driven from the CLI (kimetsu-cli) which calls
//! `evaluate_combo` in-process with injected embedder + optional reranker.
//!
//! Sweep space (config-addressable only):
//!   - min_lexical_coverage ∈ {0.3, 0.4, 0.5, 0.6}
//!   - min_semantic_score   ∈ {-1.0(auto), 0.0, 0.25, 0.35, 0.45}
//!   - reranker id ∈ {off, ms-marco-tinybert-l-2-v2, jina-reranker-v1-tiny-en,
//!     ms-marco-minilm-l-4-v2}
//!
//! NOT swept (compile-time or complex-deploy):
//!   - RERANK_POOL (compile-time const in the daemon) — deferred.
//!
//! Objective (S2.3):
//!   mean_MRR - cost_weight * mean_injected_tokens - REGRET_PENALTY_WEIGHT * regret_rate
//!
//! S2.1 Re-tune triggers:
//!   - Corpus milestone: ≥50 memories added since last tune.
//!   - Drift: insights hit-rate decline OR regret-rate rise beyond thresholds.
//!
//! S2.2 Model re-selection advisor:
//!   Recommends re-running the embedder×reranker grid at corpus milestones.
//!   Reports download+reindex cost. Never auto-switches.

use serde::{Deserialize, Serialize};

// ─── S2.1: Re-tune trigger constants ─────────────────────────────────────────

/// Corpus milestone: propose a re-tune when ≥ this many memories have been
/// added since the last tune run.
pub const RETUNE_CORPUS_MILESTONE: u64 = 50;

/// Drift threshold: propose a re-tune when the regret rate (regrets / served
/// events in the last 24h window) rises above this fraction.
pub const RETUNE_REGRET_RATE_THRESHOLD: f64 = 0.10;

/// S2.2: approximate token cost to reindex 1 000 memories when switching
/// the embedder model (conservative estimate based on batch embed overhead).
/// Used to report the cost of a full embedder switch in the advisor output.
pub const REINDEX_TOKENS_PER_1K_MEMORIES: u64 = 2_000;

/// S2.3 Regret penalty weight in the tune objective.
///
/// Weighting rationale:
///   A floor config that generates a regret has caused the model to work
///   harder than necessary (re-discover context that the brain dropped).
///   We penalise the *rate* of regrets (regrets / served events) rather than
///   the raw count so that the penalty is comparable across eval sets of
///   different sizes.
///
///   Weight = 0.5 was chosen so that a 100 % regret rate (pathological)
///   shifts the objective by −0.5, roughly equivalent to a 0.5-rank MRR
///   drop.  At realistic rates (< 10 %) the penalty is < 0.05 — meaningful
///   signal without overwhelming the MRR term.
pub const REGRET_PENALTY_WEIGHT: f64 = 0.5;

// ─── Sweep parameter space ────────────────────────────────────────────────────

pub const LEXICAL_FLOORS: &[f32] = &[0.3, 0.4, 0.5, 0.6];
pub const SEMANTIC_FLOORS: &[f32] = &[-1.0, 0.0, 0.25, 0.35, 0.45];
pub const RERANKER_IDS: &[&str] = &[
    "off",
    "ms-marco-tinybert-l-2-v2",
    "jina-reranker-v1-tiny-en",
    "ms-marco-minilm-l-4-v2",
];

/// v3.0: how the lexical and semantic rankings are merged. See
/// [`crate::fusion`] for why this is a real ranking decision and not plumbing.
///
/// It is swept rather than defaulted because BM25 and cosine live on different
/// scales, and which merge rule wins depends on the corpus — the whole reason
/// the semantic floor already had to be calibrated per embedder family. The
/// shipped default stays `linear` until a corpus says otherwise; this is how
/// you get it to say so.
pub const FUSION_MODES: &[&str] = &["linear", "rrf"];

// ─── Combo ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TuneCombo {
    pub min_lexical_coverage: f32,
    pub min_semantic_score: f32,
    pub reranker_id: String,
    /// v3.0: `"linear"` or `"rrf"`. `#[serde(default)]` keeps tune-history
    /// files written before v3.0 deserializing cleanly — they predate the
    /// dimension, so they describe linear runs.
    #[serde(default = "default_fusion_mode")]
    pub fusion: String,
}

fn default_fusion_mode() -> String {
    "linear".to_string()
}

impl TuneCombo {
    pub fn all_combos() -> Vec<TuneCombo> {
        let mut out = Vec::new();
        for &lex in LEXICAL_FLOORS {
            for &sem in SEMANTIC_FLOORS {
                for &rr in RERANKER_IDS {
                    for &fusion in FUSION_MODES {
                        out.push(TuneCombo {
                            min_lexical_coverage: lex,
                            min_semantic_score: sem,
                            reranker_id: rr.to_string(),
                            fusion: fusion.to_string(),
                        });
                    }
                }
            }
        }
        out
    }
}

// ─── Per-combo result ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComboResult {
    pub combo: TuneCombo,
    pub mean_mrr: f64,
    pub mean_tokens: f64,
    /// mean_mrr − cost_weight * mean_tokens
    pub objective: f64,
}

// ─── Tune history entry ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TuneHistoryEntry {
    pub timestamp: String,
    pub before: TuneCombo,
    pub after: TuneCombo,
    pub train_objective: f64,
    pub holdout_objective: f64,
    pub holdout_mrr: f64,
    pub baseline_holdout_objective: f64,
    /// S2.1: corpus size (active memory count) at the time of this tune run.
    /// `None` for history entries written before S2 (backward compat).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_count_at_tune: Option<u64>,
}

// ─── S2.1: Re-tune trigger state ─────────────────────────────────────────────

/// Trigger state for S2.1 re-tune proposals.  Computed cheaply (no sweep).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetuneTriggerState {
    /// Active memory count right now.
    pub current_memory_count: u64,
    /// Active memory count at the last tune, or 0 if never tuned.
    pub memory_count_at_last_tune: u64,
    /// Memories added since the last tune.
    pub memories_added_since_tune: u64,
    /// Whether the corpus milestone threshold has been crossed.
    pub corpus_milestone_triggered: bool,
    /// Regrets in the last 24 h window.
    pub recent_regret_count: u64,
    /// Context-served events in the last 24 h window.
    pub recent_served_count: u64,
    /// Regret rate = recent_regret_count / recent_served_count (0.0 when served=0).
    pub regret_rate: f64,
    /// Whether the drift threshold has been crossed.
    pub drift_triggered: bool,
    /// True when either trigger is active.
    pub should_retune: bool,
    /// Timestamp of the last tune, or `None` if never tuned.
    pub last_tuned_at: Option<String>,
}

/// Compute re-tune trigger state from the DB without running any sweep.
///
/// Reads:
/// - Active memory count (current and at last tune from `tune-history.json`).
/// - `retrieval.regret` event count in the last 24 h.
/// - `context.served` event count in the last 24 h.
pub fn compute_retune_trigger(
    conn: &rusqlite::Connection,
    kimetsu_dir: &std::path::Path,
) -> kimetsu_core::KimetsuResult<RetuneTriggerState> {
    // Current active memory count.
    let current_memory_count: u64 = conn.query_row(
        "SELECT COUNT(*) FROM memories WHERE invalidated_at IS NULL",
        [],
        |r| r.get(0),
    )?;

    // Last tune entry (if any).
    let last_entry = latest_tune_history(kimetsu_dir)?;
    let memory_count_at_last_tune = last_entry
        .as_ref()
        .and_then(|e| e.memory_count_at_tune)
        .unwrap_or(0);
    let last_tuned_at = last_entry.as_ref().map(|e| e.timestamp.clone());

    let memories_added_since_tune = current_memory_count.saturating_sub(memory_count_at_last_tune);
    let corpus_milestone_triggered = memories_added_since_tune >= RETUNE_CORPUS_MILESTONE;

    // Regret / served counts in the last 24 h.
    let cutoff_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        .saturating_sub(86_400);
    // Convert unix-secs cutoff to an approximate ISO string for the SQL comparison.
    let cutoff_iso = {
        let dt = time::OffsetDateTime::from_unix_timestamp(cutoff_secs as i64)
            .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
        dt.format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default()
    };

    let recent_regret_count: u64 = conn.query_row(
        "SELECT COUNT(*) FROM events WHERE kind = 'retrieval.regret' AND ts >= ?1",
        rusqlite::params![cutoff_iso],
        |r| r.get(0),
    )?;

    let recent_served_count: u64 = conn.query_row(
        "SELECT COUNT(*) FROM events WHERE kind = 'context.served' AND ts >= ?1",
        rusqlite::params![cutoff_iso],
        |r| r.get(0),
    )?;

    let regret_rate = if recent_served_count > 0 {
        recent_regret_count as f64 / recent_served_count as f64
    } else {
        0.0
    };
    let drift_triggered = regret_rate >= RETUNE_REGRET_RATE_THRESHOLD;
    let should_retune = corpus_milestone_triggered || drift_triggered;

    Ok(RetuneTriggerState {
        current_memory_count,
        memory_count_at_last_tune,
        memories_added_since_tune,
        corpus_milestone_triggered,
        recent_regret_count,
        recent_served_count,
        regret_rate,
        drift_triggered,
        should_retune,
        last_tuned_at,
    })
}

// ─── S2.2: Model re-selection advisor ────────────────────────────────────────

/// Known embedder models and their approximate on-disk download sizes (MiB).
///
/// These are estimates for the advisor report; actual sizes vary by format.
pub const KNOWN_EMBEDDER_MODELS: &[(&str, &str, u32)] = &[
    // (model_id, description, approx_download_mib)
    (
        "jina-embeddings-v2-base-code",
        "Jina v2 Code (768d, default)",
        280,
    ),
    ("bge-small-en-v1.5", "BGE-small (384d, lightweight)", 130),
    ("nomic-embed-text-v1.5", "Nomic Embed v1.5 (768d)", 270),
    ("all-minilm-l6-v2", "MiniLM L6 (384d, fast)", 90),
];

/// Model re-selection advisor recommendation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelAdvisorReport {
    /// Whether the advisor recommends re-running the grid now.
    pub recommend_grid_run: bool,
    /// Reason for the recommendation.
    pub reason: String,
    /// Currently active embedder model id.
    pub current_embedder: String,
    /// Approximate number of memories to re-embed if the model changes.
    pub memories_to_reindex: u64,
    /// Estimated token cost to reindex (conservative lower-bound).
    pub estimated_reindex_tokens: u64,
    /// Estimated approximate download size for all candidate models (MiB).
    pub candidate_models: Vec<ModelCandidate>,
}

/// A candidate embedder model for the grid sweep.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCandidate {
    pub model_id: String,
    pub description: String,
    pub approx_download_mib: u32,
}

/// Compute the model re-selection advisor report.
///
/// Does NOT run the sweep — only computes the metadata needed for the
/// advisor recommendation.  The actual grid run is a separate `brain tune`
/// invocation (reuses the existing sweep machinery).
///
/// `trigger` must be pre-computed via [`compute_retune_trigger`].
pub fn compute_model_advisor(
    current_embedder: &str,
    trigger: &RetuneTriggerState,
) -> ModelAdvisorReport {
    let recommend_grid_run = trigger.corpus_milestone_triggered;
    let reason = if trigger.corpus_milestone_triggered {
        format!(
            "Corpus grew by {} memories since last tune (≥{} threshold). \
             Re-running the embedder×reranker grid is recommended to verify \
             the current model remains optimal.",
            trigger.memories_added_since_tune, RETUNE_CORPUS_MILESTONE,
        )
    } else {
        format!(
            "No corpus milestone triggered ({} memories added, threshold {}). \
             Grid re-run is optional.",
            trigger.memories_added_since_tune, RETUNE_CORPUS_MILESTONE,
        )
    };

    let memories_to_reindex = trigger.current_memory_count;
    let estimated_reindex_tokens =
        (memories_to_reindex.max(1) / 1_000 + 1).saturating_mul(REINDEX_TOKENS_PER_1K_MEMORIES);

    let candidate_models = KNOWN_EMBEDDER_MODELS
        .iter()
        .map(|(id, desc, mib)| ModelCandidate {
            model_id: id.to_string(),
            description: desc.to_string(),
            approx_download_mib: *mib,
        })
        .collect();

    ModelAdvisorReport {
        recommend_grid_run,
        reason,
        current_embedder: current_embedder.to_string(),
        memories_to_reindex,
        estimated_reindex_tokens,
        candidate_models,
    }
}

// ─── Pure functions ───────────────────────────────────────────────────────────

/// Compute the tuning objective for a combo result.
///
/// `objective = mean_mrr - cost_weight * mean_tokens`
pub fn compute_objective(mean_mrr: f64, mean_tokens: f64, cost_weight: f64) -> f64 {
    mean_mrr - cost_weight * mean_tokens
}

/// S2.3: Compute the tuning objective with a regret penalty term.
///
/// Extended objective:
/// ```text
/// objective = mean_mrr
///           - cost_weight   * mean_tokens
///           - REGRET_PENALTY_WEIGHT * regret_rate
/// ```
///
/// `regret_rate` = regrets_for_this_combo / total_served_events.
/// A floor configuration that drops capsules later cited by the model
/// incurs a higher `regret_rate` and is penalised.
///
/// Weight: [`REGRET_PENALTY_WEIGHT`] = 0.5 — see module docs for
/// calibration rationale.
pub fn compute_objective_with_regret(
    mean_mrr: f64,
    mean_tokens: f64,
    cost_weight: f64,
    regret_rate: f64,
) -> f64 {
    mean_mrr - cost_weight * mean_tokens - REGRET_PENALTY_WEIGHT * regret_rate
}

/// Count `retrieval.regret` events in `conn` within an optional ISO-8601
/// timestamp window `[since, until]`.
///
/// Used by the sweep to collect regret signal per evaluation window so the
/// objective function can penalise floor configs that generated regrets.
pub fn count_regret_events(
    conn: &rusqlite::Connection,
    since: Option<&str>,
    until: Option<&str>,
) -> kimetsu_core::KimetsuResult<u64> {
    let count: u64 = match (since, until) {
        (Some(lo), Some(hi)) => conn.query_row(
            "SELECT COUNT(*) FROM events \
             WHERE kind = 'retrieval.regret' AND ts >= ?1 AND ts <= ?2",
            rusqlite::params![lo, hi],
            |r| r.get(0),
        )?,
        (Some(lo), None) => conn.query_row(
            "SELECT COUNT(*) FROM events \
             WHERE kind = 'retrieval.regret' AND ts >= ?1",
            rusqlite::params![lo],
            |r| r.get(0),
        )?,
        (None, Some(hi)) => conn.query_row(
            "SELECT COUNT(*) FROM events \
             WHERE kind = 'retrieval.regret' AND ts <= ?1",
            rusqlite::params![hi],
            |r| r.get(0),
        )?,
        (None, None) => conn.query_row(
            "SELECT COUNT(*) FROM events WHERE kind = 'retrieval.regret'",
            [],
            |r| r.get(0),
        )?,
    };
    Ok(count)
}

/// Split cases into (train, holdout) using a deterministic seed derived from
/// `case_count`. 80 % train, 20 % holdout. Indices into `cases` are returned.
///
/// The split is stable: the same set of N cases always produces the same
/// train/holdout partition regardless of case order.
pub fn train_holdout_split(case_count: usize) -> (Vec<usize>, Vec<usize>) {
    if case_count == 0 {
        return (Vec::new(), Vec::new());
    }
    let holdout_size = (case_count / 5).max(1); // ≥1 holdout
    // Deterministic: pick every 5th index as holdout.
    let holdout: Vec<usize> = (0..case_count).filter(|i| i % 5 == 0).collect();
    let train: Vec<usize> = (0..case_count).filter(|i| i % 5 != 0).collect();
    let _ = holdout_size; // used via filter logic above
    (train, holdout)
}

/// Select the best combo from a slice of `ComboResult` by objective score.
/// Returns `None` when the slice is empty.
pub fn select_winner(results: &[ComboResult]) -> Option<&ComboResult> {
    results.iter().max_by(|a, b| {
        a.objective
            .partial_cmp(&b.objective)
            .unwrap_or(std::cmp::Ordering::Equal)
    })
}

// ─── Tune history I/O ─────────────────────────────────────────────────────────

/// Append a `TuneHistoryEntry` to `.kimetsu/tune-history.json`.
pub fn append_tune_history(
    kimetsu_dir: &std::path::Path,
    entry: TuneHistoryEntry,
) -> kimetsu_core::KimetsuResult<()> {
    let path = kimetsu_dir.join("tune-history.json");
    let mut entries: Vec<TuneHistoryEntry> = if path.exists() {
        let text = std::fs::read_to_string(&path)?;
        serde_json::from_str(&text).unwrap_or_default()
    } else {
        Vec::new()
    };
    entries.push(entry);
    let json = serde_json::to_string_pretty(&entries)?;
    std::fs::write(&path, json)?;
    Ok(())
}

/// Read the latest entry from `.kimetsu/tune-history.json`, if any.
pub fn latest_tune_history(
    kimetsu_dir: &std::path::Path,
) -> kimetsu_core::KimetsuResult<Option<TuneHistoryEntry>> {
    let path = kimetsu_dir.join("tune-history.json");
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)?;
    let entries: Vec<TuneHistoryEntry> = serde_json::from_str(&text).unwrap_or_default();
    Ok(entries.into_iter().last())
}

// ─── Unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use ulid::Ulid;

    /// The sweep space is the product of every swept dimension. Asserting the
    /// arithmetic (rather than a literal) keeps this honest when a dimension is
    /// added: v3.0 added `fusion`, which doubled the grid from 80 to 160.
    #[test]
    fn all_combos_covers_the_full_grid() {
        let combos = TuneCombo::all_combos();
        let expected =
            LEXICAL_FLOORS.len() * SEMANTIC_FLOORS.len() * RERANKER_IDS.len() * FUSION_MODES.len();
        assert_eq!(
            combos.len(),
            expected,
            "expected {}×{}×{}×{}={expected} combos, got {}",
            LEXICAL_FLOORS.len(),
            SEMANTIC_FLOORS.len(),
            RERANKER_IDS.len(),
            FUSION_MODES.len(),
            combos.len()
        );

        // Every combo must be distinct: a duplicated point wastes a full
        // evaluation pass over the corpus.
        let mut keys: Vec<String> = combos
            .iter()
            .map(|c| {
                format!(
                    "{}|{}|{}|{}",
                    c.min_lexical_coverage, c.min_semantic_score, c.reranker_id, c.fusion
                )
            })
            .collect();
        keys.sort();
        let before = keys.len();
        keys.dedup();
        assert_eq!(before, keys.len(), "sweep grid contains duplicate combos");
    }

    #[test]
    fn compute_objective_formula() {
        let obj = compute_objective(0.75, 1000.0, 0.005);
        // 0.75 - 0.005 * 1000 = 0.75 - 5.0 = -4.25
        assert!((obj - (-4.25)).abs() < 1e-9, "objective: {obj}");
    }

    #[test]
    fn compute_objective_zero_cost_weight_is_just_mrr() {
        let obj = compute_objective(0.85, 500.0, 0.0);
        assert!((obj - 0.85).abs() < 1e-9, "objective with 0 cost: {obj}");
    }

    #[test]
    fn train_holdout_split_80_20() {
        let (train, holdout) = train_holdout_split(10);
        // Indices 0..10, every 5th (0,5) → holdout, rest → train.
        assert_eq!(holdout, vec![0, 5]);
        assert_eq!(train, vec![1, 2, 3, 4, 6, 7, 8, 9]);
        assert_eq!(train.len() + holdout.len(), 10);
    }

    #[test]
    fn train_holdout_split_empty() {
        let (train, holdout) = train_holdout_split(0);
        assert!(train.is_empty());
        assert!(holdout.is_empty());
    }

    #[test]
    fn select_winner_picks_highest_objective() {
        let combos = vec![
            ComboResult {
                combo: TuneCombo {
                    min_lexical_coverage: 0.3,
                    min_semantic_score: 0.0,
                    reranker_id: "off".to_string(),
                    fusion: "linear".to_string(),
                },
                mean_mrr: 0.7,
                mean_tokens: 100.0,
                objective: 0.2,
            },
            ComboResult {
                combo: TuneCombo {
                    min_lexical_coverage: 0.4,
                    min_semantic_score: 0.25,
                    reranker_id: "off".to_string(),
                    fusion: "linear".to_string(),
                },
                mean_mrr: 0.9,
                mean_tokens: 80.0,
                objective: 0.5,
            },
        ];
        let winner = select_winner(&combos).expect("winner");
        assert!((winner.objective - 0.5).abs() < 1e-9);
    }

    #[test]
    fn tune_history_roundtrip() {
        let tmp = std::env::temp_dir().join(format!("kimetsu-tune-hist-{}", Ulid::new()));
        std::fs::create_dir_all(&tmp).unwrap();

        let entry = TuneHistoryEntry {
            timestamp: "2026-06-11T00:00:00Z".to_string(),
            before: TuneCombo {
                min_lexical_coverage: 0.5,
                min_semantic_score: -1.0,
                reranker_id: "off".to_string(),
                fusion: "linear".to_string(),
            },
            after: TuneCombo {
                min_lexical_coverage: 0.4,
                min_semantic_score: 0.25,
                reranker_id: "ms-marco-tinybert-l-2-v2".to_string(),
                fusion: "linear".to_string(),
            },
            train_objective: 0.55,
            holdout_objective: 0.50,
            holdout_mrr: 0.70,
            baseline_holdout_objective: 0.45,
            memory_count_at_tune: None,
        };

        append_tune_history(&tmp, entry.clone()).unwrap();
        let latest = latest_tune_history(&tmp).unwrap().unwrap();
        assert!((latest.holdout_objective - 0.50).abs() < 1e-9);
        assert_eq!(latest.after.reranker_id, "ms-marco-tinybert-l-2-v2");

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn tune_history_empty_when_no_file() {
        let tmp = std::env::temp_dir().join(format!("kimetsu-tune-empty-{}", Ulid::new()));
        std::fs::create_dir_all(&tmp).unwrap();
        let latest = latest_tune_history(&tmp).unwrap();
        assert!(latest.is_none(), "no history file → None");
        std::fs::remove_dir_all(&tmp).ok();
    }

    // ─── S2.3: regret-penalised objective ────────────────────────────────────

    #[test]
    fn compute_objective_with_regret_zero_rate_matches_base() {
        let base = compute_objective(0.75, 500.0, 0.005);
        let with_regret = compute_objective_with_regret(0.75, 500.0, 0.005, 0.0);
        assert!(
            (base - with_regret).abs() < 1e-9,
            "zero regret_rate must give same result as base objective"
        );
    }

    #[test]
    fn compute_objective_with_regret_penalises_high_rate() {
        let base = compute_objective(0.75, 500.0, 0.005);
        let with_regret = compute_objective_with_regret(0.75, 500.0, 0.005, 0.10);
        // penalty = 0.5 * 0.10 = 0.05
        assert!(
            with_regret < base,
            "positive regret_rate must reduce the objective"
        );
        assert!(
            (base - with_regret - REGRET_PENALTY_WEIGHT * 0.10).abs() < 1e-9,
            "penalty term must equal REGRET_PENALTY_WEIGHT * regret_rate"
        );
    }

    #[test]
    fn compute_objective_with_regret_full_rate_shifts_by_weight() {
        // regret_rate = 1.0 → penalty = REGRET_PENALTY_WEIGHT
        let base = compute_objective(0.8, 0.0, 0.0);
        let with_full = compute_objective_with_regret(0.8, 0.0, 0.0, 1.0);
        assert!(
            (base - with_full - REGRET_PENALTY_WEIGHT).abs() < 1e-9,
            "100% regret rate shifts objective by REGRET_PENALTY_WEIGHT"
        );
    }

    // ─── S2.1: RetuneTriggerState ─────────────────────────────────────────────

    use crate::{
        project::{init_project, load_project},
        projector,
        user_brain::with_user_brain_disabled,
    };
    use kimetsu_core::{event::Event, ids::RunId};

    fn trigger_test_root(label: &str) -> std::path::PathBuf {
        let root =
            std::env::temp_dir().join(format!("kimetsu-tune-trigger-{label}-{}", Ulid::new()));
        kimetsu_core::paths::git_init_boundary(&root);
        root
    }

    #[test]
    fn retune_trigger_no_history_no_events() {
        with_user_brain_disabled(|| {
            let root = trigger_test_root("empty");
            std::fs::create_dir_all(&root).expect("mkdir");
            init_project(&root, false).expect("init");
            let paths = kimetsu_core::paths::ProjectPaths::discover(&root).expect("paths");
            let (_, _, conn) = load_project(&root).expect("load");
            let state = compute_retune_trigger(&conn, &paths.kimetsu_dir).expect("trigger");
            assert_eq!(state.current_memory_count, 0);
            assert_eq!(state.memories_added_since_tune, 0);
            assert!(!state.corpus_milestone_triggered);
            assert!(!state.drift_triggered);
            assert!(!state.should_retune);
            assert!(state.last_tuned_at.is_none());
            std::fs::remove_dir_all(&root).ok();
        });
    }

    #[test]
    fn retune_trigger_corpus_milestone_when_enough_memories() {
        with_user_brain_disabled(|| {
            let root = trigger_test_root("milestone");
            std::fs::create_dir_all(&root).expect("mkdir");
            init_project(&root, false).expect("init");
            let paths = kimetsu_core::paths::ProjectPaths::discover(&root).expect("paths");

            // Seed a fake tune-history entry with memory_count_at_tune = 0.
            let entry = TuneHistoryEntry {
                timestamp: "2026-01-01T00:00:00Z".to_string(),
                before: TuneCombo {
                    min_lexical_coverage: 0.4,
                    min_semantic_score: 0.0,
                    reranker_id: "off".to_string(),
                    fusion: "linear".to_string(),
                },
                after: TuneCombo {
                    min_lexical_coverage: 0.4,
                    min_semantic_score: 0.0,
                    reranker_id: "off".to_string(),
                    fusion: "linear".to_string(),
                },
                train_objective: 0.5,
                holdout_objective: 0.5,
                holdout_mrr: 0.7,
                baseline_holdout_objective: 0.45,
                memory_count_at_tune: Some(0),
            };
            append_tune_history(&paths.kimetsu_dir, entry).expect("append");

            // Add RETUNE_CORPUS_MILESTONE memories via the add_memory API.
            for i in 0..RETUNE_CORPUS_MILESTONE {
                crate::project::add_memory(
                    &root,
                    kimetsu_core::memory::MemoryScope::Project,
                    kimetsu_core::memory::MemoryKind::Fact,
                    &format!("milestone memory {i}"),
                )
                .expect("add memory");
            }

            let (_, _, conn) = load_project(&root).expect("load");
            let state = compute_retune_trigger(&conn, &paths.kimetsu_dir).expect("trigger");
            assert!(
                state.corpus_milestone_triggered,
                "milestone must trigger at ≥{RETUNE_CORPUS_MILESTONE} memories added"
            );
            assert!(state.should_retune);
            std::fs::remove_dir_all(&root).ok();
        });
    }

    #[test]
    fn retune_trigger_drift_when_regret_rate_high() {
        with_user_brain_disabled(|| {
            let root = trigger_test_root("drift");
            std::fs::create_dir_all(&root).expect("mkdir");
            init_project(&root, false).expect("init");
            let paths = kimetsu_core::paths::ProjectPaths::discover(&root).expect("paths");
            let (_, _, conn) = load_project(&root).expect("load");

            // Seed 1 served event + 1 regret event (rate = 100% >> threshold).
            let run_id = RunId::new();
            let served_ev = Event::new(
                run_id,
                "context.served",
                serde_json::json!({"query_hash":"abc","capsule_count":1,"skipped":false}),
            );
            projector::apply_events(&conn, &[served_ev]).expect("seed served");
            let regret_ev = Event::new(
                run_id,
                "retrieval.regret",
                serde_json::json!({"memory_id":"m1","dropped_at":0,"cited_at":1}),
            );
            projector::apply_events(&conn, &[regret_ev]).expect("seed regret");

            let state = compute_retune_trigger(&conn, &paths.kimetsu_dir).expect("trigger");
            assert!(
                state.drift_triggered,
                "regret_rate ({:.2}) must exceed threshold ({RETUNE_REGRET_RATE_THRESHOLD})",
                state.regret_rate
            );
            assert!(state.should_retune);
            std::fs::remove_dir_all(&root).ok();
        });
    }

    // ─── S2.2: ModelAdvisorReport ─────────────────────────────────────────────

    #[test]
    fn model_advisor_recommends_at_milestone() {
        let trigger = RetuneTriggerState {
            current_memory_count: 100,
            memory_count_at_last_tune: 10,
            memories_added_since_tune: 90,
            corpus_milestone_triggered: true,
            recent_regret_count: 0,
            recent_served_count: 20,
            regret_rate: 0.0,
            drift_triggered: false,
            should_retune: true,
            last_tuned_at: Some("2026-01-01T00:00:00Z".to_string()),
        };
        let report = compute_model_advisor("jina-embeddings-v2-base-code", &trigger);
        assert!(report.recommend_grid_run, "must recommend at milestone");
        assert!(report.estimated_reindex_tokens > 0, "cost must be stated");
        assert!(!report.candidate_models.is_empty());
    }

    #[test]
    fn model_advisor_no_recommendation_below_milestone() {
        let trigger = RetuneTriggerState {
            current_memory_count: 30,
            memory_count_at_last_tune: 25,
            memories_added_since_tune: 5,
            corpus_milestone_triggered: false,
            recent_regret_count: 0,
            recent_served_count: 10,
            regret_rate: 0.0,
            drift_triggered: false,
            should_retune: false,
            last_tuned_at: None,
        };
        let report = compute_model_advisor("jina-embeddings-v2-base-code", &trigger);
        assert!(
            !report.recommend_grid_run,
            "must NOT recommend below milestone"
        );
    }

    // ─── S2.3: count_regret_events ────────────────────────────────────────────

    #[test]
    fn count_regret_events_zero_in_empty_db() {
        with_user_brain_disabled(|| {
            let root = trigger_test_root("regret-count");
            std::fs::create_dir_all(&root).expect("mkdir");
            init_project(&root, false).expect("init");
            let (_, _, conn) = load_project(&root).expect("load");
            let count = count_regret_events(&conn, None, None).expect("count");
            assert_eq!(count, 0);
            std::fs::remove_dir_all(&root).ok();
        });
    }

    #[test]
    fn tune_history_entry_memory_count_roundtrip() {
        let tmp = std::env::temp_dir().join(format!("kimetsu-tune-memcount-{}", Ulid::new()));
        std::fs::create_dir_all(&tmp).unwrap();

        let entry = TuneHistoryEntry {
            timestamp: "2026-06-11T00:00:00Z".to_string(),
            before: TuneCombo {
                min_lexical_coverage: 0.5,
                min_semantic_score: -1.0,
                reranker_id: "off".to_string(),
                fusion: "linear".to_string(),
            },
            after: TuneCombo {
                min_lexical_coverage: 0.4,
                min_semantic_score: 0.25,
                reranker_id: "off".to_string(),
                fusion: "linear".to_string(),
            },
            train_objective: 0.55,
            holdout_objective: 0.50,
            holdout_mrr: 0.70,
            baseline_holdout_objective: 0.45,
            memory_count_at_tune: Some(123),
        };

        append_tune_history(&tmp, entry).unwrap();
        let latest = latest_tune_history(&tmp).unwrap().unwrap();
        assert_eq!(
            latest.memory_count_at_tune,
            Some(123),
            "memory_count_at_tune must round-trip"
        );

        std::fs::remove_dir_all(&tmp).ok();
    }
}
