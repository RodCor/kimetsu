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
}

/// One eval case: a query plus the set of corpus keys that are relevant to it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalCase {
    pub query: String,
    /// Keys from [`EvalMemory::key`] that are relevant to this query.
    /// Empty = off-domain query (exercises noise floor, recall trivially 1.0).
    pub relevant: Vec<String>,
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
}
