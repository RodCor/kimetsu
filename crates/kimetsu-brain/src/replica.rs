//! ADR 0001: directory sync and the bus are two carriers of one policy.
//!
//! A signed or unsigned pack is not a JSONL event-log batch. A directory
//! batch is not a pack. Callers refuse the other carrier before SQL.

use serde_json::Value;

/// Which replication carrier these bytes belong to, if either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplicaClass {
    /// Whitespace only.
    Empty,
    /// `kimetsu_pack` document, gzip is handled by the caller, or a bare memory array.
    Pack,
    /// One `SyncEvent` JSON object, or JSONL of them.
    DirectoryBatch,
    /// Neither carrier. Callers apply their own parser error.
    Other,
}

/// Directory import must not apply these bytes.
pub const PACK_AS_BATCH: &str =
    "sync import refuses a kimetsu_pack; a directory batch is not a pack";

/// Pack verify and the unsigned v1 parser must not apply these bytes.
pub const BATCH_AS_PACK: &str = "a directory sync batch is not a kimetsu_pack";

/// Gzip magic. A directory batch is uncompressed JSONL.
pub const GZIP_AS_BATCH: &str = "sync import refuses a gzip pack; a directory batch is not a pack";

/// Classify UTF-8 replica bytes. Gzip is not UTF-8; callers check the magic first.
pub fn classify_replica_text(text: &str) -> ReplicaClass {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return ReplicaClass::Empty;
    }
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        return classify_value(&value);
    }
    let mut saw_event = false;
    for line in trimmed.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("kimetsu_pack").is_some() {
            return ReplicaClass::Pack;
        }
        if is_sync_event(&value) {
            saw_event = true;
        }
    }
    if saw_event {
        ReplicaClass::DirectoryBatch
    } else {
        ReplicaClass::Other
    }
}

/// True when `bytes` is a gzip member. Directory sync does not gunzip packs.
pub fn is_gzip_pack(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0x1f, 0x8b])
}

fn classify_value(value: &Value) -> ReplicaClass {
    if value.get("kimetsu_pack").is_some() || value.is_array() {
        return ReplicaClass::Pack;
    }
    if is_sync_event(value) {
        return ReplicaClass::DirectoryBatch;
    }
    ReplicaClass::Other
}

fn is_sync_event(value: &Value) -> bool {
    value.get("event_id").and_then(Value::as_str).is_some()
        && value.get("kind").and_then(Value::as_str).is_some()
        && value.get("kimetsu_pack").is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_and_batch_are_different_carriers() {
        let pack = r#"{"kimetsu_pack":2,"memories":[],"memory_count":0}"#;
        assert_eq!(classify_replica_text(pack), ReplicaClass::Pack);
        let pretty = "{\n  \"kimetsu_pack\": 1,\n  \"memories\": []\n}\n";
        assert_eq!(classify_replica_text(pretty), ReplicaClass::Pack);
        assert_eq!(
            classify_replica_text(r#"[{"text":"lesson","scope":"repo","kind":"fact"}]"#),
            ReplicaClass::Pack
        );

        let event =
            r#"{"event_id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","kind":"memory.accepted","payload":{}}"#;
        assert_eq!(classify_replica_text(event), ReplicaClass::DirectoryBatch);
        let jsonl = format!("{event}\n{event}\n");
        assert_eq!(classify_replica_text(&jsonl), ReplicaClass::DirectoryBatch);

        assert_eq!(classify_replica_text("   \n"), ReplicaClass::Empty);
        assert_eq!(classify_replica_text("not json"), ReplicaClass::Other);
        assert!(is_gzip_pack(&[0x1f, 0x8b, 0x08]));
        assert!(!is_gzip_pack(b"{\"event_id\":\"x\"}"));
    }
}
