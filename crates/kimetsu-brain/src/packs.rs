//! Portable brain packs: security-scrubbed export, merge/replace import,
//! and the gzip'd pack envelope. Split out of `project.rs` (v2.5.1); the
//! public API is unchanged — everything here is re-exported by [`crate::project`].

use std::path::Path;

use kimetsu_core::KimetsuResult;
use kimetsu_core::ids::RunId;
use kimetsu_core::memory::{MemoryKind, MemoryScope};
use rusqlite::params;

use crate::project::{add_memory, invalidate_memory, load_project, load_project_readonly};

// ── Q5: portable memory export / import ──────────────────────────────────────

/// A single memory in the portable JSON exchange format.
///
/// Carries only the fields needed to reconstruct the memory in another brain —
/// instance-specific data (`memory_id`, `usefulness_score`, `use_count`) is
/// intentionally excluded so importing always creates a fresh row with clean
/// stats.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MemoryExport {
    pub text: String,
    pub scope: String,
    pub kind: String,
    pub confidence: f32,
    pub created_at: Option<String>,
}

/// v3.0 #4: a shareable brain PACK — a self-describing envelope (manifest +
/// memories) for distribution via the marketplace. Serialized to JSON then
/// gzip-compressed by the CLI. A bare `Vec<MemoryExport>` (the pre-pack export
/// format) also imports, for back-compat — see [`parse_pack_or_array`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Pack {
    /// Pack format version (currently 1).
    pub kimetsu_pack: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exported_at: Option<String>,
    #[serde(default)]
    pub memory_count: usize,
    pub memories: Vec<MemoryExport>,
}

/// Identity of an installed pack, stamped into each imported memory's provenance
/// so it can later be listed / updated / uninstalled.
#[derive(Debug, Clone, Default)]
pub struct PackRef {
    pub name: Option<String>,
    pub version: Option<String>,
}

/// Parse a pack file body: a [`Pack`] envelope OR a bare `Vec<MemoryExport>`
/// (back-compat with pre-pack exports). Returns the manifest [`PackRef`] (empty
/// for a bare array) and the memory entries.
pub fn parse_pack_or_array(json: &str) -> KimetsuResult<(PackRef, Vec<MemoryExport>)> {
    // A Pack is a JSON object with `kimetsu_pack` + `memories`; a bare array is
    // a JSON array. Try the envelope first; fall back to the array.
    if let Ok(pack) = serde_json::from_str::<Pack>(json) {
        return Ok((
            PackRef {
                name: pack.name,
                version: pack.version,
            },
            pack.memories,
        ));
    }
    let entries: Vec<MemoryExport> = serde_json::from_str(json)
        .map_err(|e| format!("pack: not a Pack envelope or a memory array: {e}"))?;
    Ok((PackRef::default(), entries))
}

/// Strip the trailing `(context: …)` segment from a memory text produced by
/// the distiller / `brain record` workflow, leaving only the lesson body.
///
/// Matches the literal pattern ` (context: <anything>)` at the very end of
/// the trimmed string. The match is case-sensitive to avoid false positives.
///
/// Returns the original `text` unchanged when:
///   - the pattern is absent, or
///   - stripping would leave an empty or whitespace-only string (safety
///     fallback: a blank lesson is worse than a slightly noisy one).
///
/// # Examples
/// ```
/// # use kimetsu_brain::project::redact_context_suffix;
/// assert_eq!(
///     redact_context_suffix("always use --locked (context: cargo build)"),
///     "always use --locked"
/// );
/// assert_eq!(
///     redact_context_suffix("bare lesson"),
///     "bare lesson"
/// );
/// ```
pub fn redact_context_suffix(text: &str) -> &str {
    let trimmed = text.trim_end();
    // Pattern: " (context: …)" where the parenthesised segment is at the end.
    // Walk backwards to find the matching open-paren for a ` (context: ` prefix.
    if let Some(pos) = find_trailing_context_paren(trimmed) {
        let candidate = trimmed[..pos].trim_end();
        if !candidate.is_empty() {
            return candidate;
        }
    }
    text
}

/// Strip the leading `[tags: …]` prefix from a memory text, leaving only the
/// lesson body (and any trailing context segment unless that is separately
/// stripped by [`redact_context_suffix`]).
///
/// Matches `[tags: …] ` at the very start of the trimmed string.
/// Returns the original `text` when:
///   - the pattern is absent, or
///   - stripping would leave an empty or whitespace-only string.
///
/// # Examples
/// ```
/// # use kimetsu_brain::project::redact_tags_prefix;
/// assert_eq!(
///     redact_tags_prefix("[tags: rust, cargo] always use --locked"),
///     "always use --locked"
/// );
/// assert_eq!(
///     redact_tags_prefix("no tags here"),
///     "no tags here"
/// );
/// ```
pub fn redact_tags_prefix(text: &str) -> &str {
    let trimmed = text.trim_start();
    if let Some(rest) = trimmed.strip_prefix("[tags: ") {
        if let Some(close) = rest.find(']') {
            let after = rest[close + 1..].trim_start();
            if !after.is_empty() {
                return after;
            }
        }
    }
    text
}

/// Apply export-time redaction to a single `MemoryExport`'s text field
/// according to the requested flags. Returns a new `MemoryExport` with the
/// text replaced (or the original when no patterns match and the safety
/// fallback applies).
///
/// The two-step order matters: strip tags first, then context, so that a
/// memory like `[tags: rust] lesson body (context: foo)` becomes
/// `lesson body` when both flags are active.
pub fn apply_export_redaction(
    entry: MemoryExport,
    redact: bool,
    redact_tags: bool,
) -> MemoryExport {
    if !redact && !redact_tags {
        return entry;
    }
    let mut text: &str = &entry.text;
    // Temporary storage so we can chain borrows without lifetime woes.
    let after_tags: String;
    let after_ctx: String;
    if redact_tags {
        let stripped = redact_tags_prefix(text);
        after_tags = stripped.to_string();
        text = &after_tags;
    }
    if redact {
        let stripped = redact_context_suffix(text);
        after_ctx = stripped.to_string();
        text = &after_ctx;
    }
    MemoryExport {
        text: text.to_string(),
        ..entry
    }
}

// Helper: find the byte offset of the opening ` (context: ` run that closes
// at the very end of `s` (which must already be trimmed of trailing
// whitespace). Returns `None` when no such suffix is present.
fn find_trailing_context_paren(s: &str) -> Option<usize> {
    // We look for a closing `)` at the end, then walk left to find ` (context: `.
    if !s.ends_with(')') {
        return None;
    }
    // The minimum suffix is ` (context: x)` — 13 chars.
    let bytes = s.as_bytes();
    // Find the matching open paren by scanning backwards from the terminal `)`.
    let close = s.len() - 1;
    // We need at least " (context: " before the close paren, so start scanning
    // no further than close - len(" (context: ") = close - 11.
    // Use a simple prefix search scanning from the right.
    let prefix = b" (context: ";
    for start in (0..close).rev() {
        if start + prefix.len() > close {
            continue;
        }
        if &bytes[start..start + prefix.len()] == prefix {
            // Found the open sequence; the segment is s[start..=close].
            return Some(start);
        }
    }
    None
}

/// Summary returned by [`import_memories`] / [`import_pack`].
#[derive(Debug, Clone, Default)]
pub struct ImportSummary {
    /// Memories that were actually written (new rows).
    pub imported: usize,
    /// Entries that were skipped because an identical memory already existed
    /// (detected by `add_memory`'s normalized-text dedup) or because the
    /// scope/kind was malformed.
    pub deduped: usize,
    /// v3.0 #4: memories superseded by a `replace`-mode pack install (existing
    /// active memories in the pack's scope(s), invalidated before the load).
    pub superseded: usize,
}

thread_local! {
    /// v3.0 #4: provenance source stamped onto memories written during a pack
    /// install (e.g. `{source:"pack", pack_name, pack_version}`). When unset,
    /// `add_memory` uses its default `manual_cli` provenance. RAII-scoped by
    /// [`ImportProvenanceScope`] so it never leaks past the import.
    static IMPORT_PROVENANCE: std::cell::RefCell<Option<serde_json::Value>> =
        const { std::cell::RefCell::new(None) };
}

pub(crate) struct ImportProvenanceScope;
impl ImportProvenanceScope {
    pub(crate) fn new(v: serde_json::Value) -> Self {
        IMPORT_PROVENANCE.with(|c| *c.borrow_mut() = Some(v));
        ImportProvenanceScope
    }
}
impl Drop for ImportProvenanceScope {
    fn drop(&mut self) {
        IMPORT_PROVENANCE.with(|c| *c.borrow_mut() = None);
    }
}

/// Build a memory's `provenance_snapshot`. Uses the thread-local pack source
/// (set during a pack install) when present, else the default `manual_cli`.
pub(crate) fn build_provenance(run_id: RunId, text: &str) -> serde_json::Value {
    IMPORT_PROVENANCE.with(|c| {
        if let Some(src) = c.borrow().as_ref() {
            let mut v = src.clone();
            if let Some(obj) = v.as_object_mut() {
                obj.insert("run_id".into(), serde_json::json!(run_id.to_string()));
                obj.insert("text".into(), serde_json::json!(text));
            }
            v
        } else {
            serde_json::json!({
                "source": "manual_cli",
                "run_id": run_id.to_string(),
                "text": text,
            })
        }
    })
}

/// Export active memories as a vec of portable records.
///
/// `scope` and `kind` are optional filters; `None` means "all".
/// `redact` strips the trailing `(context: …)` segment from each text.
/// `redact_tags` additionally strips the leading `[tags: …]` prefix.
/// Aggregate security-scrub findings across an export (no credentials / PII may
/// ship in a shareable pack). `kinds` maps each redaction kind to its count.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ScrubReport {
    pub total: usize,
    pub kinds: std::collections::BTreeMap<String, usize>,
}

impl ScrubReport {
    pub fn is_clean(&self) -> bool {
        self.total == 0
    }
    /// One-liner like `"scrubbed 4: email×2, anthropic_oauth×1, ssn×1"`.
    pub fn summary(&self) -> String {
        if self.total == 0 {
            return "no credentials or PII found".to_string();
        }
        let parts: Vec<String> = self.kinds.iter().map(|(k, n)| format!("{k}×{n}")).collect();
        format!("scrubbed {}: {}", self.total, parts.join(", "))
    }
}

pub fn export_memories(
    start: &Path,
    scope: Option<MemoryScope>,
    kind: Option<MemoryKind>,
    redact: bool,
    redact_tags: bool,
) -> KimetsuResult<(Vec<MemoryExport>, ScrubReport)> {
    // Build the SQL dynamically based on the optional filters, including
    // `created_at` so the JSON record carries the origin timestamp.
    let (sql, params_vec): (&str, Vec<String>) = match (scope.as_ref(), kind.as_ref()) {
        (Some(s), Some(k)) => (
            "SELECT scope, kind, text, confidence, created_at
             FROM memories
             WHERE invalidated_at IS NULL
               AND superseded_by IS NULL
               AND lower(scope) = lower(?1)
               AND lower(kind)  = lower(?2)
             ORDER BY created_at DESC",
            vec![s.to_string(), k.to_string()],
        ),
        (Some(s), None) => (
            "SELECT scope, kind, text, confidence, created_at
             FROM memories
             WHERE invalidated_at IS NULL
               AND superseded_by IS NULL
               AND lower(scope) = lower(?1)
             ORDER BY created_at DESC",
            vec![s.to_string()],
        ),
        (None, Some(k)) => (
            "SELECT scope, kind, text, confidence, created_at
             FROM memories
             WHERE invalidated_at IS NULL
               AND superseded_by IS NULL
               AND lower(kind) = lower(?1)
             ORDER BY created_at DESC",
            vec![k.to_string()],
        ),
        (None, None) => (
            "SELECT scope, kind, text, confidence, created_at
             FROM memories
             WHERE invalidated_at IS NULL
               AND superseded_by IS NULL
             ORDER BY created_at DESC",
            vec![],
        ),
    };

    // Project-level memories only (user brain memories live in a separate DB;
    // callers wanting the user brain should call with scope=GlobalUser on the
    // user-brain path, or simply use list_memories which merges both).
    let (_paths, _config, conn) = load_project(start)?;

    let mut stmt = conn.prepare(sql)?;
    let refs: Vec<&dyn rusqlite::ToSql> = params_vec
        .iter()
        .map(|s| s as &dyn rusqlite::ToSql)
        .collect();
    let rows = stmt.query_map(refs.as_slice(), |row| {
        Ok(MemoryExport {
            scope: row.get(0)?,
            kind: row.get(1)?,
            text: row.get(2)?,
            confidence: row.get::<_, f64>(3)? as f32,
            created_at: row.get(4)?,
        })
    })?;

    // Security scrub (v3.0 #4): every exported memory passes through the
    // credential + PII scrubber so a shareable pack can never ship secrets or
    // personal data. The scrub is on the EXPORT COPY only — the source DB is
    // untouched. Findings are tallied for the caller to report (and --strict).
    let mut out = Vec::new();
    let mut report = ScrubReport::default();
    for row in rows {
        let mut entry = apply_export_redaction(row?, redact, redact_tags);
        let scrubbed = crate::redact::scrub_for_export(&entry.text);
        for m in &scrubbed.matches {
            *report.kinds.entry(m.kind.to_string()).or_insert(0) += 1;
            report.total += 1;
        }
        entry.text = scrubbed.text;
        out.push(entry);
    }
    Ok((out, report))
}

/// Import a slice of [`MemoryExport`] records into the brain at `start`.
///
/// For each entry:
/// - Parse scope + kind from the string fields (with optional `scope_override`).
/// - Call `add_memory`, which dedups by normalized text. Dedup is detected by
///   comparing the set of active memory IDs in the project DB before vs after
///   each `add_memory` call — if the returned ID was already in the DB at
///   the start of this import batch, it counts as deduped.
/// - Malformed entries (bad scope/kind string) are skipped with a warning;
///   they do NOT abort the whole import.
///
/// Returns an [`ImportSummary`] with `imported` (new rows) and `deduped`
/// (entries that collapsed to an existing row or were skipped).
pub fn import_memories(
    start: &Path,
    entries: &[MemoryExport],
    scope_override: Option<MemoryScope>,
) -> KimetsuResult<ImportSummary> {
    let mut summary = ImportSummary::default();

    // Snapshot all active memory IDs before we start importing.  Any ID
    // returned by add_memory that is already in this set is a dedup.
    let pre_existing_ids: std::collections::HashSet<String> = {
        // Open a read-only connection just for the snapshot; avoid holding it
        // across the write calls (each add_memory opens its own connection).
        match load_project_readonly(start) {
            Ok((_paths, _config, conn)) => {
                let mut stmt = conn
                    .prepare("SELECT memory_id FROM memories WHERE invalidated_at IS NULL")
                    .unwrap_or_else(|_| conn.prepare("SELECT memory_id FROM memories").unwrap());
                stmt.query_map([], |row| row.get::<_, String>(0))
                    .map(|rows| rows.filter_map(|r| r.ok()).collect())
                    .unwrap_or_default()
            }
            Err(_) => std::collections::HashSet::new(),
        }
    };

    // Also track IDs minted during THIS batch so we can detect within-batch
    // duplicates (e.g. two identical entries in the import file).
    let mut this_batch_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

    for entry in entries {
        // Resolve scope: prefer override, then parse from the entry.
        let scope = if let Some(ref ov) = scope_override {
            *ov
        } else {
            match entry.scope.parse::<MemoryScope>() {
                Ok(s) => s,
                Err(_) => {
                    eprintln!(
                        "kimetsu-brain import: skipping entry with unknown scope `{}`",
                        entry.scope
                    );
                    summary.deduped += 1;
                    continue;
                }
            }
        };

        // Resolve kind.
        let kind = match entry.kind.parse::<MemoryKind>() {
            Ok(k) => k,
            Err(_) => {
                eprintln!(
                    "kimetsu-brain import: skipping entry with unknown kind `{}`",
                    entry.kind
                );
                summary.deduped += 1;
                continue;
            }
        };

        match add_memory(start, scope, kind, &entry.text) {
            Ok(id) => {
                // Dedup if the ID was present before this import started OR
                // was already seen in this batch (within-batch duplicates).
                if pre_existing_ids.contains(&id) || !this_batch_ids.insert(id) {
                    summary.deduped += 1;
                } else {
                    summary.imported += 1;
                }
            }
            Err(e) => {
                eprintln!("kimetsu-brain import: failed to add memory: {e}");
                summary.deduped += 1;
            }
        }
    }

    Ok(summary)
}

/// v3.0 #4: install a pack's memories. `merge` adds additively (dedup against
/// existing). `replace` first invalidates active memories in the pack's scope(s)
/// — REVERSIBLE (events kept; rows marked invalidated) — then loads the pack.
/// Each installed memory is stamped with the `pack` provenance.
pub fn import_pack(
    start: &Path,
    entries: &[MemoryExport],
    scope_override: Option<MemoryScope>,
    replace: bool,
    pack: Option<&PackRef>,
) -> KimetsuResult<ImportSummary> {
    let mut superseded = 0usize;
    if replace {
        let scopes = pack_target_scopes(entries, scope_override);
        let reason = match pack {
            Some(p) => format!(
                "replaced_by_pack:{}@{}",
                p.name.as_deref().unwrap_or("unknown"),
                p.version.as_deref().unwrap_or("?")
            ),
            None => "replaced_by_import".to_string(),
        };
        for id in active_memory_ids_in_scopes(start, &scopes)? {
            invalidate_memory(start, &id, Some(&reason))?;
            superseded += 1;
        }
    }

    // Defensive scrub: never INGEST a credential/PII from a pack, even if the
    // author bypassed export-time scrubbing. (Export already scrubs; this is
    // belt-and-suspenders on the receiving side.)
    let scrubbed: Vec<MemoryExport> = entries
        .iter()
        .map(|e| {
            let mut e = e.clone();
            e.text = crate::redact::scrub_for_export(&e.text).text;
            e
        })
        .collect();

    // Stamp pack provenance on each installed memory for the duration of the load.
    let _prov = pack.map(|p| {
        ImportProvenanceScope::new(serde_json::json!({
            "source": "pack",
            "pack_name": p.name,
            "pack_version": p.version,
        }))
    });
    let mut summary = import_memories(start, &scrubbed, scope_override)?;
    summary.superseded = superseded;
    Ok(summary)
}

/// Distinct scopes a pack will write to (override wins; else parsed per entry).
fn pack_target_scopes(
    entries: &[MemoryExport],
    scope_override: Option<MemoryScope>,
) -> Vec<MemoryScope> {
    if let Some(ov) = scope_override {
        return vec![ov];
    }
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for e in entries {
        if let Ok(s) = e.scope.parse::<MemoryScope>() {
            if seen.insert(s.to_string()) {
                out.push(s);
            }
        }
    }
    out
}

/// Active (non-invalidated, non-superseded) memory ids in the given scopes.
fn active_memory_ids_in_scopes(start: &Path, scopes: &[MemoryScope]) -> KimetsuResult<Vec<String>> {
    if scopes.is_empty() {
        return Ok(Vec::new());
    }
    let (_p, _c, conn) = load_project_readonly(start)?;
    let mut ids = Vec::new();
    for sc in scopes {
        let mut stmt = conn.prepare(
            "SELECT memory_id FROM memories
             WHERE scope = ?1 AND invalidated_at IS NULL AND superseded_by IS NULL",
        )?;
        let rows = stmt.query_map(params![sc.to_string()], |r| r.get::<_, String>(0))?;
        for r in rows {
            ids.push(r?);
        }
    }
    Ok(ids)
}
