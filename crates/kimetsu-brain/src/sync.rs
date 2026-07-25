/// Epic S3 — Personal brain sync (event-log replication).
///
/// Design insight: `events` is the durable source of truth and the projector
/// rebuilds everything from it.  Sync = EVENT-LOG REPLICATION, not SQLite file
/// copying.  We export durable events and import them through the projector with
/// per-event idempotency.  No server, no merge daemon.
///
/// # Allowed (durable memory-lifecycle) kinds — exported
/// - `memory.accepted`
/// - `memory.proposed`
/// - `memory.rejected`
/// - `memory.invalidated`
/// - `memory.cited`
/// - `memory.superseded`
///
/// # Excluded kinds — never exported
/// - `work.episode`         — episodes are LOCAL-ONLY (Flagship 1)
/// - `context.served`       — local telemetry
/// - `retrieval.regret`     — local telemetry
/// - `digest_served`        — local telemetry
/// - `resume_served`        — local telemetry
/// - `context.injected`     — raw query bearing
/// - `run.started`          — local run metadata
/// - `run.finished`         — local run metadata
/// - `run.failed`           — local run metadata
/// - `run.aborted`          — local run metadata
///
/// Everything else that is not on the allowlist is also excluded by default.
///
/// # Cursor
/// The monotonic ordering column is `rowid` (the implicit SQLite integer
/// primary key alias).  A cursor is the last exported `rowid`.  The next
/// export picks up WHERE rowid > cursor.  Cursor 0 means "from the beginning".
///
/// # Idempotency
/// Import checks `event_id` (ULID) against the local `events` table.
/// `INSERT OR IGNORE` in `insert_event` already provides this, but we also
/// count skipped events so the caller can report applied/skipped.
///
/// # Directory protocol (3.2)
/// `<sync_dir>/<machine_id>/<cursor>.jsonl` — each batch file is atomically
/// written (temp + rename).  A per-source-cursor registry lives at
/// `.kimetsu/sync-cursors.json`.  `kimetsu brain sync` (no args):
///   1. Write this machine's new events under `<sync_dir>/<machine_id>/`.
///   2. For every OTHER subdirectory (= other machine), read batches after
///      the locally stored cursor for that machine, import them (idempotent),
///      and advance the cursor.
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Write as IoWrite};
use std::path::{Path, PathBuf};

use kimetsu_core::KimetsuResult;
use kimetsu_core::event::Event;
use kimetsu_core::ids::{EventId, RunId};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use ulid::Ulid;

use crate::projector;
use crate::redact;

// ---------------------------------------------------------------------------
// Allowlist
// ---------------------------------------------------------------------------

/// Kinds that carry durable memory-lifecycle meaning and SHOULD be replicated.
const SYNC_ALLOWED_KINDS: &[&str] = &[
    "memory.accepted",
    "memory.proposed",
    "memory.rejected",
    "memory.invalidated",
    "memory.cited",
    "memory.superseded",
];

/// Returns `true` when `kind` is allowed in a sync batch.
pub fn is_sync_allowed(kind: &str) -> bool {
    SYNC_ALLOWED_KINDS.contains(&kind)
}

// ---------------------------------------------------------------------------
// Event wire format
// ---------------------------------------------------------------------------

/// One line in a sync JSONL batch.  Carries the full event so the remote
/// projector can replay it.  `payload` is the redacted payload (same
/// redaction the projector applies at ingest).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncEvent {
    pub event_id: String,
    pub run_id: String,
    #[serde(with = "time::serde::rfc3339")]
    pub ts: OffsetDateTime,
    pub kind: String,
    pub schema_version: u32,
    pub payload: serde_json::Value,
    /// v2.6 #3: who/where wrote this event (`<machine_id>/<agent>`). Carried so a
    /// replicated/team brain can attribute each event. `#[serde(default)]` keeps
    /// pre-v8 sync batches (no `origin`) importable.
    #[serde(default)]
    pub origin: Option<String>,
    /// v2.6 #3 Slice B: the event's HLC (canonical string) for convergent
    /// total-order replay. `#[serde(default)]` keeps pre-v9 batches importable
    /// (the importer synthesizes a local HLC for those).
    #[serde(default)]
    pub hlc: Option<String>,
}

impl From<&Event> for SyncEvent {
    fn from(e: &Event) -> Self {
        // Apply the same payload redaction the projector does so sync batches
        // are never a second secret store (matches projector::redact_memory_event).
        let payload = redact_event_payload(e);
        Self {
            event_id: e.event_id.to_string(),
            run_id: e.run_id.to_string(),
            ts: e.ts,
            kind: e.kind.clone(),
            schema_version: e.schema_version,
            payload,
            origin: e.origin.clone(),
            hlc: e.hlc.clone(),
        }
    }
}

impl TryFrom<SyncEvent> for Event {
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn try_from(s: SyncEvent) -> Result<Self, Self::Error> {
        let event_id = EventId(
            Ulid::from_string(&s.event_id)
                .map_err(|e| format!("invalid event_id {:?}: {e}", s.event_id))?,
        );
        let run_id = RunId(
            Ulid::from_string(&s.run_id)
                .map_err(|e| format!("invalid run_id {:?}: {e}", s.run_id))?,
        );
        // Preserve the REMOTE HLC; advance the local clock past it so subsequent
        // LOCAL events sort after everything imported (causality). A pre-v9 peer
        // sends no HLC → synthesize a current local one so the event still sorts.
        let hlc = match s.hlc {
            Some(h) => {
                if let Some(parsed) = kimetsu_core::clock::Hlc::parse(&h) {
                    kimetsu_core::clock::observe(&parsed);
                }
                Some(h)
            }
            None => Some(kimetsu_core::clock::now().to_canonical()),
        };
        Ok(Event {
            event_id,
            run_id,
            ts: s.ts,
            parent_event_id: None,
            kind: s.kind,
            schema_version: s.schema_version,
            payload: s.payload,
            // Preserve the REMOTE origin — do NOT stamp the local process origin.
            origin: s.origin,
            hlc,
        })
    }
}

/// Apply export-time redaction to the event payload — same logic as
/// `projector::redact_memory_event` but returns an owned `Value`.
fn redact_event_payload(event: &Event) -> serde_json::Value {
    if !matches!(
        event.kind.as_str(),
        "memory.accepted" | "memory.proposed" | "memory.cited"
    ) {
        return event.payload.clone();
    }
    redact_json_strings_owned(&event.payload)
}

fn redact_json_strings_owned(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::String(text) => {
            serde_json::Value::String(redact::redact_secrets(text).text)
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(redact_json_strings_owned).collect())
        }
        serde_json::Value::Object(map) => {
            let out = map
                .iter()
                .map(|(k, v)| (k.clone(), redact_json_strings_owned(v)))
                .collect();
            serde_json::Value::Object(out)
        }
        other => other.clone(),
    }
}

// ---------------------------------------------------------------------------
// 3.1 — Export
// ---------------------------------------------------------------------------

/// Summary returned by [`export_events`].
#[derive(Debug, Clone, Default)]
pub struct ExportSummary {
    /// Number of events written to the batch.
    pub exported: usize,
    /// The highest rowid included in this batch (= next cursor).
    pub next_cursor: i64,
}

/// Export durable events from `conn` after `since_rowid` (exclusive).
///
/// Only events whose `kind` is on the sync allowlist are included.
/// Redaction is applied inline (no workspace paths / secrets leak).
///
/// When `out_path` is `None`, returns the JSONL as a `String`.
/// When `out_path` is `Some(path)`, writes atomically via temp+rename.
pub fn export_events(
    conn: &Connection,
    since_rowid: i64,
    out_path: Option<&Path>,
    dry_run: bool,
) -> KimetsuResult<(ExportSummary, Option<String>)> {
    let rows = read_durable_events_after(conn, since_rowid)?;

    let mut lines = Vec::new();
    let mut next_cursor = since_rowid;
    for (rowid, event) in &rows {
        if !is_sync_allowed(&event.kind) {
            continue;
        }
        let se = SyncEvent::from(event);
        let line = serde_json::to_string(&se)
            .map_err(|e| format!("sync export: serialize event {}: {e}", event.event_id))?;
        lines.push(line);
        if *rowid > next_cursor {
            next_cursor = *rowid;
        }
    }

    let summary = ExportSummary {
        exported: lines.len(),
        next_cursor,
    };

    if dry_run {
        return Ok((summary, None));
    }

    let jsonl = lines.join("\n");
    if let Some(path) = out_path {
        atomic_write(path, jsonl.as_bytes())?;
        Ok((summary, None))
    } else {
        Ok((summary, Some(jsonl)))
    }
}

/// Read all (rowid, Event) pairs from the `events` table with rowid > `after`.
fn read_durable_events_after(conn: &Connection, after: i64) -> KimetsuResult<Vec<(i64, Event)>> {
    let mut stmt = conn.prepare(
        "SELECT rowid, event_id, run_id, ts, kind, schema_version, payload_json, origin, hlc
         FROM events
         WHERE rowid > ?1
         ORDER BY rowid",
    )?;
    let rows = stmt.query_map(rusqlite::params![after], |row| {
        let rowid: i64 = row.get(0)?;
        let event_id_str: String = row.get(1)?;
        let run_id_str: String = row.get(2)?;
        let ts_str: String = row.get(3)?;
        let kind: String = row.get(4)?;
        let schema_version: u32 = row.get(5)?;
        let payload_json: String = row.get(6)?;
        let origin: Option<String> = row.get(7)?;
        let hlc: Option<String> = row.get(8)?;
        Ok((
            rowid,
            event_id_str,
            run_id_str,
            ts_str,
            kind,
            schema_version,
            payload_json,
            origin,
            hlc,
        ))
    })?;

    let mut out = Vec::new();
    for row in rows {
        let (
            rowid,
            event_id_str,
            run_id_str,
            ts_str,
            kind,
            schema_version,
            payload_json,
            origin,
            hlc,
        ) = row?;
        let event_id = EventId(
            Ulid::from_string(&event_id_str)
                .map_err(|e| format!("invalid event_id {event_id_str:?}: {e}"))?,
        );
        let run_id = RunId(
            Ulid::from_string(&run_id_str)
                .map_err(|e| format!("invalid run_id {run_id_str:?}: {e}"))?,
        );
        let ts = OffsetDateTime::parse(&ts_str, &Rfc3339)
            .map_err(|e| format!("invalid ts {ts_str:?}: {e}"))?;
        let payload: serde_json::Value = serde_json::from_str(&payload_json)?;
        out.push((
            rowid,
            Event {
                event_id,
                run_id,
                ts,
                parent_event_id: None,
                kind,
                schema_version,
                payload,
                origin,
                hlc,
            },
        ));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// 3.1 — Import
// ---------------------------------------------------------------------------

/// Summary returned by [`import_events`].
#[derive(Debug, Clone, Default)]
pub struct ImportSummary {
    /// Events actually applied (projected into derived tables).
    pub applied: usize,
    /// Events skipped because their `event_id` already existed locally.
    pub skipped: usize,
}

/// Import a JSONL batch (one `SyncEvent` per line) into `conn`.
///
/// Per-event idempotency: if the `event_id` already exists in the local
/// `events` table, the event is skipped (no double-apply).
/// `INSERT OR IGNORE` in `projector::insert_event` provides the underlying
/// dedup; we additionally count skips for reporting.
///
/// When `dry_run` is true, parse and count but do NOT write anything.
pub fn import_events(
    conn: &Connection,
    jsonl: &str,
    dry_run: bool,
) -> KimetsuResult<ImportSummary> {
    let mut summary = ImportSummary::default();
    for (line_no, line) in jsonl.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let se: SyncEvent = serde_json::from_str(line)
            .map_err(|e| format!("sync import: malformed JSON on line {}: {e}", line_no + 1))?;

        // Validate it's an allowed kind — defence-in-depth (the exporter
        // already filters, but a hand-crafted batch might not).
        if !is_sync_allowed(&se.kind) {
            // Skip silently — telemetry/local kinds should never appear.
            summary.skipped += 1;
            continue;
        }

        let event: Event = Event::try_from(se)
            .map_err(|e| format!("sync import: invalid event on line {}: {e}", line_no + 1))?;

        // Check whether event_id already exists.
        let exists: bool = conn
            .query_row(
                "SELECT 1 FROM events WHERE event_id = ?1",
                rusqlite::params![event.event_id.to_string()],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false);

        if exists {
            summary.skipped += 1;
            continue;
        }

        if dry_run {
            summary.applied += 1;
            continue;
        }

        // Apply through the projector: inserts into events table + projects
        // into derived tables.  The projector's `apply_events` wraps in a
        // transaction; we call it one event at a time to keep the
        // applied/skipped tally accurate.
        projector::apply_events(conn, &[event])?;
        summary.applied += 1;
    }
    Ok(summary)
}

/// Slice B: count unresolved concurrent-supersede conflicts surfaced by team
/// sync (a member superseded to two different survivors). Deterministic across
/// brains; shown by `kimetsu brain sync --status`.
pub fn sync_conflict_count(conn: &Connection) -> KimetsuResult<i64> {
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM sync_conflicts", [], |r| r.get(0))?;
    Ok(n)
}

/// Read a JSONL batch file and import it.
pub fn import_events_from_file(
    conn: &Connection,
    path: &Path,
    dry_run: bool,
) -> KimetsuResult<ImportSummary> {
    let file = fs::File::open(path)
        .map_err(|e| format!("sync import: cannot open {:?}: {e}", path.display()))?;
    let reader = BufReader::new(file);
    let mut buf = String::new();
    for line in reader.lines() {
        let l = line.map_err(|e| format!("sync import: read error {:?}: {e}", path.display()))?;
        buf.push_str(&l);
        buf.push('\n');
    }
    import_events(conn, &buf, dry_run)
}

// ---------------------------------------------------------------------------
// 3.2 — Sync cursor registry
// ---------------------------------------------------------------------------

/// Per-source cursor state persisted at `.kimetsu/sync-cursors.json`.
///
/// Keys are machine_id strings; values are the last rowid imported from that
/// machine.  Our OWN machine_id is in here too (last exported rowid).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyncCursors {
    /// map machine_id → last imported rowid (0 = never imported).
    #[serde(default)]
    pub sources: BTreeMap<String, i64>,
}

impl SyncCursors {
    pub fn load(path: &Path) -> KimetsuResult<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(path)
            .map_err(|e| format!("sync-cursors: cannot read {:?}: {e}", path.display()))?;
        serde_json::from_str(&text)
            .map_err(|e| format!("sync-cursors: malformed JSON at {:?}: {e}", path.display()))
            .map_err(Into::into)
    }

    pub fn save(&self, path: &Path) -> KimetsuResult<()> {
        let text = serde_json::to_string_pretty(self)
            .map_err(|e| format!("sync-cursors: serialize error: {e}"))?;
        atomic_write(path, text.as_bytes())
    }

    pub fn cursor_for(&self, machine_id: &str) -> i64 {
        *self.sources.get(machine_id).unwrap_or(&0)
    }

    pub fn set_cursor(&mut self, machine_id: &str, rowid: i64) {
        self.sources.insert(machine_id.to_string(), rowid);
    }
}

// ---------------------------------------------------------------------------
// 3.2 — Directory protocol
// ---------------------------------------------------------------------------

/// The max rowid among all rows in the local `events` table with an allowed
/// kind.  This is what we compare against the stored export cursor to decide
/// whether there's anything new to push.
pub fn max_local_sync_rowid(conn: &Connection) -> KimetsuResult<i64> {
    let placeholders: String = SYNC_ALLOWED_KINDS
        .iter()
        .enumerate()
        .map(|(i, _)| format!("?{}", i + 1))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!("SELECT COALESCE(MAX(rowid), 0) FROM events WHERE kind IN ({placeholders})");
    let mut stmt = conn.prepare(&sql)?;
    let params: Vec<Box<dyn rusqlite::ToSql>> = SYNC_ALLOWED_KINDS
        .iter()
        .map(|k| -> Box<dyn rusqlite::ToSql> { Box::new(k.to_string()) })
        .collect();
    let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let max: i64 = stmt.query_row(refs.as_slice(), |r| r.get(0))?;
    Ok(max)
}

/// Write this machine's new events as a JSONL batch under
/// `<sync_dir>/<machine_id>/<cursor>.jsonl` (atomic write).
///
/// Returns the number of events written and the new export cursor.
pub fn push_machine_batch(
    conn: &Connection,
    sync_dir: &Path,
    machine_id: &str,
    since_rowid: i64,
    dry_run: bool,
) -> KimetsuResult<ExportSummary> {
    let (dry_summary, _content) = export_events(conn, since_rowid, None, true)?;
    if dry_summary.exported == 0 || dry_run {
        return Ok(dry_summary);
    }

    // Re-run for real (not dry_run) to get the content.
    let (summary, content) = export_events(conn, since_rowid, None, false)?;
    let jsonl = content.unwrap_or_default();

    let machine_dir = sync_dir.join(machine_id);
    fs::create_dir_all(&machine_dir).map_err(|e| {
        format!(
            "sync push: cannot create dir {:?}: {e}",
            machine_dir.display()
        )
    })?;

    let batch_name = format!("{}.jsonl", summary.next_cursor);
    let batch_path = machine_dir.join(&batch_name);
    atomic_write(&batch_path, jsonl.as_bytes())?;

    Ok(summary)
}

/// Pull and import all batches from `<sync_dir>/<source_machine_id>/` that
/// come AFTER `since_cursor`.  Updates the cursor in the registry.
///
/// Batches are files named `<rowid>.jsonl`; we sort numerically and process
/// only those whose stem > since_cursor.
pub fn pull_machine_batches(
    conn: &Connection,
    sync_dir: &Path,
    source_machine_id: &str,
    since_cursor: i64,
    dry_run: bool,
) -> KimetsuResult<(ImportSummary, i64)> {
    let machine_dir = sync_dir.join(source_machine_id);
    if !machine_dir.exists() {
        return Ok((ImportSummary::default(), since_cursor));
    }

    // Collect batch files, parse their numeric stem (= the export cursor at
    // the time they were written, i.e. the highest rowid in that batch on
    // the source machine).
    let mut batches: Vec<(i64, PathBuf)> = Vec::new();
    let entries = fs::read_dir(&machine_dir).map_err(|e| {
        format!(
            "sync pull: cannot read dir {:?}: {e}",
            machine_dir.display()
        )
    })?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("sync pull: dir entry error: {e}"))?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            if let Ok(cursor_val) = stem.parse::<i64>() {
                if cursor_val > since_cursor {
                    batches.push((cursor_val, path));
                }
            }
        }
    }
    batches.sort_by_key(|(c, _)| *c);

    let mut total = ImportSummary::default();
    let mut new_cursor = since_cursor;
    for (cursor_val, batch_path) in &batches {
        let batch_summary = import_events_from_file(conn, batch_path, dry_run)?;
        total.applied += batch_summary.applied;
        total.skipped += batch_summary.skipped;
        if *cursor_val > new_cursor {
            new_cursor = *cursor_val;
        }
    }
    Ok((total, new_cursor))
}

/// Full sync cycle:
/// 1. Push this machine's new events.
/// 2. Pull every other machine's new batches.
/// 3. Persist updated cursors.
///
/// Returns a summary of what happened.
pub fn sync_dir(
    conn: &Connection,
    sync_dir: &Path,
    machine_id: &str,
    cursors_path: &Path,
    dry_run: bool,
) -> KimetsuResult<SyncReport> {
    let mut cursors = SyncCursors::load(cursors_path)?;
    let export_since = cursors.cursor_for(machine_id);

    // --- push ---
    let push_summary = push_machine_batch(conn, sync_dir, machine_id, export_since, dry_run)?;
    if !dry_run && push_summary.exported > 0 {
        cursors.set_cursor(machine_id, push_summary.next_cursor);
        cursors.save(cursors_path)?;
    }

    // --- pull ---
    let mut total_applied = 0usize;
    let mut total_skipped = 0usize;
    let mut machines_pulled: Vec<String> = Vec::new();

    // List subdirs (each = one machine's batch directory).
    if sync_dir.exists() {
        let entries = fs::read_dir(sync_dir)
            .map_err(|e| format!("sync: cannot read sync_dir {:?}: {e}", sync_dir.display()))?;
        let mut other_machines: Vec<String> = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| format!("sync: dir entry error: {e}"))?;
            if entry.path().is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    if name != machine_id {
                        other_machines.push(name.to_string());
                    }
                }
            }
        }
        other_machines.sort(); // deterministic order

        for other_id in &other_machines {
            let since = cursors.cursor_for(other_id);
            let (pull_summary, new_cursor) =
                pull_machine_batches(conn, sync_dir, other_id, since, dry_run)?;
            total_applied += pull_summary.applied;
            total_skipped += pull_summary.skipped;
            if !dry_run && new_cursor > since {
                cursors.set_cursor(other_id, new_cursor);
                machines_pulled.push(other_id.clone());
            } else if dry_run && (pull_summary.applied + pull_summary.skipped) > 0 {
                machines_pulled.push(other_id.clone());
            }
        }

        if !dry_run && !machines_pulled.is_empty() {
            cursors.save(cursors_path)?;
        }
    }

    // Slice B: total-order replay. After importing peer events (which were
    // applied incrementally in arrival order), re-project the merged log in HLC
    // order so this brain converges to the SAME state every peer reaches,
    // independent of import order. Skipped when nothing was pulled.
    if !dry_run && total_applied > 0 {
        projector::rebuild_in_place(conn)?;
    }

    Ok(SyncReport {
        pushed: push_summary.exported,
        pulled_applied: total_applied,
        pulled_skipped: total_skipped,
        machines_pulled,
        dry_run,
    })
}

/// Summary of a full sync cycle.
#[derive(Debug, Clone, Default)]
pub struct SyncReport {
    pub pushed: usize,
    pub pulled_applied: usize,
    pub pulled_skipped: usize,
    pub machines_pulled: Vec<String>,
    pub dry_run: bool,
}

// ---------------------------------------------------------------------------
// 3.3 — Doctor / status
// ---------------------------------------------------------------------------

/// Status of the sync configuration and state.
#[derive(Debug, Clone)]
pub struct SyncStatus {
    pub sync_dir: Option<PathBuf>,
    pub machine_id: String,
    /// Per-source: (machine_id, cursor, pending_count)
    pub sources: Vec<(String, i64, usize)>,
    pub local_pending: usize,
}

/// Compute the sync status without performing any writes.
pub fn sync_status(
    conn: &Connection,
    sync_dir_opt: Option<&Path>,
    machine_id: &str,
    cursors_path: &Path,
) -> KimetsuResult<SyncStatus> {
    let cursors = SyncCursors::load(cursors_path)?;
    let export_since = cursors.cursor_for(machine_id);

    // Count this machine's unpushed events.
    let (push_dry, _) = export_events(conn, export_since, None, true)?;
    let local_pending = push_dry.exported;

    let mut sources: Vec<(String, i64, usize)> = Vec::new();
    if let Some(sd) = sync_dir_opt {
        if sd.exists() {
            let entries = fs::read_dir(sd)
                .map_err(|e| format!("sync status: cannot read {:?}: {e}", sd.display()))?;
            let mut other_machines: Vec<String> = Vec::new();
            for entry in entries {
                let entry = entry.map_err(|e| format!("sync status: dir entry error: {e}"))?;
                if entry.path().is_dir() {
                    if let Some(name) = entry.file_name().to_str() {
                        if name != machine_id {
                            other_machines.push(name.to_string());
                        }
                    }
                }
            }
            other_machines.sort();
            for other_id in &other_machines {
                let since = cursors.cursor_for(other_id);
                let (pull_summary, _) = pull_machine_batches(conn, sd, other_id, since, true)?;
                sources.push((
                    other_id.clone(),
                    since,
                    pull_summary.applied + pull_summary.skipped,
                ));
            }
        }
    }

    Ok(SyncStatus {
        sync_dir: sync_dir_opt.map(|p| p.to_path_buf()),
        machine_id: machine_id.to_string(),
        sources,
        local_pending,
    })
}

// ---------------------------------------------------------------------------
// Atomic write helper
// ---------------------------------------------------------------------------

/// Write `data` to `path` atomically via a sibling temp file + rename.
pub fn atomic_write(path: &Path, data: &[u8]) -> KimetsuResult<()> {
    let parent = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(parent).map_err(|e| {
        format!(
            "atomic_write: cannot create dir {:?}: {e}",
            parent.display()
        )
    })?;
    let tmp_path = path.with_extension("tmp");
    {
        let mut file = fs::File::create(&tmp_path).map_err(|e| {
            format!(
                "atomic_write: cannot create tmp {:?}: {e}",
                tmp_path.display()
            )
        })?;
        file.write_all(data)
            .map_err(|e| format!("atomic_write: write error {:?}: {e}", tmp_path.display()))?;
        file.flush()
            .map_err(|e| format!("atomic_write: flush error {:?}: {e}", tmp_path.display()))?;
    }
    fs::rename(&tmp_path, path).map_err(|e| {
        format!(
            "atomic_write: rename {:?} -> {:?}: {e}",
            tmp_path.display(),
            path.display()
        )
    })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Extension trait for Option with rusqlite
// ---------------------------------------------------------------------------

trait OptionalExt<T> {
    fn optional(self) -> KimetsuResult<Option<T>>;
}

impl<T> OptionalExt<T> for rusqlite::Result<T> {
    fn optional(self) -> KimetsuResult<Option<T>> {
        match self {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use kimetsu_core::ids::RunId;
    use rusqlite::Connection;
    use serde_json::json;

    use super::*;
    use crate::projector::apply_events;
    use crate::schema;

    fn make_conn() -> Connection {
        let conn = Connection::open_in_memory().expect("open_in_memory");
        schema::initialize(&conn).expect("schema init");
        conn
    }

    fn seed_events(conn: &Connection) -> (RunId, String, String) {
        let run_id = RunId::new();
        let mem_id_a = format!("mem-{}", ulid::Ulid::new());
        let mem_id_b = format!("mem-{}", ulid::Ulid::new());
        apply_events(
            conn,
            &[
                kimetsu_core::event::Event::new(
                    run_id,
                    "run.started",
                    json!({"project_id":"p","task":"t"}),
                ),
                kimetsu_core::event::Event::new(
                    run_id,
                    "memory.accepted",
                    json!({"memory_id": mem_id_a, "text": "always use cargo --locked", "scope": "project", "kind": "fact"}),
                ),
                kimetsu_core::event::Event::new(
                    run_id,
                    "memory.accepted",
                    json!({"memory_id": mem_id_b, "text": "prefer ripgrep over grep", "scope": "global_user", "kind": "preference"}),
                ),
                kimetsu_core::event::Event::new(
                    run_id,
                    "work.episode",
                    json!({"task":"local task","project_id":"p"}),
                ),
                kimetsu_core::event::Event::new(
                    run_id,
                    "context.served",
                    json!({"query":"test","results":[]}),
                ),
                kimetsu_core::event::Event::new(
                    run_id,
                    "run.finished",
                    json!({"total_cost_usd":0.01}),
                ),
            ],
        )
        .expect("seed events");
        (run_id, mem_id_a, mem_id_b)
    }

    // Slice B headline: two brains that exchange the same events CONVERGE to an
    // identical projection regardless of the order edits were made/imported —
    // including the one genuinely-divergent op (memory.superseded), which an HLC
    // replay resolves last-writer-wins, plus a surfaced conflict.
    #[test]
    fn two_brains_converge_after_exchange() {
        use kimetsu_core::event::Event;
        let a = make_conn();
        let b = make_conn();
        let run = RunId(ulid::Ulid::nil()); // sentinel → standalone cite outcome
        let (m1, s1, s2) = ("mem-m1", "mem-s1", "mem-s2");

        // Shared base: identical accepted events on both brains.
        let base = vec![
            Event::new(
                run,
                "memory.accepted",
                json!({"memory_id": m1, "text":"alpha rule", "scope":"project","kind":"fact"}),
            ),
            Event::new(
                run,
                "memory.accepted",
                json!({"memory_id": s1, "text":"survivor one", "scope":"project","kind":"fact"}),
            ),
            Event::new(
                run,
                "memory.accepted",
                json!({"memory_id": s2, "text":"survivor two", "scope":"project","kind":"fact"}),
            ),
        ];
        apply_events(&a, &base).unwrap();
        apply_events(&b, &base).unwrap();

        // Divergent edits, created in sequence so B's supersede has a LATER HLC.
        let a_mut = vec![
            Event::new(run, "memory.cited", json!({"memory_id": m1, "turn": 0})),
            Event::new(
                run,
                "memory.superseded",
                json!({"memory_id": m1, "survivor_id": s1}),
            ),
        ];
        apply_events(&a, &a_mut).unwrap();
        let b_mut = vec![
            Event::new(run, "memory.cited", json!({"memory_id": m1, "turn": 0})),
            Event::new(
                run,
                "memory.superseded",
                json!({"memory_id": m1, "survivor_id": s2}),
            ),
        ];
        apply_events(&b, &b_mut).unwrap();

        // Cross-exchange the full logs, then converge (rebuild in HLC order).
        let ax = export_events(&a, 0, None, false).unwrap().1.unwrap();
        let bx = export_events(&b, 0, None, false).unwrap().1.unwrap();
        import_events(&b, &ax, false).unwrap();
        import_events(&a, &bx, false).unwrap();
        crate::projector::rebuild_in_place(&a).unwrap();
        crate::projector::rebuild_in_place(&b).unwrap();

        // superseded_by converges to the LATER-HLC survivor (s2) on BOTH brains.
        let superseded = |c: &Connection| -> Option<String> {
            c.query_row(
                "SELECT superseded_by FROM memories WHERE memory_id = ?1",
                [m1],
                |r| r.get::<_, Option<String>>(0),
            )
            .unwrap()
        };
        assert_eq!(
            superseded(&a),
            superseded(&b),
            "superseded_by must converge"
        );
        assert_eq!(
            superseded(&a),
            Some(s2.to_string()),
            "later-HLC supersede wins deterministically"
        );

        // Additive field (use_count) converges; both cites counted.
        let use_count = |c: &Connection| -> i64 {
            c.query_row(
                "SELECT use_count FROM memories WHERE memory_id = ?1",
                [m1],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(use_count(&a), use_count(&b), "use_count must converge");
        assert_eq!(use_count(&a), 2, "both brains' cites counted");

        // Even order-sensitive confidence converges (same HLC replay order).
        let confidence = |c: &Connection| -> f64 {
            c.query_row(
                "SELECT confidence FROM memories WHERE memory_id = ?1",
                [m1],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert!(
            (confidence(&a) - confidence(&b)).abs() < 1e-9,
            "confidence must converge: {} vs {}",
            confidence(&a),
            confidence(&b)
        );

        // The genuine concurrent supersede is surfaced (once) on both brains.
        assert_eq!(sync_conflict_count(&a).unwrap(), 1);
        assert_eq!(sync_conflict_count(&b).unwrap(), 1);
    }

    // S3-1: Export excludes telemetry + work.episode; only memory.* kinds appear.
    #[test]
    fn export_excludes_local_only_kinds() {
        let conn = make_conn();
        seed_events(&conn);
        let (summary, content) = export_events(&conn, 0, None, false).expect("export");
        let jsonl = content.expect("content must be Some when out_path is None");
        assert!(summary.exported > 0, "must export at least 1 event");
        assert!(
            summary.exported <= 2,
            "only memory.accepted events (2 max); got {}",
            summary.exported
        );
        for line in jsonl.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let se: SyncEvent = serde_json::from_str(line).expect("valid json");
            assert!(
                is_sync_allowed(&se.kind),
                "exported kind {:?} is NOT on the allowlist",
                se.kind
            );
            assert_ne!(
                se.kind, "work.episode",
                "work.episode must never be exported"
            );
            assert_ne!(
                se.kind, "context.served",
                "context.served must never be exported"
            );
            assert_ne!(
                se.kind, "run.started",
                "run metadata must never be exported"
            );
            assert_ne!(
                se.kind, "run.finished",
                "run metadata must never be exported"
            );
        }
    }

    // S3-2: Import is idempotent — re-importing the same batch is a NO-OP.
    #[test]
    fn import_is_idempotent() {
        let conn_a = make_conn();
        seed_events(&conn_a);
        let (_, content) = export_events(&conn_a, 0, None, false).expect("export");
        let jsonl = content.expect("content");

        let conn_b = make_conn();
        let s1 = import_events(&conn_b, &jsonl, false).expect("first import");
        assert!(s1.applied > 0, "first import must apply events");
        assert_eq!(s1.skipped, 0, "first import must have 0 skipped");

        let s2 = import_events(&conn_b, &jsonl, false).expect("second import");
        assert_eq!(s2.applied, 0, "re-import must apply 0 (idempotent)");
        assert_eq!(
            s2.skipped, s1.applied,
            "all events must be skipped on re-import"
        );
    }

    // S3-3: Round-trip — memories exported from brain A appear in brain B.
    #[test]
    fn round_trip_export_import() {
        let conn_a = make_conn();
        let (_, mem_id_a, mem_id_b) = seed_events(&conn_a);
        let (_, content) = export_events(&conn_a, 0, None, false).expect("export");
        let jsonl = content.expect("content");

        let conn_b = make_conn();
        let s = import_events(&conn_b, &jsonl, false).expect("import");
        assert!(s.applied > 0, "must have applied events");

        // Verify memories are projected in brain B.
        let count: i64 = conn_b
            .query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0))
            .expect("count");
        assert!(
            count >= 1,
            "at least one memory must appear in B after import"
        );

        // Both memory ids should exist.
        for mid in [&mem_id_a, &mem_id_b] {
            let exists: i64 = conn_b
                .query_row(
                    "SELECT COUNT(*) FROM memories WHERE memory_id = ?1",
                    rusqlite::params![mid],
                    |r| r.get(0),
                )
                .expect("exists check");
            assert_eq!(exists, 1, "memory {} must exist in B after import", mid);
        }
    }

    // S3-4: Cursor advance — second export only emits the new event.
    #[test]
    fn cursor_advances_correctly() {
        let conn = make_conn();
        seed_events(&conn);
        let (summary1, _) = export_events(&conn, 0, None, false).expect("export 1");
        let cursor_after_first = summary1.next_cursor;

        // Add one more memory.accepted event.
        let run_id = RunId::new();
        let mem_id_c = format!("mem-c-{}", ulid::Ulid::new());
        apply_events(
            &conn,
            &[kimetsu_core::event::Event::new(
                run_id,
                "memory.accepted",
                json!({"memory_id": mem_id_c, "text": "new after cursor", "scope": "project", "kind": "fact"}),
            )],
        )
        .expect("add new event");

        let (summary2, content2) =
            export_events(&conn, cursor_after_first, None, false).expect("export 2");
        let jsonl2 = content2.expect("content");
        assert_eq!(
            summary2.exported, 1,
            "second export must emit exactly 1 new event"
        );
        let se: SyncEvent = serde_json::from_str(jsonl2.trim()).expect("parse");
        let payload_mid = se
            .payload
            .get("memory_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert_eq!(
            payload_mid, mem_id_c,
            "cursor must only export the new event"
        );
    }

    // S3-5: Redaction — secrets in memory.accepted payloads are redacted in export.
    #[test]
    fn export_redacts_secrets() {
        let conn = make_conn();
        let run_id = RunId::new();
        let secret = "sk-ant-api03-AbCdEfGhIjKlMnOpQrStUv0123456789AbCdEf";
        apply_events(
            &conn,
            &[kimetsu_core::event::Event::new(
                run_id,
                "memory.accepted",
                json!({
                    "memory_id": "mem-secret",
                    "text": format!("do not use {secret}"),
                    "scope": "project",
                    "kind": "fact"
                }),
            )],
        )
        .expect("seed");
        let (_, content) = export_events(&conn, 0, None, false).expect("export");
        let jsonl = content.expect("content");
        assert!(
            !jsonl.contains(secret),
            "exported batch must NOT contain the secret"
        );
        assert!(
            jsonl.contains("[REDACTED:anthropic_oauth]"),
            "exported batch must contain the REDACTED placeholder"
        );
    }

    // S3-6: Dry-run on import reports what WOULD apply without writing.
    #[test]
    fn dry_run_import_does_not_write() {
        let conn_a = make_conn();
        seed_events(&conn_a);
        let (_, content) = export_events(&conn_a, 0, None, false).expect("export");
        let jsonl = content.expect("content");

        let conn_b = make_conn();
        let s = import_events(&conn_b, &jsonl, true).expect("dry-run import");
        assert!(s.applied > 0, "dry-run must report events it WOULD apply");

        let count: i64 = conn_b
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .expect("count");
        assert_eq!(count, 0, "dry-run must NOT write any events");
    }

    // S3-7: Directory protocol — push writes a file; pull reads and imports it.
    #[test]
    fn directory_protocol_push_pull() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let sync_dir = tmp.path().join("sync");
        let cursors_path = tmp.path().join("sync-cursors.json");

        // Brain A.
        let conn_a = make_conn();
        seed_events(&conn_a);
        let machine_a = "machine-a";
        let report =
            sync_dir_fn(&conn_a, &sync_dir, machine_a, &cursors_path, false).expect("sync A");
        assert!(report.pushed > 0, "A must push events");

        // Brain B — pull from A.
        let conn_b = make_conn();
        let cursors_b_path = tmp.path().join("cursors-b.json");
        let machine_b = "machine-b";
        let report_b =
            sync_dir_fn(&conn_b, &sync_dir, machine_b, &cursors_b_path, false).expect("sync B");
        assert!(report_b.pulled_applied > 0, "B must import events from A");

        // Idempotent: sync B again — nothing new to apply.
        let report_b2 = sync_dir_fn(&conn_b, &sync_dir, machine_b, &cursors_b_path, false)
            .expect("sync B again");
        assert_eq!(
            report_b2.pulled_applied, 0,
            "second sync B must be idempotent (0 applied)"
        );
    }

    /// Thin wrapper so the test can call the module function by its short name.
    fn sync_dir_fn(
        conn: &Connection,
        sd: &Path,
        mid: &str,
        cp: &Path,
        dry: bool,
    ) -> KimetsuResult<SyncReport> {
        sync_dir(conn, sd, mid, cp, dry)
    }
}
