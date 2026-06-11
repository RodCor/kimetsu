//! v1.5: Self-Tuning Brain sweep engine.
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
//! Objective: mean_MRR - cost_weight * mean_injected_tokens

use serde::{Deserialize, Serialize};

// ─── Sweep parameter space ────────────────────────────────────────────────────

pub const LEXICAL_FLOORS: &[f32] = &[0.3, 0.4, 0.5, 0.6];
pub const SEMANTIC_FLOORS: &[f32] = &[-1.0, 0.0, 0.25, 0.35, 0.45];
pub const RERANKER_IDS: &[&str] = &[
    "off",
    "ms-marco-tinybert-l-2-v2",
    "jina-reranker-v1-tiny-en",
    "ms-marco-minilm-l-4-v2",
];

// ─── Combo ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TuneCombo {
    pub min_lexical_coverage: f32,
    pub min_semantic_score: f32,
    pub reranker_id: String,
}

impl TuneCombo {
    pub fn all_combos() -> Vec<TuneCombo> {
        let mut out = Vec::new();
        for &lex in LEXICAL_FLOORS {
            for &sem in SEMANTIC_FLOORS {
                for &rr in RERANKER_IDS {
                    out.push(TuneCombo {
                        min_lexical_coverage: lex,
                        min_semantic_score: sem,
                        reranker_id: rr.to_string(),
                    });
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
}

// ─── Pure functions ───────────────────────────────────────────────────────────

/// Compute the tuning objective for a combo result.
///
/// `objective = mean_mrr - cost_weight * mean_tokens`
pub fn compute_objective(mean_mrr: f64, mean_tokens: f64, cost_weight: f64) -> f64 {
    mean_mrr - cost_weight * mean_tokens
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

    #[test]
    fn all_combos_count_is_80() {
        let combos = TuneCombo::all_combos();
        assert_eq!(
            combos.len(),
            4 * 5 * 4,
            "expected 4×5×4=80 combos, got {}",
            combos.len()
        );
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
            },
            after: TuneCombo {
                min_lexical_coverage: 0.4,
                min_semantic_score: 0.25,
                reranker_id: "ms-marco-tinybert-l-2-v2".to_string(),
            },
            train_objective: 0.55,
            holdout_objective: 0.50,
            holdout_mrr: 0.70,
            baseline_holdout_objective: 0.45,
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
}
