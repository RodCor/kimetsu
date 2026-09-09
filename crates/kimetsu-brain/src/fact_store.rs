//! Rebuildable evidence derived only from redacted memory text.
use crate::facts::FactClaim;
use kimetsu_core::KimetsuResult;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredFact {
    pub memory_id: String,
    pub claim_revision: String,
    pub source_event_id: String,
    pub valid_from: Option<String>,
    pub valid_to: Option<String>,
    pub claim: FactClaim,
}

/// Caller holds the event/migration write transaction.
pub fn refresh(conn: &Connection, memory_id: &str) -> KimetsuResult<()> {
    conn.execute("DELETE FROM memory_facts WHERE memory_id=?1", [memory_id])?;
    let row: Option<(String, Option<String>)> = conn
        .query_row(
            "SELECT text,source_event_id FROM memories WHERE memory_id=?1",
            [memory_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((text, accepted_event)) = row else {
        return Ok(());
    };
    let revision = crate::projector::claim_revision_at(conn, memory_id, None)?;
    let source = if revision.starts_with("baseline:") {
        accepted_event.unwrap_or_else(|| revision.clone())
    } else {
        revision.clone()
    };
    // Defense in depth for legacy backfills. Never duplicate raw legacy secrets.
    let redacted = crate::redact::redact_secrets(&text).text;
    let digest = blake3::hash(text.as_bytes()).to_hex().to_string();
    let mut insert=conn.prepare_cached("INSERT INTO memory_facts(memory_id,claim_revision,ordinal,source_event_id,source_digest,claim_json) VALUES(?1,?2,?3,?4,?5,?6)")?;
    for (ordinal, claim) in crate::facts::extract(&redacted).into_iter().enumerate() {
        insert.execute(params![
            memory_id,
            revision,
            ordinal as i64,
            source,
            digest,
            serde_json::to_string(&claim)?
        ])?;
    }
    Ok(())
}

/// Caller holds the migration write transaction.
pub fn backfill(conn: &Connection) -> KimetsuResult<()> {
    let mut stmt = conn.prepare("SELECT memory_id FROM memories ORDER BY memory_id")?;
    let ids = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);
    for id in ids {
        refresh(conn, &id)?;
    }
    Ok(())
}

/// Current evidence only. The text, lifecycle and revision predicates are read
/// in one SQLite statement. Retrieval calls this while its text SELECT remains
/// active, so both queries share the capsule's read snapshot.
pub fn load(
    conn: &Connection,
    memory_id: &str,
    claim_revision: &str,
) -> KimetsuResult<Vec<StoredFact>> {
    let mut stmt = conn.prepare_cached(
        "SELECT f.source_event_id,m.valid_from,m.valid_to,f.claim_json,f.source_digest,m.text
        FROM memory_facts f JOIN memories m ON m.memory_id=f.memory_id
        WHERE f.memory_id=?1 AND f.claim_revision=?2
          AND m.invalidated_at IS NULL AND m.superseded_by IS NULL
          AND (m.valid_from IS NULL OR julianday(m.valid_from)<=julianday('now'))
          AND (m.valid_to IS NULL OR julianday(m.valid_to)>julianday('now'))
          AND f.claim_revision=COALESCE((SELECT event_id FROM (
            SELECT event_id,revision_id,text,LAG(text) OVER (ORDER BY revision_id) AS previous_text
            FROM memory_revisions WHERE memory_id=?1)
            WHERE previous_text IS NULL OR text!=previous_text
            ORDER BY revision_id DESC LIMIT 1),'baseline:' || m.memory_id)
        ORDER BY f.ordinal",
    )?;
    let rows = stmt.query_map(params![memory_id, claim_revision], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, String>(5)?,
        ))
    })?;
    let mut out = Vec::new();
    let mut source_digest = None;
    for row in rows {
        let (source_event_id, valid_from, valid_to, json, digest, text) = row?;
        // Historical temporary views may swap text without changing revisions.
        let current_digest =
            source_digest.get_or_insert_with(|| blake3::hash(text.as_bytes()).to_hex().to_string());
        if current_digest.as_str() != digest {
            continue;
        }
        out.push(StoredFact {
            memory_id: memory_id.into(),
            claim_revision: claim_revision.into(),
            source_event_id,
            valid_from,
            valid_to,
            claim: serde_json::from_str(&json)?,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use crate::{projector, schema};
    use kimetsu_core::{event::Event, ids::RunId};
    use rusqlite::Connection;

    fn event(kind: &str, payload: serde_json::Value) -> Event {
        Event::new(RunId::new(), kind, payload)
    }
    fn seeded() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        schema::initialize(&c).unwrap();
        projector::apply_events(
            &c,
            &[event(
                "memory.accepted",
                serde_json::json!({
                    "memory_id":"m", "scope":"project", "kind":"fact",
                    "text":"Orchid staging gateway port is 7319."
                }),
            )],
        )
        .unwrap();
        c
    }
    #[test]
    fn acceptance_projects_redacted_claim_and_correction_replaces_it_on_replay() {
        let c = seeded();
        let count: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM memory_facts WHERE memory_id='m'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
        projector::apply_events(
            &c,
            &[event(
                "memory.corrected",
                serde_json::json!({
                    "memory_id":"m", "text":"Orchid staging gateway port is 8420."
                }),
            )],
        )
        .unwrap();
        let before: String = c
            .query_row(
                "SELECT claim_json FROM memory_facts WHERE memory_id='m'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(before.contains("8420"));
        assert!(!before.contains("7319"));
        projector::rebuild_in_place(&c).unwrap();
        let after: String = c
            .query_row(
                "SELECT claim_json FROM memory_facts WHERE memory_id='m'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(before, after);
    }
}

#[cfg(test)]
mod migration_tests {
    use rusqlite::Connection;
    #[test]
    fn schema_fifteen_backfills_existing_text() {
        let c = Connection::open_in_memory().unwrap();
        crate::schema::initialize(&c).unwrap();
        c.execute("INSERT INTO memories(memory_id,scope,kind,text,normalized_text,confidence,created_at,provenance_snapshot_json) VALUES('legacy','project','fact','Orchid staging gateway port is 7319.','',1,'2026-01-01T00:00:00Z','{}')",[]).unwrap();
        c.execute_batch("DROP TABLE IF EXISTS memory_facts; UPDATE schema_info SET value=14 WHERE key='kimetsu_schema_version'").unwrap();
        crate::migrate::run_migrations(&c).unwrap();
        let count: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM memory_facts WHERE memory_id='legacy'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }
}

#[cfg(test)]
mod transaction_tests {
    use kimetsu_core::{event::Event, ids::RunId};
    use rusqlite::Connection;
    #[test]
    fn failed_event_batch_rolls_back_fact_projection() {
        let c = Connection::open_in_memory().unwrap();
        crate::schema::initialize(&c).unwrap();
        let events = [
            Event::new(
                RunId::new(),
                "memory.accepted",
                serde_json::json!({"memory_id":"m","text":"Orchid gateway port is 7319."}),
            ),
            Event::new(
                RunId::new(),
                "memory.corrected",
                serde_json::json!({"memory_id":"missing","text":"Orchid gateway port is 8420."}),
            ),
        ];
        assert!(crate::projector::apply_events(&c, &events).is_err());
        let count: i64 = c
            .query_row("SELECT COUNT(*) FROM memory_facts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }
}

#[cfg(test)]
mod load_tests {
    use super::*;
    use kimetsu_core::{event::Event, ids::RunId};
    fn apply(c: &Connection, kind: &str, payload: serde_json::Value) -> Event {
        let e = Event::new(RunId::new(), kind, payload);
        crate::projector::apply_events(c, std::slice::from_ref(&e)).unwrap();
        e
    }
    fn seeded() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        crate::schema::initialize(&c).unwrap();
        apply(
            &c,
            "memory.accepted",
            serde_json::json!({"memory_id":"m","text":"Orchid staging gateway port is 7319."}),
        );
        c
    }
    #[test]
    fn revisions_and_text_must_match_and_provenance_tracks_text_changes() {
        let c = seeded();
        let original = load(&c, "m", "baseline:m").unwrap();
        assert_eq!(original.len(), 1);
        let correction = apply(
            &c,
            "memory.corrected",
            serde_json::json!({"memory_id":"m","text":"Orchid staging gateway port is 8420."}),
        );
        assert!(load(&c, "m", "baseline:m").unwrap().is_empty());
        let revision = correction.event_id.to_string();
        let changed = load(&c, "m", &revision).unwrap();
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].claim.value, "8420");
        assert_eq!(changed[0].source_event_id, revision);
        apply(
            &c,
            "memory.corrected",
            serde_json::json!({"memory_id":"m","kind":"preference"}),
        );
        assert_eq!(load(&c, "m", &revision).unwrap(), changed);
        // The historical retrieval clone can retain revision rows but swap text.
        c.execute(
            "UPDATE memories SET text='Orchid staging gateway port is 7319.' WHERE memory_id='m'",
            [],
        )
        .unwrap();
        assert!(load(&c, "m", &revision).unwrap().is_empty());
    }
    #[test]
    fn lifecycle_and_temporal_bounds_are_authoritative_at_read_time() {
        let c = seeded();
        for update in [
            "invalidated_at='2026-01-01T00:00:00Z'",
            "superseded_by='other'",
            "valid_from='2999-01-01T00:00:00Z'",
            "valid_to='2020-01-01T00:00:00Z'",
        ] {
            c.execute(
                &format!("UPDATE memories SET {update} WHERE memory_id='m'"),
                [],
            )
            .unwrap();
            assert!(load(&c, "m", "baseline:m").unwrap().is_empty(), "{update}");
            c.execute("UPDATE memories SET invalidated_at=NULL,superseded_by=NULL,valid_from=NULL,valid_to=NULL WHERE memory_id='m'",[]).unwrap();
            assert_eq!(load(&c, "m", "baseline:m").unwrap().len(), 1);
        }
        apply(
            &c,
            "memory.temporal",
            serde_json::json!({"memory_id":"m","valid_from":"2020-01-01T00:00:00Z","valid_to":"2999-01-01T00:00:00Z"}),
        );
        let projected = load(&c, "m", "baseline:m").unwrap();
        assert_eq!(
            projected[0].valid_from.as_deref(),
            Some("2020-01-01T00:00:00Z")
        );
        assert_eq!(
            projected[0].valid_to.as_deref(),
            Some("2999-01-01T00:00:00Z")
        );
        crate::projector::rebuild_in_place(&c).unwrap();
        assert_eq!(load(&c, "m", "baseline:m").unwrap(), projected);
    }
    #[test]
    fn ingestion_and_legacy_backfill_do_not_project_secrets() {
        let c = seeded();
        let secret = "ghp_abcdefghijklmnopqrstuvwxyz1234567890ABCD";
        apply(
            &c,
            "memory.accepted",
            serde_json::json!({"memory_id":"secret","text":format!("Orchid gateway password is {secret}. Orchid gateway port is 7319.")}),
        );
        let rows: Vec<String> = c
            .prepare("SELECT claim_json FROM memory_facts")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(rows.iter().all(|s| !s.contains(secret)));
        c.execute(
            "UPDATE memories SET text=?1 WHERE memory_id='secret'",
            [format!("Orchid gateway password is {secret}.")],
        )
        .unwrap();
        backfill(&c).unwrap();
        assert!(load(&c, "secret", "baseline:secret").unwrap().is_empty());
    }
}
