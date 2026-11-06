//! Federated supersede links carried inside a signed pack.
//!
//! Packs still omit `memory_id`. A link names the predecessor and successor
//! by scrubbed text, kind, mount, and the HLC of the `memory.superseded`
//! event. Import writes that event through the existing projector. The same
//! pair is a no-op. A different successor is appended, then replay picks the
//! later HLC and records one `sync_conflicts` row. No model chooses a winner.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use kimetsu_core::KimetsuResult;
use kimetsu_core::clock::{self, Hlc};
use kimetsu_core::event::Event;
use kimetsu_core::ids::RunId;
use kimetsu_core::memory::{MountLevel, mount_stored_scopes, normalize_memory_text};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::json;

use crate::lock::ProjectLock;
use crate::packs::{self, MemoryExport, ScrubReport};
use crate::project::load_project;
use crate::projector;
use crate::signed_pack::SupersedeLink;

pub(crate) struct ApplyCounts {
    pub superseded: usize,
    pub conflicts: usize,
}

/// Supersede events on `level`, scrubbed the same way as the memory rows.
pub(crate) fn collect_links(
    start: &Path,
    level: MountLevel,
    redact: bool,
    redact_tags: bool,
) -> KimetsuResult<(Vec<SupersedeLink>, ScrubReport)> {
    if matches!(level, MountLevel::Run | MountLevel::Session) {
        return Err("export_pack refuses run and session mounts".into());
    }
    let scopes = mount_stored_scopes(level);
    let marks = (0..scopes.len())
        .map(|index| format!("?{}", index + 1))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT pred.kind, pred.text, succ.kind, succ.text,
                COALESCE(json_extract(ev.payload_json, '$.source_hlc'), ev.hlc)
         FROM events ev
         JOIN memories pred
           ON pred.memory_id = json_extract(ev.payload_json, '$.memory_id')
         JOIN memories succ
           ON succ.memory_id = json_extract(ev.payload_json, '$.survivor_id')
         WHERE ev.kind = 'memory.superseded'
           AND COALESCE(json_extract(ev.payload_json, '$.source_hlc'), ev.hlc) IS NOT NULL
           AND pred.invalidated_at IS NULL
           AND lower(pred.scope) IN ({marks})
         ORDER BY 5, pred.memory_id"
    );
    let (_paths, _config, conn) = load_project(start)?;
    let mut stmt = conn.prepare(&sql)?;
    let values: Vec<String> = scopes.iter().copied().map(str::to_string).collect();
    let refs: Vec<&dyn rusqlite::ToSql> = values.iter().map(|value| value as _).collect();
    let rows = stmt.query_map(refs.as_slice(), |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
        ))
    })?;
    let mut report = ScrubReport::default();
    let mut links = Vec::new();
    for row in rows {
        let (pred_kind, pred_text, succ_kind, succ_text, hlc) = row?;
        let predecessor = scrubbed(&pred_text, redact, redact_tags, &mut report);
        let successor = scrubbed(&succ_text, redact, redact_tags, &mut report);
        links.push(SupersedeLink {
            predecessor,
            predecessor_kind: pred_kind,
            successor,
            successor_kind: succ_kind,
            mount: level.as_str().to_string(),
            hlc,
        });
    }
    links.sort_by(|left, right| link_ord(left).cmp(&link_ord(right)));
    links.dedup();
    Ok((links, report))
}

/// Apply trusted links. Quarantine and replace stay with the caller: an
/// untrusted signer must not retire a local row, and replace already
/// invalidates the mount.
pub(crate) fn apply_links(
    start: &Path,
    destination: MountLevel,
    links: &[SupersedeLink],
) -> KimetsuResult<ApplyCounts> {
    if links.is_empty() {
        return Ok(ApplyCounts {
            superseded: 0,
            conflicts: 0,
        });
    }
    let mut ordered = links.to_vec();
    ordered.sort_by(|left, right| left.hlc.cmp(&right.hlc));
    let (paths, _config, conn) = load_project(start)?;
    let _lock = ProjectLock::acquire(&paths, "federated supersede", None)?;
    let mut superseded = 0usize;
    let mut conflicts = 0usize;
    projector::with_write_txn(&conn, |conn| {
        let mut known: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for link in &ordered {
            if let Some(parsed) = Hlc::parse(&link.hlc) {
                clock::observe(&parsed);
            }
            if link.mount != destination.as_str() {
                continue;
            }
            let pred_text = normalize_memory_text(&scrub_only(&link.predecessor));
            let succ_text = normalize_memory_text(&scrub_only(&link.successor));
            let Some(pred_id) =
                lookup(conn, destination, &link.predecessor_kind, &pred_text, false)?
            else {
                continue;
            };
            let Some(succ_id) = lookup(conn, destination, &link.successor_kind, &succ_text, true)?
            else {
                continue;
            };
            if pred_id == succ_id {
                continue;
            }
            if !known.contains_key(&pred_id) {
                let loaded = load_survivors(conn, &pred_id)?;
                known.insert(pred_id.clone(), loaded);
            }
            let Some(survivors) = known.get_mut(&pred_id) else {
                return Err("federated supersede lost the predecessor".into());
            };
            if survivors.contains(&succ_id) {
                continue;
            }
            let disagreed = !survivors.is_empty();
            // The event's own HLC is local and sorts after this brain's accept.
            // `source_hlc` is the peer's clock and is what orders two successors.
            // A raw remote HLC can sit before this brain's accept and replay
            // would then miss the row.
            let event = Event::new(
                RunId::new(),
                "memory.superseded",
                json!({
                    "memory_id": pred_id,
                    "survivor_id": succ_id,
                    "use_count_delta": 0,
                    "score_delta": 0.0,
                    "level": destination.as_str(),
                    "source_hlc": link.hlc,
                }),
            );
            projector::insert_event(conn, &event)?;
            survivors.insert(succ_id);
            if disagreed {
                conflicts += 1;
            } else {
                superseded += 1;
            }
        }
        if superseded + conflicts > 0 {
            projector::replay_locked(conn)?;
        }
        Ok(())
    })?;
    Ok(ApplyCounts {
        superseded,
        conflicts,
    })
}

fn scrubbed(text: &str, redact: bool, redact_tags: bool, report: &mut ScrubReport) -> String {
    let entry = packs::apply_export_redaction(
        MemoryExport {
            text: text.to_string(),
            scope: String::new(),
            kind: String::new(),
            confidence: 0.0,
            created_at: None,
        },
        redact,
        redact_tags,
    );
    let scrubbed = crate::redact::scrub_for_export(&entry.text);
    for hit in &scrubbed.matches {
        *report.kinds.entry(hit.kind.to_string()).or_insert(0) += 1;
        report.total += 1;
    }
    scrubbed.text
}

fn scrub_only(text: &str) -> String {
    crate::redact::scrub_for_export(text).text
}

fn link_ord(link: &SupersedeLink) -> (&str, &str, &str, &str, &str, &str) {
    (
        link.mount.as_str(),
        link.predecessor_kind.as_str(),
        link.predecessor.as_str(),
        link.successor_kind.as_str(),
        link.successor.as_str(),
        link.hlc.as_str(),
    )
}

fn lookup(
    conn: &Connection,
    destination: MountLevel,
    kind: &str,
    normalized: &str,
    active_only: bool,
) -> KimetsuResult<Option<String>> {
    let scopes = mount_stored_scopes(destination);
    let marks = (0..scopes.len())
        .map(|index| format!("?{}", index + 3))
        .collect::<Vec<_>>()
        .join(", ");
    let active = if active_only {
        "AND superseded_by IS NULL"
    } else {
        ""
    };
    let order = if active_only {
        "ORDER BY memory_id"
    } else {
        "ORDER BY (superseded_by IS NULL), memory_id"
    };
    let sql = format!(
        "SELECT memory_id FROM memories
         WHERE lower(kind) = lower(?1)
           AND normalized_text = ?2
           AND invalidated_at IS NULL
           {active}
           AND lower(scope) IN ({marks})
         {order}
         LIMIT 1"
    );
    let mut values = vec![kind.to_string(), normalized.to_string()];
    values.extend(scopes.iter().copied().map(str::to_string));
    let refs: Vec<&dyn rusqlite::ToSql> = values.iter().map(|value| value as _).collect();
    conn.query_row(&sql, refs.as_slice(), |row| row.get(0))
        .optional()
        .map_err(Into::into)
}

fn load_survivors(conn: &Connection, predecessor: &str) -> KimetsuResult<BTreeSet<String>> {
    let mut stmt = conn.prepare(
        "SELECT json_extract(payload_json, '$.survivor_id')
         FROM events
         WHERE kind = 'memory.superseded'
           AND json_extract(payload_json, '$.memory_id') = ?1",
    )?;
    let rows = stmt.query_map(params![predecessor], |row| row.get::<_, Option<String>>(0))?;
    let mut out = BTreeSet::new();
    for row in rows {
        if let Some(id) = row? {
            out.insert(id);
        }
    }
    Ok(out)
}
