use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;

use crate::EVENT_SCHEMA_VERSION;
use crate::ids::{EventId, RunId};

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
        }
    }

    pub fn with_parent(mut self, parent_event_id: EventId) -> Self {
        self.parent_event_id = Some(parent_event_id);
        self
    }
}
