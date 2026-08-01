use std::cell::RefCell;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;

use crate::EVENT_SCHEMA_VERSION;
use crate::ids::{EventId, RunId};

/// Process-global write origin, set once at startup (CLI / MCP server) and
/// stamped onto every locally-created [`Event`]. `None` until configured.
/// Format is `<machine_id>/<agent>` (e.g. `laptop-01/claude-code`), so a shared
/// or replicated brain can attribute each event to the device + agent that wrote
/// it. Imported events keep their REMOTE origin (set explicitly), never this one.
static PROCESS_ORIGIN: OnceLock<Option<String>> = OnceLock::new();

thread_local! {
    /// Per-thread write origin override. Takes precedence over [`PROCESS_ORIGIN`]
    /// when set. The multi-user remote server (kimetsu-remote) runs each request
    /// on one `spawn_blocking` thread and uses [`OriginScope`] to attribute that
    /// request's writes to the authenticated USER — something the process-global
    /// (a write-once `OnceLock`) cannot do. Unset for normal CLI/agent processes.
    static THREAD_ORIGIN: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Set the process write origin. First call wins (idempotent thereafter), so
/// call it once during startup before any brain write. A blank/empty value is
/// normalized to `None` (unconfigured).
pub fn set_process_origin(origin: impl Into<String>) {
    let s = origin.into();
    let value = if s.trim().is_empty() { None } else { Some(s) };
    let _ = PROCESS_ORIGIN.set(value);
}

/// The effective write origin for the current thread: the thread-local override
/// ([`OriginScope`]) if set, else the process-global, else `None`.
pub fn process_origin() -> Option<String> {
    if let Some(o) = THREAD_ORIGIN.with(|c| c.borrow().clone()) {
        return Some(o);
    }
    PROCESS_ORIGIN.get().cloned().flatten()
}

/// RAII guard that overrides the write origin for the current thread for its
/// lifetime, restoring the previous value on drop. Required for the remote
/// server: tokio reuses blocking threads, so a bare set would leak one request's
/// user into the next request on the same thread. Empty input is treated as "no
/// override" (the guard still restores the prior value on drop).
#[must_use]
pub struct OriginScope {
    prev: Option<String>,
}

impl OriginScope {
    pub fn new(origin: impl Into<String>) -> Self {
        let s = origin.into();
        let value = if s.trim().is_empty() { None } else { Some(s) };
        let prev = THREAD_ORIGIN.with(|c| c.replace(value));
        OriginScope { prev }
    }
}

impl Drop for OriginScope {
    fn drop(&mut self) {
        let prev = self.prev.take();
        THREAD_ORIGIN.with(|c| *c.borrow_mut() = prev);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub event_id: EventId,
    pub run_id: RunId,
    #[serde(with = "time::serde::rfc3339")]
    pub ts: OffsetDateTime,
    pub parent_event_id: Option<EventId>,
    pub kind: String,
    pub schema_version: u32,
    pub payload: Value,
    /// Who/where wrote this event: `<machine_id>/<agent>`, or `None` for events
    /// created before origin tracking (schema < v8) or when unconfigured.
    /// Auto-stamped from [`process_origin`] by [`Event::new`]; preserved verbatim
    /// across rebuild and sync replication.
    #[serde(default)]
    pub origin: Option<String>,
    /// v2.6 #3 Slice B: Hybrid Logical Clock timestamp (canonical string) giving
    /// a globally-deterministic, causal total order for convergent team sync.
    /// `None` for events created before HLC tracking (schema < v9); the v9
    /// migration backfills those from `(ts, rowid)`. Auto-stamped by
    /// [`Event::new`]; preserved verbatim across rebuild and replication.
    #[serde(default)]
    pub hlc: Option<String>,
}

impl Event {
    pub fn new(run_id: RunId, kind: impl Into<String>, payload: Value) -> Self {
        Self {
            event_id: EventId::new(),
            run_id,
            ts: OffsetDateTime::now_utc(),
            parent_event_id: None,
            kind: kind.into(),
            schema_version: EVENT_SCHEMA_VERSION,
            payload,
            origin: process_origin(),
            hlc: Some(crate::clock::now().to_canonical()),
        }
    }

    pub fn with_parent(mut self, parent_event_id: EventId) -> Self {
        self.parent_event_id = Some(parent_event_id);
        self
    }

    /// Override the origin (used by the sync import path to preserve a remote
    /// event's origin instead of stamping the local process origin).
    pub fn with_origin(mut self, origin: Option<String>) -> Self {
        self.origin = origin;
        self
    }

    /// Override the HLC (used by the sync import path to preserve a remote
    /// event's HLC instead of stamping the local clock).
    pub fn with_hlc(mut self, hlc: Option<String>) -> Self {
        self.hlc = hlc;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_scope_overrides_and_restores() {
        // No override → falls through to the process-global (None here in tests).
        assert_eq!(process_origin(), None);

        {
            let _s = OriginScope::new("srv1/user:alice");
            assert_eq!(process_origin().as_deref(), Some("srv1/user:alice"));
            // A fresh event picks up the thread origin.
            let e = Event::new(RunId::new(), "memory.cited", serde_json::json!({}));
            assert_eq!(e.origin.as_deref(), Some("srv1/user:alice"));

            // Nesting restores the previous override on inner drop.
            {
                let _inner = OriginScope::new("srv1/user:bob");
                assert_eq!(process_origin().as_deref(), Some("srv1/user:bob"));
            }
            assert_eq!(process_origin().as_deref(), Some("srv1/user:alice"));
        }

        // Outer guard dropped → cleared (no leak to the next request on this thread).
        assert_eq!(process_origin(), None);
    }

    #[test]
    fn origin_scope_empty_is_no_override() {
        let _s = OriginScope::new("");
        assert_eq!(process_origin(), None);
    }
}
