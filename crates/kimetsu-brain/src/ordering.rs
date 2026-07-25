//! v2.6: answering "which came first".
//!
//! Event ordering is Kimetsu's worst measured ability by a wide margin — 32.5%
//! on BEAM 100K, 30% at 1M — and the reason is visible the moment you look at
//! what the reader actually receives.
//!
//! Memories carry `created_at`. Capsules do not. The broker renders the bundle
//! as an unordered set, sorted by relevance, with no timestamps anywhere in the
//! text. So a reader asked "did we switch to thiserror before or after the
//! schema migration?" is handed two memories in score order, carrying no dates,
//! and has nothing to order them *by*. It guesses. At two events that is a coin
//! flip, which is roughly what 30% looks like.
//!
//! Nothing about retrieval is wrong here; the information exists and is
//! selected. It is thrown away at render time.
//!
//! ## What this does
//!
//! When the query is asking about order, the bundle is re-rendered
//! chronologically with each memory's date attached. Relevance still decides
//! *which* memories are selected — this changes presentation, not selection, so
//! it cannot drop a capsule the broker chose or admit one it rejected.
//!
//! Deliberately narrow. Timestamping every capsule on every query would spend
//! tokens on the large majority of questions that are not about time, and
//! reordering a normal bundle away from relevance order would bury the best
//! answer. So [`is_ordering_query`] gates it on the question actually asking.

use crate::context::ContextCapsule;

/// Words that mark a question as being about sequence or time.
///
/// Matched on whole words against the lowercased query. Kept to terms whose
/// presence really does signal an ordering question — "when", "before",
/// "after", "first" — rather than anything vaguely temporal, because a false
/// positive reorders a bundle away from relevance for no reason.
const ORDERING_MARKERS: &[&str] = &[
    "before",
    "after",
    "first",
    "last",
    "latest",
    "earliest",
    "earlier",
    "later",
    "order",
    "ordering",
    "sequence",
    "chronological",
    "chronologically",
    "timeline",
    "when",
    "then",
    "initially",
    "originally",
    "eventually",
    "previously",
    "subsequently",
    "recent",
    "recently",
    "since",
    "until",
    "history",
];

/// True when the query is asking about order or time.
///
/// `_` counts as a word character, so an identifier like `ordering_service`
/// stays one token and does not trip the check — matching how
/// `context::content_tokens` splits, and avoiding the false positive where
/// refactoring a module named after a marker reorders the whole bundle.
pub fn is_ordering_query(query: &str) -> bool {
    let lower = query.to_ascii_lowercase();
    lower
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .any(|word| ORDERING_MARKERS.contains(&word))
}

/// The date part of an RFC 3339 timestamp (`2026-03-01T…` → `2026-03-01`).
///
/// Day granularity on purpose: it is what an ordering question is about, and a
/// full timestamp per capsule would cost tokens to say the same thing.
fn date_of(created_at: &str) -> &str {
    created_at.split('T').next().unwrap_or(created_at)
}

/// Insert `date` into a capsule summary, in front of the text.
///
/// A memory capsule's summary is `"scope:kind - text"`, and the context hook
/// renders only the part after the first `" - "` so the reader is not shown
/// Kimetsu's internal taxonomy. A date placed in front of the whole summary
/// would therefore be stripped by exactly the readability step it is meant to
/// survive — so it goes in front of the *text*, which is where the reader is
/// looking anyway. Summaries without that prefix (any future capsule shape)
/// get it at the front, which is the same position relative to their text.
fn dated_summary(summary: &str, date: &str) -> String {
    match summary.split_once(" - ") {
        Some((prefix, text)) => format!("{prefix} - [{date}] {text}"),
        None => format!("[{date}] {summary}"),
    }
}

/// Re-render `capsules` chronologically, dating each one.
///
/// `created_at` maps a capsule's `expansion_handle` to its RFC 3339 creation
/// time. A capsule with no entry — a repo file or manifest, which has no
/// position in the memory timeline — keeps its relative order after the dated
/// ones rather than being dropped or given a fabricated date.
///
/// Returns the capsules in chronological order, oldest first, because that is
/// the order the question is about.
pub fn render_chronologically(
    capsules: Vec<ContextCapsule>,
    created_at: &std::collections::HashMap<String, String>,
) -> Vec<ContextCapsule> {
    let mut dated: Vec<(String, ContextCapsule)> = Vec::new();
    let mut undated: Vec<ContextCapsule> = Vec::new();

    for capsule in capsules {
        match created_at.get(&capsule.expansion_handle) {
            Some(ts) => dated.push((ts.clone(), capsule)),
            None => undated.push(capsule),
        }
    }

    // Stable sort: equal timestamps keep the broker's relevance order, so a
    // batch of memories written in the same second stays sensibly ranked.
    dated.sort_by(|a, b| a.0.cmp(&b.0));

    let mut out: Vec<ContextCapsule> = dated
        .into_iter()
        .map(|(ts, mut capsule)| {
            capsule.summary = dated_summary(&capsule.summary, date_of(&ts));
            // The date is real tokens, so account for them.
            capsule.token_estimate = capsule.token_estimate.saturating_add(4);
            capsule
        })
        .collect();
    out.append(&mut undated);
    out
}

/// A one-line preamble telling the reader the bundle is in time order.
///
/// Without it a chronological bundle looks like a relevance-ranked one whose
/// ranking has gone wrong.
pub const CHRONOLOGICAL_NOTE: &str =
    "These memories are in chronological order, oldest first, with the date each was recorded.";

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn capsule(handle: &str, summary: &str) -> ContextCapsule {
        ContextCapsule {
            id: String::new(),
            kind: "memory".to_string(),
            summary: summary.to_string(),
            token_estimate: 10,
            expansion_handle: handle.to_string(),
            provenance: Vec::new(),
            confidence: 0.9,
            freshness: 0.5,
            relevance: 0.0,
            scope_weight: 0.9,
            score: 0.5,
        }
    }

    fn handles(capsules: &[ContextCapsule]) -> Vec<&str> {
        capsules
            .iter()
            .map(|c| c.expansion_handle.as_str())
            .collect()
    }

    #[test]
    fn ordering_questions_are_recognised() {
        for query in [
            "did we switch to thiserror before or after the migration",
            "what came first, the parser or the lexer",
            "when did we adopt edition 2024",
            "show me the timeline of schema changes",
            "what did we do most recently",
        ] {
            assert!(is_ordering_query(query), "should be ordering: {query:?}");
        }
    }

    /// A false positive reorders a bundle away from relevance for no reason,
    /// so ordinary questions must not trip it.
    #[test]
    fn ordinary_questions_are_not_ordering_questions() {
        for query in [
            "how do I checkpoint the wal",
            "add error handling to the parser",
            "why does the build fail",
            "what is the schema version",
        ] {
            assert!(
                !is_ordering_query(query),
                "should not be ordering: {query:?}"
            );
        }
    }

    /// Matching must be on whole words: "afterthought" is not "after".
    #[test]
    fn markers_match_whole_words_only() {
        assert!(!is_ordering_query("this was an afterthought"));
        assert!(!is_ordering_query("refactor the ordering_service module"));
        assert!(is_ordering_query("refactor the ordering service"));
    }

    /// The fix itself: relevance chose the memories, time decides how they are
    /// shown, and each carries the date the reader needs to compare.
    #[test]
    fn capsules_are_reordered_by_time_and_dated() {
        let mut created = HashMap::new();
        created.insert("memory:b".to_string(), "2026-01-15T10:00:00Z".to_string());
        created.insert("memory:a".to_string(), "2026-06-01T09:00:00Z".to_string());

        // Broker order is by relevance: `a` first.
        let ordered = render_chronologically(
            vec![
                capsule("memory:a", "project:fact - switched to thiserror"),
                capsule("memory:b", "project:fact - migrated the schema"),
            ],
            &created,
        );

        assert_eq!(
            handles(&ordered),
            vec!["memory:b", "memory:a"],
            "oldest first"
        );
        assert_eq!(
            ordered[0].summary,
            "project:fact - [2026-01-15] migrated the schema"
        );
        assert_eq!(
            ordered[1].summary,
            "project:fact - [2026-06-01] switched to thiserror"
        );
    }

    /// The context hook renders only the part of a summary after the first
    /// `" - "`. A date the reader never sees orders nothing, so the date has to
    /// survive that step — this is the regression that made it a suffix of the
    /// prefix rather than a prefix of the whole line.
    #[test]
    fn the_date_survives_the_hooks_summary_stripping() {
        let mut created = HashMap::new();
        created.insert("memory:a".to_string(), "2026-01-15T10:00:00Z".to_string());
        let ordered = render_chronologically(
            vec![capsule("memory:a", "project:fact - switched to thiserror")],
            &created,
        );
        let shown = ordered[0]
            .summary
            .split(" - ")
            .nth(1)
            .expect("hook renders the text half");
        assert!(shown.starts_with("[2026-01-15] "), "got: {shown}");
    }

    /// A summary with no `scope:kind - ` prefix still gets dated, in the same
    /// position relative to its text.
    #[test]
    fn a_prefixless_summary_is_dated_at_the_front() {
        let mut created = HashMap::new();
        created.insert("memory:a".to_string(), "2026-01-15T10:00:00Z".to_string());
        let ordered = render_chronologically(vec![capsule("memory:a", "bare text")], &created);
        assert_eq!(ordered[0].summary, "[2026-01-15] bare text");
    }

    /// A repo file has no position in the memory timeline. It must not be
    /// dropped, and must not be given a date it does not have.
    #[test]
    fn undated_capsules_are_kept_after_the_timeline() {
        let mut created = HashMap::new();
        created.insert("memory:a".to_string(), "2026-01-01T00:00:00Z".to_string());

        let ordered = render_chronologically(
            vec![
                capsule("repo_file:src/main.rs", "repo_file:src/main.rs - fn main"),
                capsule("memory:a", "project:fact - a thing"),
            ],
            &created,
        );
        assert_eq!(
            handles(&ordered),
            vec!["memory:a", "repo_file:src/main.rs"],
            "dated first, undated kept: {:?}",
            handles(&ordered)
        );
        assert!(
            !ordered[1].summary.contains('['),
            "an undated capsule must not be given a date: {}",
            ordered[1].summary
        );
    }

    #[test]
    fn equal_timestamps_keep_the_brokers_order() {
        let mut created = HashMap::new();
        created.insert("memory:a".to_string(), "2026-01-01T00:00:00Z".to_string());
        created.insert("memory:b".to_string(), "2026-01-01T00:00:00Z".to_string());
        let ordered = render_chronologically(
            vec![
                capsule("memory:a", "project:fact - best match"),
                capsule("memory:b", "project:fact - second"),
            ],
            &created,
        );
        assert_eq!(handles(&ordered), vec!["memory:a", "memory:b"]);
    }

    /// Presentation only: the same capsules come out, never more or fewer.
    #[test]
    fn reordering_never_adds_or_drops_a_capsule() {
        let mut created = HashMap::new();
        created.insert("memory:a".to_string(), "2026-03-01T00:00:00Z".to_string());
        let input = vec![
            capsule("memory:a", "a"),
            capsule("memory:b", "b"),
            capsule("repo_file:x", "x"),
        ];
        let ordered = render_chronologically(input.clone(), &created);
        assert_eq!(ordered.len(), input.len());
        let mut got = handles(&ordered);
        got.sort_unstable();
        let mut want = handles(&input);
        want.sort_unstable();
        assert_eq!(got, want);
    }

    #[test]
    fn the_token_estimate_accounts_for_the_date_prefix() {
        let mut created = HashMap::new();
        created.insert("memory:a".to_string(), "2026-03-01T00:00:00Z".to_string());
        let ordered = render_chronologically(vec![capsule("memory:a", "a")], &created);
        assert!(ordered[0].token_estimate > 10, "the prefix costs tokens");
    }
}
