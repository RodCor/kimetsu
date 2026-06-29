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

/// Set the process write origin. First call wins (idempotent thereafter), so
/// call it once during startup before any brain write. A blank/empty value is
/// normalized to `None` (unconfigured).
pub fn set_process_origin(origin: impl Into<String>) {
    let s = origin.into();
    let value = if s.trim().is_empty() { None } else { Some(s) };
    let _ = PROCESS_ORIGIN.set(value);
}

/// The configured process write origin, or `None` if unset.
pub fn process_origin() -> Option<String> {
    PROCESS_ORIGIN.get().cloned().flatten()
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
    /// v3.0 #3 Slice B: Hybrid Logical Clock timestamp (canonical string) giving
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
