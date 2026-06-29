//! v3.0 #3 Slice B: Hybrid Logical Clock (HLC) for convergent team sync.
//!
//! An HLC stamps every event with a timestamp that is (a) globally
//! lexicographically sortable, (b) monotonic on a single brain, and (c) CAUSAL
//! across brains — receiving a remote event advances the local clock past it, so
//! any later local event sorts after everything observed. Replaying a merged
//! event log in HLC order is therefore deterministic on every brain, which makes
//! the projection converge field-by-field (last-writer-in-HLC-order wins) without
//! per-field bookkeeping. This generalizes the single-brain `(ts, rowid)` causal
//! order to the multi-brain case.
//!
//! The canonical wire/storage form is `"{wall_ms:013}.{counter:010}.{node}"` —
//! zero-padded so plain string comparison equals causal comparison. 13 digits
//! covers epoch-millis through year 5138; 10 digits covers the full u32 counter
//! range (the counter only grows within a single ms, then resets). The 10-digit
//! width is also wide enough for the v9 migration to backfill a row's `rowid`
//! into the counter slot (`wall = 0`), so old events sort before new ones by a
//! consistent string width.

use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// A Hybrid Logical Clock timestamp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hlc {
    pub wall_ms: u64,
    pub counter: u32,
    pub node: String,
}

impl Hlc {
    /// Canonical sortable string: `{wall_ms:013}.{counter:010}.{node}`.
    pub fn to_canonical(&self) -> String {
        format!("{:013}.{:010}.{}", self.wall_ms, self.counter, self.node)
    }

    /// Parse a canonical string back into an `Hlc`. The node may itself contain
    /// `.` (e.g. `machine.local`), so only the first two `.`-separated fields are
    /// structured; the remainder is the node.
    pub fn parse(s: &str) -> Option<Hlc> {
        let mut it = s.splitn(3, '.');
        let wall_ms = it.next()?.parse::<u64>().ok()?;
        let counter = it.next()?.parse::<u32>().ok()?;
        let node = it.next()?.to_string();
        Some(Hlc {
            wall_ms,
            counter,
            node,
        })
    }
}

struct State {
    wall_ms: u64,
    counter: u32,
}

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(|| {
        Mutex::new(State {
            wall_ms: 0,
            counter: 0,
        })
    })
}

/// Process-global node id (the machine part of the write origin). Defaults to
/// `"local"` until [`set_node`] is called at startup.
fn node_cell() -> &'static OnceLock<String> {
    static NODE: OnceLock<String> = OnceLock::new();
    &NODE
}

/// Set this process's HLC node id once at startup (first call wins). Use the
/// machine part of the write origin so equal `(wall, counter)` ties break by
/// machine — a globally consistent total order. Empty input is ignored.
pub fn set_node(node: impl Into<String>) {
    let n = node.into();
    if !n.trim().is_empty() {
        let _ = node_cell().set(n);
    }
}

fn node() -> String {
    node_cell()
        .get()
        .cloned()
        .unwrap_or_else(|| "local".to_string())
}

fn physical_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Generate the next local HLC timestamp (monotonic). Within the same wall
/// millisecond the counter increments; a newer wall clock resets it to 0.
pub fn now() -> Hlc {
    let mut st = state().lock().unwrap_or_else(|p| p.into_inner());
    let phys = physical_now_ms();
    if phys > st.wall_ms {
        st.wall_ms = phys;
        st.counter = 0;
    } else {
        // Same or backwards physical clock → keep the logical wall, bump counter.
        st.counter = st.counter.saturating_add(1);
    }
    Hlc {
        wall_ms: st.wall_ms,
        counter: st.counter,
        node: node(),
    }
}

/// Observe a remote HLC (on sync import): advance the local clock past
/// `max(physical, local, remote)` so every subsequent local event sorts AFTER
/// everything received — the causality guarantee that makes total-order replay
/// deterministic across brains.
pub fn observe(remote: &Hlc) {
    let mut st = state().lock().unwrap_or_else(|p| p.into_inner());
    let phys = physical_now_ms();
    let max_wall = st.wall_ms.max(remote.wall_ms).max(phys);
    if max_wall == st.wall_ms && max_wall == remote.wall_ms {
        // All three share a wall ms → counter must exceed both seen counters.
        st.counter = st.counter.max(remote.counter).saturating_add(1);
    } else if max_wall == remote.wall_ms {
        // Remote's wall dominates → adopt it, counter just past the remote's.
        st.wall_ms = max_wall;
        st.counter = remote.counter.saturating_add(1);
    } else if max_wall == st.wall_ms {
        // Local wall still dominates → keep advancing the local counter.
        st.counter = st.counter.saturating_add(1);
    } else {
        // Physical clock dominates both → fresh tick.
        st.wall_ms = max_wall;
        st.counter = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_is_strictly_increasing() {
        let a = now();
        let b = now();
        let c = now();
        assert!(
            a.to_canonical() < b.to_canonical(),
            "{} !< {}",
            a.to_canonical(),
            b.to_canonical()
        );
        assert!(b.to_canonical() < c.to_canonical());
    }

    #[test]
    fn observe_advances_past_far_future_remote() {
        let remote = Hlc {
            wall_ms: physical_now_ms() + 1_000_000, // ~16 min in the future
            counter: 42,
            node: "other".to_string(),
        };
        observe(&remote);
        let next = now();
        assert!(
            next.to_canonical() > remote.to_canonical(),
            "local clock must advance past an observed future remote: {} !> {}",
            next.to_canonical(),
            remote.to_canonical()
        );
    }

    #[test]
    fn canonical_sorts_chronologically_and_breaks_ties_by_node() {
        let earlier = Hlc {
            wall_ms: 100,
            counter: 5,
            node: "z".into(),
        };
        let later_wall = Hlc {
            wall_ms: 101,
            counter: 0,
            node: "a".into(),
        };
        assert!(earlier.to_canonical() < later_wall.to_canonical());

        let same_a = Hlc {
            wall_ms: 100,
            counter: 5,
            node: "a".into(),
        };
        let same_b = Hlc {
            wall_ms: 100,
            counter: 5,
            node: "b".into(),
        };
        assert!(
            same_a.to_canonical() < same_b.to_canonical(),
            "equal (wall,counter) must break by node"
        );
    }

    #[test]
    fn parse_roundtrips_including_dotted_node() {
        let h = Hlc {
            wall_ms: 1234567890,
            counter: 7,
            node: "laptop.local".into(),
        };
        let parsed = Hlc::parse(&h.to_canonical()).expect("parse");
        assert_eq!(parsed, h);
    }
}
