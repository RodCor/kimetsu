//! How the broker combines candidate lists from different retrieval strategies.
//!
//! Retrieval produces several ranked lists over the same corpus — lexical FTS5,
//! semantic ANN, and (on the graph backends) edge traversal. They have to be
//! merged into one ranking, and the merge rule matters more than it looks.
//!
//! ## The problem with combining scores
//!
//! Kimetsu's original rule was union-max: take every candidate from every list,
//! and where a memory appears twice keep whichever instance scored higher, with
//! each candidate's score already a linear blend
//! `(1-α)·lexical + α·cosine` at `α = 0.5`.
//!
//! That is a *score* combination, and BM25 scores and cosine similarities are
//! not on the same scale. BM25 is unbounded and corpus-dependent; cosine is
//! bounded in `[-1, 1]` and tends to cluster tightly near the top of the range.
//! Averaging them means the blend's behaviour drifts with corpus size and with
//! whichever embedder is loaded — the same reason the semantic floor had to be
//! calibrated per embedder family in the first place.
//!
//! ## Reciprocal rank fusion
//!
//! [`rrf_fuse`] discards the scores and uses only each candidate's *rank* in
//! each list:
//!
//! ```text
//! score(d) = Σ over lists  1 / (k + rank_list(d))     rank is 1-based
//! ```
//!
//! Rank is scale-free, so the score-incompatibility problem disappears
//! entirely. A memory that both lists rank highly beats one that only a single
//! list loves, which is the behaviour union-max was reaching for and did not
//! quite get: under union-max, a candidate ranked #1 lexically and #1
//! semantically scores exactly the same as one ranked #1 lexically and last
//! semantically.
//!
//! `k` (default [`DEFAULT_RRF_K`]) damps the head of each list: with `k = 60`
//! the gap between rank 1 and rank 2 is small, so a single list cannot dominate
//! on the strength of its top hit alone.
//!
//! ## Which one runs
//!
//! Both. `[broker] fusion` selects, and it defaults to `linear` — the existing
//! behaviour — because Kimetsu's house rule is that every claim ships with a
//! measurement, and swapping the ranking rule on the strength of "RRF is the
//! 2026 default" would be a claim without one. `kimetsu brain tune` sweeps the
//! two against your own query history; flip the key when your corpus says to.

// The fusion rules only have work to do when there is more than one candidate
// list, which today means the `embeddings` build (FTS + ANN). On the lean build
// retrieval has a single lexical ranking, so nothing calls them — but `Fusion`
// itself is still parsed and threaded through backend construction, and the
// tests below still run, so the module stays compiled rather than cfg'd out.
#![cfg_attr(not(feature = "embeddings"), allow(dead_code))]

use std::collections::HashMap;

use crate::context::Candidate;

/// Rank-damping constant. 60 is the value from the original RRF paper
/// (Cormack, Clarke & Buettcher 2009) and remains the common default: large
/// enough that no single list's top hit can dominate the fused ranking, small
/// enough that rank still matters.
pub const DEFAULT_RRF_K: f32 = 60.0;

/// Which fusion rule the broker uses to merge candidate lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fusion {
    /// Union the lists, keeping each memory's best `raw_relevance`, where that
    /// relevance is itself a linear blend of lexical and cosine signal.
    /// Kimetsu's behaviour through v2.5.
    Linear,
    /// Reciprocal rank fusion over the per-list ranks.
    Rrf,
}

impl Fusion {
    /// Parse a `[broker] fusion` value. Unknown values fall back to `Linear`,
    /// matching how the storage backend handles a typo: a bad config key
    /// degrades to the previous behaviour rather than failing retrieval.
    pub fn from_config(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "rrf" => Fusion::Rrf,
            _ => Fusion::Linear,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Fusion::Linear => "linear",
            Fusion::Rrf => "rrf",
        }
    }
}

/// Stable identity for a candidate across lists.
///
/// `Capsule::id` is a fresh ULID per retrieval, so it cannot be used to
/// recognise the same memory in two lists; `expansion_handle` is the stable
/// `"memory:<id>"` / `"repo_file:<path>"` key the rest of the broker already
/// dedupes on.
fn candidate_key(candidate: &Candidate) -> &str {
    &candidate.capsule.expansion_handle
}

/// Union `lists`, keeping the highest-scoring instance of each candidate.
///
/// This is the pre-v2.6 rule, extracted so both fusion modes go through one
/// code path and the difference between them is a single call.
pub(crate) fn union_max(lists: Vec<Vec<Candidate>>) -> Vec<Candidate> {
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut merged: Vec<Candidate> = Vec::new();
    for candidate in lists.into_iter().flatten() {
        let key = candidate_key(&candidate).to_string();
        match seen.get(&key) {
            Some(&idx) => {
                if candidate.raw_relevance > merged[idx].raw_relevance {
                    merged[idx] = candidate;
                }
            }
            None => {
                seen.insert(key, merged.len());
                merged.push(candidate);
            }
        }
    }
    merged
}

/// Fuse ranked `lists` by reciprocal rank.
///
/// Each list is treated as already ordered best-first — which is how every
/// candidate source in the broker returns its results. The surviving
/// candidate for a memory is the instance with the highest `raw_relevance`
/// (so downstream consumers that still read that field, such as the semantic
/// floor and the MMR pass, see the strongest evidence), but the returned
/// ordering is the fused one, and `raw_relevance` is rewritten to the
/// normalized fused score so the broker's per-kind scaling still has a
/// meaningful magnitude to work with.
///
/// Fused scores are normalized to `[0, 1]` by dividing by the best fused score,
/// so the value stays in the same range the rest of the pipeline expects from
/// a `raw_relevance`.
///
/// Returns candidates sorted by descending fused score. Ties break on the
/// candidate key, so the result is deterministic.
pub(crate) fn rrf_fuse(lists: Vec<Vec<Candidate>>, k: f32) -> Vec<Candidate> {
    let k = if k > 0.0 { k } else { DEFAULT_RRF_K };

    let mut fused: HashMap<String, f32> = HashMap::new();
    for list in &lists {
        for (rank, candidate) in list.iter().enumerate() {
            // 1-based rank: the top hit contributes 1/(k+1), not 1/k.
            let contribution = 1.0 / (k + (rank as f32 + 1.0));
            *fused
                .entry(candidate_key(candidate).to_string())
                .or_insert(0.0) += contribution;
        }
    }

    // Keep the strongest instance of each candidate, then re-score and re-sort.
    let mut best = union_max(lists);
    let max_fused = fused.values().copied().fold(0.0_f32, f32::max);
    for candidate in &mut best {
        let score = fused
            .get(candidate_key(candidate))
            .copied()
            .unwrap_or_default();
        candidate.raw_relevance = if max_fused > 0.0 {
            score / max_fused
        } else {
            0.0
        };
    }
    best.sort_by(|a, b| {
        b.raw_relevance
            .partial_cmp(&a.raw_relevance)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| candidate_key(a).cmp(candidate_key(b)))
    });
    best
}

/// Apply the configured fusion rule to `lists`.
pub(crate) fn fuse(mode: Fusion, lists: Vec<Vec<Candidate>>) -> Vec<Candidate> {
    match mode {
        Fusion::Linear => union_max(lists),
        Fusion::Rrf => rrf_fuse(lists, DEFAULT_RRF_K),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{Candidate, ContextCapsule};

    fn candidate(handle: &str, relevance: f32) -> Candidate {
        Candidate {
            capsule: ContextCapsule {
                id: String::new(),
                kind: "memory".to_string(),
                summary: format!("project:fact - {handle}"),
                token_estimate: 10,
                expansion_handle: handle.to_string(),
                provenance: Vec::new(),
                confidence: 0.9,
                freshness: 0.5,
                relevance: 0.0,
                scope_weight: 0.9,
                score: 0.0,
            },
            raw_relevance: relevance,
            embedding: None,
            cosine: None,
            created_at: None,
        }
    }

    fn handles(candidates: &[Candidate]) -> Vec<&str> {
        candidates
            .iter()
            .map(|c| c.capsule.expansion_handle.as_str())
            .collect()
    }

    #[test]
    fn fusion_parses_and_falls_back_to_linear() {
        assert_eq!(Fusion::from_config("rrf"), Fusion::Rrf);
        assert_eq!(Fusion::from_config("RRF"), Fusion::Rrf);
        assert_eq!(Fusion::from_config("linear"), Fusion::Linear);
        assert_eq!(
            Fusion::from_config("reciprocal-rank"),
            Fusion::Linear,
            "an unknown value degrades to the previous behaviour, it does not fail retrieval"
        );
    }

    #[test]
    fn union_max_keeps_the_best_instance_of_each_candidate() {
        let merged = union_max(vec![
            vec![candidate("memory:a", 0.4), candidate("memory:b", 0.9)],
            vec![candidate("memory:a", 0.7)],
        ]);
        assert_eq!(merged.len(), 2);
        let a = merged
            .iter()
            .find(|c| c.capsule.expansion_handle == "memory:a")
            .unwrap();
        assert!((a.raw_relevance - 0.7).abs() < f32::EPSILON);
    }

    /// The behaviour union-max cannot express: a candidate both lists rank
    /// highly should beat one that a single list ranks first and the other
    /// ranks last.
    #[test]
    fn rrf_rewards_agreement_between_lists() {
        // `both` is #2 lexically and #2 semantically.
        // `lexical_only` is #1 lexically and absent from the semantic list.
        let lexical = vec![
            candidate("memory:lexical_only", 0.95),
            candidate("memory:both", 0.90),
        ];
        let semantic = vec![
            candidate("memory:semantic_only", 0.95),
            candidate("memory:both", 0.90),
        ];

        let fused = rrf_fuse(vec![lexical.clone(), semantic.clone()], DEFAULT_RRF_K);
        assert_eq!(
            handles(&fused)[0],
            "memory:both",
            "agreement across lists must win: {:?}",
            handles(&fused)
        );

        // Under union-max the agreeing candidate stays behind both #1s,
        // because only its own best score counts.
        let unioned = union_max(vec![lexical, semantic]);
        let both_idx = handles(&unioned)
            .iter()
            .position(|h| *h == "memory:both")
            .unwrap();
        assert!(
            both_idx > 0,
            "union-max cannot see the agreement: {:?}",
            handles(&unioned)
        );
    }

    #[test]
    fn rrf_normalizes_the_top_score_to_one() {
        let fused = rrf_fuse(
            vec![
                vec![candidate("memory:a", 0.5), candidate("memory:b", 0.4)],
                vec![candidate("memory:a", 0.3)],
            ],
            DEFAULT_RRF_K,
        );
        assert!((fused[0].raw_relevance - 1.0).abs() < 1e-5);
        assert!(fused[1].raw_relevance < 1.0);
    }

    #[test]
    fn rrf_is_deterministic_on_ties() {
        let build = || {
            vec![
                vec![candidate("memory:b", 0.5)],
                vec![candidate("memory:a", 0.5)],
            ]
        };
        let first = handles(&rrf_fuse(build(), DEFAULT_RRF_K))
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>();
        let second = handles(&rrf_fuse(build(), DEFAULT_RRF_K))
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>();
        assert_eq!(first, second);
        assert_eq!(first[0], "memory:a", "ties break on the stable key");
    }

    /// A single list must come out in its original order: with nothing to fuse
    /// against, RRF is rank-preserving. This is the lean (FTS-only) build, so
    /// the fusion mode must be a no-op there rather than a reshuffle.
    #[test]
    fn rrf_over_one_list_preserves_its_order() {
        let single = vec![
            candidate("memory:first", 0.9),
            candidate("memory:second", 0.5),
            candidate("memory:third", 0.1),
        ];
        let fused = rrf_fuse(vec![single], DEFAULT_RRF_K);
        assert_eq!(
            handles(&fused),
            vec!["memory:first", "memory:second", "memory:third"]
        );
    }

    #[test]
    fn fuse_dispatches_on_the_mode() {
        let lists = || {
            vec![
                vec![candidate("memory:a", 0.4)],
                vec![candidate("memory:a", 0.8), candidate("memory:b", 0.2)],
            ]
        };
        let linear = fuse(Fusion::Linear, lists());
        let a = linear
            .iter()
            .find(|c| c.capsule.expansion_handle == "memory:a")
            .unwrap();
        assert!(
            (a.raw_relevance - 0.8).abs() < f32::EPSILON,
            "linear keeps the raw blended score"
        );

        let rrf = fuse(Fusion::Rrf, lists());
        let a = rrf
            .iter()
            .find(|c| c.capsule.expansion_handle == "memory:a")
            .unwrap();
        assert!(
            (a.raw_relevance - 1.0).abs() < 1e-5,
            "rrf rewrites relevance to the normalized fused score"
        );
    }
}
