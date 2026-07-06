//! Retrieval quality metrics for `kimetsu brain eval`.
//!
//! Pure, no-I/O module. All functions operate on slices of `String`
//! (memory keys / ranked result keys) so they are trivially unit-testable.

use serde::{Deserialize, Serialize};

// ─── Fixture types ────────────────────────────────────────────────────────────

/// A single corpus memory: stable key (referenced by [`EvalCase::relevant`]) and text.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalMemory {
    /// Stable short key used to cross-reference from [`EvalCase::relevant`].
    pub key: String,
    /// Full text of the memory to add to the corpus.
    pub text: String,
    /// Flagship 1 Pass A: optional RFC 3339 timestamp. When present and in the
    /// PAST, the bench seeder stamps this memory with `valid_to` (expired) so
    /// validity-aware retrieval excludes it. Omitting this field leaves the
    /// memory valid indefinitely — existing fixtures are unchanged.
    #[serde(default)]
    pub valid_to: Option<String>,
    /// Flagship 1 Pass A: optional key of another `EvalMemory` that supersedes
    /// this one. When present, the bench seeder stamps `superseded_by` on this
    /// memory (pointing to the survivor's DB id) so retrieval excludes it via
    /// the existing `superseded_by IS NULL` guard.
    /// Omitting this field leaves the memory active — existing fixtures unchanged.
    #[serde(default)]
    pub superseded_by_key: Option<String>,
}

/// Classification of an eval case for correctness measurement.
///
/// `Recall` is the default (and the only kind used by existing fixtures).
/// The new kinds are used by `bench/dataset-correctness.json` to measure
/// temporal correctness, contradiction resolution, and knowledge-update quality.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CaseKind {
    /// Plain retrieval: the relevant memory should appear in the top-k.
    #[default]
    Recall,
    /// A newer memory supersedes an older one; the query asks for current state.
    /// The current/correct memory should win; the stale one should NOT appear.
    KnowledgeUpdate,
    /// Two memories make contradictory claims; the authoritative one should win.
    Contradiction,
    /// A fact is qualified by an as-of date; the most recent should win.
    Temporal,
    /// Multiple sessions produced overlapping memories; the canonical one wins.
    MultiSession,
}

/// One eval case: a query plus the set of corpus keys that are relevant to it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalCase {
    pub query: String,
    /// Keys from [`EvalMemory::key`] that are relevant to this query.
    /// Empty = off-domain query (exercises noise floor, recall trivially 1.0).
    pub relevant: Vec<String>,
    /// Classification of this case. Defaults to [`CaseKind::Recall`].
    /// Existing fixtures omit this field; `#[serde(default)]` keeps them valid.
    #[serde(default)]
    pub kind: CaseKind,
    /// Keys of memories that should NOT appear in the top-k for this case
    /// (superseded / contradicted / losing memories).
    /// Empty by default — existing fixtures unchanged.
    #[serde(default)]
    pub stale: Vec<String>,
}

/// A committed eval fixture: a corpus of memories and a set of query cases.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalFixture {
    pub memories: Vec<EvalMemory>,
    pub cases: Vec<EvalCase>,
}

// ─── Metric math ─────────────────────────────────────────────────────────────

/// Fraction of `relevant` items found in the **first `k`** positions of `ranked`.
///
/// Each relevant key is counted at most once even if it appears multiple times
/// in `ranked`. Returns `1.0` when `relevant` is empty (trivial recall for
/// off-domain / noise queries). Returns `0.0` when `k == 0`.
pub fn recall_at_k(ranked: &[String], relevant: &[String], k: usize) -> f64 {
    if relevant.is_empty() {
        return 1.0;
    }
    if k == 0 || ranked.is_empty() {
        return 0.0;
    }
    let window = &ranked[..k.min(ranked.len())];
    let found = relevant
        .iter()
        .filter(|r| window.iter().any(|w| w == *r))
        .count();
    found as f64 / relevant.len() as f64
}

/// Mean Reciprocal Rank of the **first** relevant item in `ranked` (1-based).
///
/// Returns `1/rank` where `rank` is the 1-based position of the first relevant
/// item. Returns `0.0` when no relevant item appears in `ranked`.
pub fn mrr(ranked: &[String], relevant: &[String]) -> f64 {
    if relevant.is_empty() || ranked.is_empty() {
        return 0.0;
    }
    for (idx, key) in ranked.iter().enumerate() {
        if relevant.iter().any(|r| r == key) {
            return 1.0 / (idx as f64 + 1.0);
        }
    }
    0.0
}

/// Arithmetic mean of a slice of metric values.
///
/// Returns `0.0` for an empty slice.
pub fn mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.iter().sum::<f64>() / values.len() as f64
}

/// Per-case stale-hit rate: returns `1.0` if any `stale` key is present in the
/// first `k` positions of `ranked`, else `0.0`.
///
/// Lower is better. Averaged across cases → mean stale-hit rate.
/// Returns `0.0` when `stale` is empty (no stale keys defined for this case).
pub fn stale_hit_rate(ranked: &[String], stale: &[String], k: usize) -> f64 {
    if stale.is_empty() || k == 0 || ranked.is_empty() {
        return 0.0;
    }
    let window = &ranked[..k.min(ranked.len())];
    if stale.iter().any(|s| window.iter().any(|w| w == s)) {
        1.0
    } else {
        0.0
    }
}

/// Returns `true` when the case is "resolved correctly": every `relevant` key
/// that appears in `ranked` outranks every `stale` key that appears in `ranked`.
///
/// More precisely: the rank of the **best** (lowest-index) relevant key must be
/// strictly less than the rank of the **best** stale key. If no stale key
/// appears in `ranked` at all, the case is resolved (stale is absent — ideal).
/// If no relevant key appears, the case is unresolved.
///
/// Used for contradiction / knowledge-update cases. Averaged → resolution accuracy.
pub fn resolution_correct(ranked: &[String], relevant: &[String], stale: &[String]) -> bool {
    if relevant.is_empty() {
        return false;
    }
    // Position of the first relevant key in ranked (best = lowest index).
    let best_relevant = ranked
        .iter()
        .enumerate()
        .find(|(_, k)| relevant.iter().any(|r| r == *k))
        .map(|(i, _)| i);

    let best_relevant = match best_relevant {
        Some(pos) => pos,
        None => return false, // no relevant in ranked → unresolved
    };

    // Position of the first (best) stale key in ranked.
    let best_stale = ranked
        .iter()
        .enumerate()
        .find(|(_, k)| stale.iter().any(|s| s == *k))
        .map(|(i, _)| i);

    match best_stale {
        None => true, // stale absent from ranked → ideal, resolved
        Some(stale_pos) => best_relevant < stale_pos,
    }
}

// ─── Unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    // ── recall_at_k ──────────────────────────────────────────────────────────

    #[test]
    fn recall_at_k_empty_relevant_is_one() {
        // Off-domain queries: no relevant items → trivially 1.0.
        assert_eq!(recall_at_k(&s(&["a", "b"]), &[], 4), 1.0);
        assert_eq!(recall_at_k(&[], &[], 4), 1.0);
    }

    #[test]
    fn recall_at_k_zero_k_is_zero() {
        assert_eq!(recall_at_k(&s(&["a", "b"]), &s(&["a"]), 0), 0.0);
    }

    #[test]
    fn recall_at_k_k_larger_than_ranked_uses_full_list() {
        // k > len(ranked): should still count everything in ranked.
        let ranked = s(&["a", "b"]);
        let relevant = s(&["a", "b", "c"]);
        // 2 of 3 found in first 100 positions → 2/3.
        let r = recall_at_k(&ranked, &relevant, 100);
        assert!((r - 2.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn recall_at_k_exact_hits() {
        let ranked = s(&["a", "b", "c", "d"]);
        let relevant = s(&["b", "d"]);
        // k=2: only "b" in first 2 → 0.5
        assert!((recall_at_k(&ranked, &relevant, 2) - 0.5).abs() < 1e-9);
        // k=4: both found → 1.0
        assert_eq!(recall_at_k(&ranked, &relevant, 4), 1.0);
    }

    #[test]
    fn recall_at_k_duplicates_in_ranked_count_once() {
        // "a" appears twice in ranked, but should only count as 1 hit.
        let ranked = s(&["a", "a", "b"]);
        let relevant = s(&["a", "b"]);
        // Both are in first 3 positions → 2/2 = 1.0 (not 3/2).
        assert_eq!(recall_at_k(&ranked, &relevant, 3), 1.0);
        // k=1: "a" appears → 1 of 2 relevant found = 0.5
        assert!((recall_at_k(&ranked, &relevant, 1) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn recall_at_k_no_hits_is_zero() {
        let ranked = s(&["x", "y", "z"]);
        let relevant = s(&["a", "b"]);
        assert_eq!(recall_at_k(&ranked, &relevant, 5), 0.0);
    }

    // ── mrr ──────────────────────────────────────────────────────────────────

    #[test]
    fn mrr_first_position_is_one() {
        let ranked = s(&["a", "b", "c"]);
        let relevant = s(&["a"]);
        assert_eq!(mrr(&ranked, &relevant), 1.0);
    }

    #[test]
    fn mrr_second_position_is_half() {
        let ranked = s(&["x", "a", "b"]);
        let relevant = s(&["a"]);
        assert!((mrr(&ranked, &relevant) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn mrr_third_position_is_one_third() {
        let ranked = s(&["x", "y", "a"]);
        let relevant = s(&["a"]);
        assert!((mrr(&ranked, &relevant) - 1.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn mrr_absent_is_zero() {
        let ranked = s(&["x", "y", "z"]);
        let relevant = s(&["a"]);
        assert_eq!(mrr(&ranked, &relevant), 0.0);
    }

    #[test]
    fn mrr_empty_relevant_is_zero() {
        let ranked = s(&["a", "b"]);
        assert_eq!(mrr(&ranked, &[]), 0.0);
    }

    #[test]
    fn mrr_empty_ranked_is_zero() {
        assert_eq!(mrr(&[], &s(&["a"]),), 0.0);
    }

    #[test]
    fn mrr_uses_first_hit_when_multiple_relevant() {
        // "b" is at rank 2, "a" is at rank 3 — MRR should be 1/2.
        let ranked = s(&["x", "b", "a"]);
        let relevant = s(&["a", "b"]);
        assert!((mrr(&ranked, &relevant) - 0.5).abs() < 1e-9);
    }

    // ── mean ─────────────────────────────────────────────────────────────────

    #[test]
    fn mean_empty_is_zero() {
        assert_eq!(mean(&[]), 0.0);
    }

    #[test]
    fn mean_single() {
        assert!((mean(&[0.75]) - 0.75).abs() < 1e-9);
    }

    #[test]
    fn mean_normal() {
        let v = [0.0, 0.5, 1.0];
        assert!((mean(&v) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn mean_all_ones() {
        assert!((mean(&[1.0, 1.0, 1.0]) - 1.0).abs() < 1e-9);
    }

    // ── stale_hit_rate ────────────────────────────────────────────────────────

    #[test]
    fn stale_hit_rate_no_stale_is_zero() {
        // No stale keys defined → always 0.0 regardless of ranked.
        assert_eq!(stale_hit_rate(&s(&["a", "b", "c"]), &[], 4), 0.0);
        assert_eq!(stale_hit_rate(&[], &[], 4), 0.0);
    }

    #[test]
    fn stale_hit_rate_stale_in_top_k_is_one() {
        // "b" is stale and is at rank 2 (within k=4 window) → 1.0.
        let ranked = s(&["a", "b", "c", "d"]);
        let stale = s(&["b"]);
        assert_eq!(stale_hit_rate(&ranked, &stale, 4), 1.0);
    }

    #[test]
    fn stale_hit_rate_stale_beyond_k_is_zero() {
        // "d" is stale but is at rank 4; window k=2 → 0.0.
        let ranked = s(&["a", "b", "c", "d"]);
        let stale = s(&["d"]);
        assert_eq!(stale_hit_rate(&ranked, &stale, 2), 0.0);
    }

    #[test]
    fn stale_hit_rate_stale_absent_is_zero() {
        let ranked = s(&["a", "b", "c"]);
        let stale = s(&["z"]);
        assert_eq!(stale_hit_rate(&ranked, &stale, 4), 0.0);
    }

    // ── resolution_correct ────────────────────────────────────────────────────

    #[test]
    fn resolution_correct_relevant_above_stale_is_true() {
        // relevant "new" at rank 1, stale "old" at rank 3 → resolved.
        let ranked = s(&["new", "x", "old"]);
        assert!(resolution_correct(&ranked, &s(&["new"]), &s(&["old"])));
    }

    #[test]
    fn resolution_correct_stale_above_relevant_is_false() {
        // stale "old" at rank 1, relevant "new" at rank 3 → NOT resolved.
        let ranked = s(&["old", "x", "new"]);
        assert!(!resolution_correct(&ranked, &s(&["new"]), &s(&["old"])));
    }

    #[test]
    fn resolution_correct_stale_absent_is_true() {
        // relevant present, stale absent from ranked → ideal resolution.
        let ranked = s(&["new", "x", "y"]);
        assert!(resolution_correct(&ranked, &s(&["new"]), &s(&["old"])));
    }

    #[test]
    fn resolution_correct_relevant_absent_is_false() {
        // relevant missing from ranked entirely → cannot be resolved.
        let ranked = s(&["old", "x", "y"]);
        assert!(!resolution_correct(&ranked, &s(&["new"]), &s(&["old"])));
    }

    #[test]
    fn resolution_correct_empty_relevant_is_false() {
        let ranked = s(&["new", "old"]);
        assert!(!resolution_correct(&ranked, &[], &s(&["old"])));
    }
}
