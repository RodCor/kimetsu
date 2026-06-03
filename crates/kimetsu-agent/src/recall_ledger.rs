//! v1.1 (E/F): a per-run, in-memory record of what the brain has already
//! surfaced and injected during a single coding run.
//!
//! Generalized from the interactive hook's `proactive_state` dedupe logic, but
//! scoped to one run and held in memory (the autonomous pipeline has no need to
//! persist it). Two consumers share it:
//!   * **F1 — cross-stage capsule dedup:** [`is_injected`](RunRecallLedger::is_injected)
//!     / [`mark_injected`](RunRecallLedger::mark_injected) so a capsule that is
//!     top-ranked in several stages is rendered in full once and back-referenced
//!     afterwards, with its token cost counted a single time. The more stages a
//!     task runs, the more duplication disappears — overhead shrinks *with* task
//!     size.
//!   * **E1/E2 — proactive recall dedup:** [`is_surfaced`](RunRecallLedger::is_surfaced)
//!     / [`mark_surfaced`](RunRecallLedger::mark_surfaced) so a pitfall/convention
//!     surfaced before the first attempt is not re-surfaced on a retry.
//!
//! The ledger never decides *what* to recall — it only remembers what already
//! was, so the renderers and proactive passes don't re-pay or re-warn.

use std::collections::{HashMap, HashSet};

/// Per-run recall bookkeeping. Cheap to construct; one per run.
#[derive(Debug, Default, Clone)]
pub struct RunRecallLedger {
    /// Capsule id → the token estimate charged at its FIRST injection. A
    /// capsule re-encountered in a later stage is a back-reference and is not
    /// charged again, so the map's value is the once-counted cost.
    injected: HashMap<String, u32>,
    /// Opaque keys for proactive items already surfaced this run (e.g. a
    /// failure-pattern memory id), so a retry doesn't repeat the same warning.
    surfaced: HashSet<String>,
}

impl RunRecallLedger {
    /// A fresh, empty ledger for a new run.
    pub fn new() -> Self {
        Self::default()
    }

    /// Has this capsule id already been injected (in any prior stage) this run?
    pub fn is_injected(&self, capsule_id: &str) -> bool {
        self.injected.contains_key(capsule_id)
    }

    /// Record a capsule's first injection and the token cost charged for it.
    /// Idempotent: a repeat call for an already-injected id keeps the ORIGINAL
    /// token charge (a back-reference must never inflate the once-counted cost).
    pub fn mark_injected(&mut self, capsule_id: impl Into<String>, tokens: u32) {
        self.injected.entry(capsule_id.into()).or_insert(tokens);
    }

    /// Total tokens charged for brain-injected capsules this run, counting each
    /// capsule exactly once regardless of how many stages referenced it.
    pub fn injected_tokens(&self) -> u32 {
        self.injected.values().copied().sum()
    }

    /// How many distinct capsules have been injected this run.
    pub fn injected_count(&self) -> usize {
        self.injected.len()
    }

    /// Has this proactive item already been surfaced this run?
    pub fn is_surfaced(&self, key: &str) -> bool {
        self.surfaced.contains(key)
    }

    /// Mark a proactive item surfaced so it isn't repeated on a retry.
    pub fn mark_surfaced(&mut self, key: impl Into<String>) {
        self.surfaced.insert(key.into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_ledger_is_empty() {
        let l = RunRecallLedger::new();
        assert!(!l.is_injected("m1"));
        assert_eq!(l.injected_tokens(), 0);
        assert_eq!(l.injected_count(), 0);
        assert!(!l.is_surfaced("p1"));
    }

    #[test]
    fn mark_injected_tracks_membership_and_tokens() {
        let mut l = RunRecallLedger::new();
        l.mark_injected("m1", 50);
        l.mark_injected("m2", 30);
        assert!(l.is_injected("m1"));
        assert!(l.is_injected("m2"));
        assert!(!l.is_injected("m3"));
        assert_eq!(l.injected_tokens(), 80);
        assert_eq!(l.injected_count(), 2);
    }

    #[test]
    fn reinjection_counts_tokens_once_keeping_original_charge() {
        let mut l = RunRecallLedger::new();
        l.mark_injected("m1", 50);
        // Same capsule re-encountered in a later stage (a back-reference): the
        // ledger must NOT add a second charge nor overwrite the first.
        l.mark_injected("m1", 999);
        assert_eq!(l.injected_count(), 1);
        assert_eq!(l.injected_tokens(), 50);
    }

    #[test]
    fn surfaced_dedup_for_proactive_recall() {
        let mut l = RunRecallLedger::new();
        assert!(!l.is_surfaced("fail:rg-not-found"));
        l.mark_surfaced("fail:rg-not-found");
        assert!(l.is_surfaced("fail:rg-not-found"));
        // Marking twice is harmless and idempotent.
        l.mark_surfaced("fail:rg-not-found");
        assert!(l.is_surfaced("fail:rg-not-found"));
    }
}
