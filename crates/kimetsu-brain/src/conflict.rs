//! v0.5.2: conflict detection at ingest.
//!
//! Two memories that say opposite things ("use thiserror" /
//! "use anyhow") confuse the model when both surface in the same
//! broker bundle. v0.5.0 + v0.5.1 made the brain learn from
//! outcomes; v0.5.2 prevents the brain from accumulating
//! contradictions in the first place.
//!
//! The detector runs at `add_memory` / `add_user_memory` time:
//!   1. Embed the incoming text via the active embedder.
//!   2. Scan all active memories in the same scope, score cosine
//!      against the new vector.
//!   3. Pairs that exceed `DEFAULT_CONFLICT_THRESHOLD` (0.8) AND
//!      whose `normalized_text` differs from the new text get
//!      flagged as a conflict.
//!   4. The match is recorded in `memory_conflicts` (idempotent on
//!      (new_memory_id, existing_memory_id)) and a one-line
//!      warning is printed by the caller.
//!
//! Embedder gating:
//!   * NoopEmbedder → empty result, no DB writes. Lean builds keep
//!     v0.4.x behavior.
//!   * Cross-model rows (embedding_model != active model_id) are
//!     skipped — cosine across models is meaningless. A subsequent
//!     `kimetsu brain reindex` would rehydrate them under the
//!     active model and let the next ingest catch the conflict.
//!
//! Resolution policy:
//!   v0.5.2 surfaces conflicts but does NOT block the write. The
//!   new memory is accepted; the operator reviews open conflicts
//!   via `kimetsu brain memory conflicts` and decides which to
//!   invalidate. Surfacing > blocking: a blocked write loses the
//!   user's intent; a logged write loses nothing because the
//!   operator can always invalidate after the fact.

use kimetsu_core::KimetsuResult;
use kimetsu_core::ids::new_id;
use kimetsu_core::memory::{MemoryScope, normalize_memory_text};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::embeddings::{Embedder, cosine_similarity, decode_embedding};

/// Default cosine-similarity threshold above which two memories
/// (with differing normalized text) are flagged as a potential
/// conflict. 0.8 is BGE-small-en-v1.5's empirical "same concept"
/// floor — tighter than 0.7 (which catches loosely related ideas)
/// and looser than 0.9 (which only fires on near-paraphrases).
pub const DEFAULT_CONFLICT_THRESHOLD: f32 = 0.8;

/// Default number of nearest existing memories to evaluate per
/// ingest. We don't need many — if more than 3 capsules
/// simultaneously cross the threshold, the deeper bug is duplicate
/// concepts in the corpus, not a conflict with this one new write.
pub const DEFAULT_TOP_K: u32 = 3;

/// A single conflict-detection hit. Returned by
/// [`find_potential_conflicts`]; persisted by [`record_conflict`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictHit {
    pub existing_memory_id: String,
    pub existing_kind: String,
    pub existing_text: String,
    pub similarity: f32,
}

/// A persisted conflict row joined with both memories' text for
/// CLI / MCP display. Used by [`list_unresolved_conflicts`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictReport {
    pub conflict_id: String,
    pub new_memory_id: String,
    pub new_text: String,
    pub existing_memory_id: String,
    pub existing_text: String,
    pub scope: String,
    pub kind: String,
    pub similarity: f32,
    pub detected_at: String,
    pub resolved_at: Option<String>,
    pub resolution: Option<String>,
}

/// Scan for memories in `scope` whose embedding is within
/// `threshold` cosine distance of `new_text`'s embedding AND whose
/// normalized text differs. Returns at most `top_k` hits sorted
/// by descending similarity.
///
/// `embedder.is_noop()` short-circuits to an empty vec — lean
/// builds never trigger conflict detection.
///
/// Errors from the embedder are propagated; a real embedder
/// failing on a single text means we don't trust *any* downstream
/// cosine and should let the caller decide whether to fail the
/// ingest or fall through.
pub fn find_potential_conflicts(
    conn: &Connection,
    scope: &MemoryScope,
    new_text: &str,
    embedder: &dyn Embedder,
    top_k: u32,
    threshold: f32,
) -> KimetsuResult<Vec<ConflictHit>> {
    if embedder.is_noop() {
        return Ok(Vec::new());
    }
    let new_vec = embedder
        .embed(new_text)
        .map_err(|e| format!("embedder failed during conflict scan: {e}"))?;
    if new_vec.len() != embedder.dim() {
        return Err(format!(
            "embedder {} returned {} dims, expected {}",
            embedder.model_id(),
            new_vec.len(),
            embedder.dim()
        )
        .into());
    }
    let new_normalized = normalize_memory_text(new_text);
    let scope_label = scope.to_string();
    let active_model = embedder.model_id();

    let mut stmt = conn.prepare(
        "
        SELECT memory_id, kind, text, normalized_text, embedding, embedding_model
        FROM memories
        WHERE scope = ?1
          AND invalidated_at IS NULL
          AND embedding IS NOT NULL
          AND embedding_model = ?2
        ",
    )?;
    let rows = stmt.query_map(params![scope_label, active_model], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Vec<u8>>(4)?,
        ))
    })?;

    let mut hits: Vec<ConflictHit> = Vec::new();
    for row in rows {
        let (existing_id, kind, text, normalized, bytes) = row?;
        // Skip exact-text matches: those are dedup territory, not
        // conflicts. The caller's INSERT path already collapses
        // them by (scope, kind, normalized_text).
        if normalized == new_normalized {
            continue;
        }
        let Ok(existing_vec) = decode_embedding(&bytes, Some(new_vec.len())) else {
            // Corrupted blob — skip without erroring out the whole
            // scan. A reindex will fix the row.
            continue;
        };
        let sim = cosine_similarity(&new_vec, &existing_vec);
        if sim >= threshold {
            hits.push(ConflictHit {
                existing_memory_id: existing_id,
                existing_kind: kind,
                existing_text: text,
                similarity: sim,
            });
        }
    }

    hits.sort_by(|a, b| {
        b.similarity
            .partial_cmp(&a.similarity)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    hits.truncate(top_k as usize);
    Ok(hits)
}

/// Persist a single conflict pair. Idempotent on
/// (new_memory_id, existing_memory_id) via UNIQUE — a re-scan of
/// the same ingest won't double-write rows. Returns the
/// conflict_id (freshly minted or existing) so the caller can
/// chain follow-ups.
pub fn record_conflict(
    conn: &Connection,
    new_memory_id: &str,
    scope: &MemoryScope,
    kind: &str,
    hit: &ConflictHit,
) -> KimetsuResult<String> {
    // If a row for this pair already exists, return its id.
    let existing: Option<String> = conn
        .query_row(
            "
            SELECT conflict_id
            FROM memory_conflicts
            WHERE new_memory_id = ?1 AND existing_memory_id = ?2
            ",
            params![new_memory_id, hit.existing_memory_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if let Some(id) = existing {
        return Ok(id);
    }
    let conflict_id = new_id().to_string();
    let detected_at = OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .map_err(|e| format!("timestamp format: {e}"))?;
    conn.execute(
        "
        INSERT INTO memory_conflicts (
            conflict_id, new_memory_id, existing_memory_id,
            scope, kind, similarity, detected_at
        )
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
        ",
        params![
            conflict_id,
            new_memory_id,
            hit.existing_memory_id,
            scope.to_string(),
            kind,
            hit.similarity as f64,
            detected_at,
        ],
    )?;
    Ok(conflict_id)
}

/// Convenience wrapper used by `add_memory` / `add_user_memory`:
/// run detection, persist each hit, return the number of recorded
/// conflicts so the caller can decide whether to surface a
/// warning to stderr.
///
/// Best-effort: an error inside the scan is downgraded to "no
/// conflicts detected this round" + a stderr line, because we
/// never want conflict detection to fail an otherwise-valid memory
/// write.
pub fn detect_and_record(
    conn: &Connection,
    new_memory_id: &str,
    scope: &MemoryScope,
    kind: &str,
    text: &str,
    embedder: &dyn Embedder,
) -> usize {
    let hits = match find_potential_conflicts(
        conn,
        scope,
        text,
        embedder,
        DEFAULT_TOP_K,
        DEFAULT_CONFLICT_THRESHOLD,
    ) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("kimetsu-brain: conflict scan skipped: {e}");
            return 0;
        }
    };
    let mut recorded = 0usize;
    for hit in &hits {
        match record_conflict(conn, new_memory_id, scope, kind, hit) {
            Ok(_) => recorded += 1,
            Err(e) => {
                eprintln!(
                    "kimetsu-brain: failed to record conflict {} <-> {}: {e}",
                    new_memory_id, hit.existing_memory_id
                );
            }
        }
    }
    recorded
}

/// List open (unresolved) conflicts ordered by most recent first,
/// joined with both memories' text so the CLI can render rich
/// rows without a second query round-trip. `limit` is applied
/// after sorting; pass a generous default at the call site
/// (e.g. 50) since conflicts are sparse by construction.
pub fn list_unresolved_conflicts(
    conn: &Connection,
    limit: u32,
) -> KimetsuResult<Vec<ConflictReport>> {
    let mut stmt = conn.prepare(
        "
        SELECT c.conflict_id, c.new_memory_id, mn.text, c.existing_memory_id,
               me.text, c.scope, c.kind, c.similarity, c.detected_at,
               c.resolved_at, c.resolution
        FROM memory_conflicts c
        LEFT JOIN memories mn ON mn.memory_id = c.new_memory_id
        LEFT JOIN memories me ON me.memory_id = c.existing_memory_id
        WHERE c.resolved_at IS NULL
        ORDER BY c.detected_at DESC
        LIMIT ?1
        ",
    )?;
    let rows = stmt.query_map(params![limit], |row| {
        Ok(ConflictReport {
            conflict_id: row.get(0)?,
            new_memory_id: row.get(1)?,
            new_text: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
            existing_memory_id: row.get(3)?,
            existing_text: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
            scope: row.get(5)?,
            kind: row.get(6)?,
            similarity: row.get::<_, f64>(7)? as f32,
            detected_at: row.get(8)?,
            resolved_at: row.get(9)?,
            resolution: row.get(10)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Mark a conflict as resolved with one of `'kept_new'`,
/// `'kept_existing'`, or `'kept_both'`. Returns true if a row was
/// updated (i.e. the id exists and was previously unresolved).
///
/// Side effect: when `resolution = 'kept_new'` the existing
/// memory is invalidated (resolution "I chose the new write");
/// `'kept_existing'` invalidates the new memory; `'kept_both'`
/// invalidates neither. Either invalidation is idempotent —
/// re-applying the same resolution is a no-op on the memory rows.
pub fn resolve_conflict(
    conn: &Connection,
    conflict_id: &str,
    resolution: &str,
) -> KimetsuResult<bool> {
    let resolution = resolution.trim();
    if !matches!(resolution, "kept_new" | "kept_existing" | "kept_both") {
        return Err(format!(
            "invalid conflict resolution {resolution:?}; expected kept_new | kept_existing | kept_both"
        )
        .into());
    }
    // Pull the pair so we know which (if any) memory to invalidate.
    let pair: Option<(String, String)> = conn
        .query_row(
            "
            SELECT new_memory_id, existing_memory_id
            FROM memory_conflicts
            WHERE conflict_id = ?1 AND resolved_at IS NULL
            ",
            params![conflict_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    let Some((new_memory_id, existing_memory_id)) = pair else {
        return Ok(false);
    };

    let now = OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .map_err(|e| format!("timestamp format: {e}"))?;

    // Invalidate the losing side, if any. We do this BEFORE marking
    // the conflict resolved so a crash mid-resolve leaves the row
    // still actionable for the operator.
    let invalidation_reason = format!("v0.5.2 conflict {conflict_id} resolved as {resolution}");
    if resolution == "kept_new" {
        conn.execute(
            "
            UPDATE memories
            SET invalidated_at = COALESCE(invalidated_at, ?2),
                invalidated_reason = COALESCE(invalidated_reason, ?3)
            WHERE memory_id = ?1
            ",
            params![existing_memory_id, now, invalidation_reason],
        )?;
    } else if resolution == "kept_existing" {
        conn.execute(
            "
            UPDATE memories
            SET invalidated_at = COALESCE(invalidated_at, ?2),
                invalidated_reason = COALESCE(invalidated_reason, ?3)
            WHERE memory_id = ?1
            ",
            params![new_memory_id, now, invalidation_reason],
        )?;
    }

    let updated = conn.execute(
        "
        UPDATE memory_conflicts
        SET resolved_at = ?2, resolution = ?3
        WHERE conflict_id = ?1 AND resolved_at IS NULL
        ",
        params![conflict_id, now, resolution],
    )?;
    Ok(updated > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embeddings::{NoopEmbedder, StubEmbedder, encode_embedding};
    use kimetsu_core::memory::normalize_memory_text;
    use rusqlite::Connection;

    fn open_test_brain() -> Connection {
        let conn = Connection::open_in_memory().expect("open in-memory");
        crate::schema::initialize(&conn).expect("init schema");
        conn
    }

    fn insert_memory(
        conn: &Connection,
        memory_id: &str,
        scope: &str,
        kind: &str,
        text: &str,
        embedder: &dyn Embedder,
    ) {
        let normalized = normalize_memory_text(text);
        let vec = embedder.embed(text).expect("embed test row");
        let blob = encode_embedding(&vec);
        conn.execute(
            "
            INSERT INTO memories (
                memory_id, scope, kind, text, normalized_text, confidence,
                source_event_id, provenance_snapshot_json, created_at,
                use_count, usefulness_score, embedding, embedding_model
            )
            VALUES (?1, ?2, ?3, ?4, ?5, 1.0, NULL, '{}',
                    '2026-01-01T00:00:00Z', 0, 0.0, ?6, ?7)
            ",
            params![
                memory_id,
                scope,
                kind,
                text,
                normalized,
                blob,
                embedder.model_id(),
            ],
        )
        .expect("insert");
        conn.execute(
            "INSERT INTO memories_fts (memory_id, text, kind, scope)
             VALUES (?1, ?2, ?3, ?4)",
            params![memory_id, text, kind, scope],
        )
        .expect("fts");
    }

    /// v0.5.2: NoopEmbedder MUST short-circuit to zero hits. Lean
    /// builds without --features embeddings keep v0.4.x behavior.
    #[test]
    fn noop_embedder_returns_no_conflicts() {
        let conn = open_test_brain();
        // Insert via stub so the row has an embedding; then scan with Noop.
        let stub = StubEmbedder::new();
        insert_memory(&conn, "m_existing", "global_user", "fact", "use thiserror for libraries", &stub);
        let hits = find_potential_conflicts(
            &conn,
            &MemoryScope::GlobalUser,
            "use anyhow for libraries",
            &NoopEmbedder,
            DEFAULT_TOP_K,
            DEFAULT_CONFLICT_THRESHOLD,
        )
        .expect("scan");
        assert!(hits.is_empty(), "noop embedder should produce no hits");
    }

    /// v0.5.2: cross-model rows are skipped (cosine across models is
    /// meaningless). Critical for safety mid-reindex when some rows
    /// carry the old model id.
    #[test]
    fn cross_model_rows_are_skipped() {
        let conn = open_test_brain();
        let stub = StubEmbedder::new();
        insert_memory(&conn, "m_xmodel", "global_user", "fact", "use thiserror", &stub);
        // Stomp the model id to simulate a pre-reindex row.
        conn.execute(
            "UPDATE memories SET embedding_model = 'bge-small-en-v1.5' WHERE memory_id = 'm_xmodel'",
            [],
        )
        .expect("force mismatch");
        let hits = find_potential_conflicts(
            &conn,
            &MemoryScope::GlobalUser,
            "use thiserror everywhere", // very similar text
            &stub,
            DEFAULT_TOP_K,
            // Threshold low enough that the StubEmbedder would normally hit it.
            0.0,
        )
        .expect("scan");
        assert!(
            hits.is_empty(),
            "cross-model rows must be skipped from conflict scan"
        );
    }

    /// v0.5.2: identical normalized text is dedup territory, not a
    /// conflict. The scanner must filter exact matches out so a
    /// re-add doesn't generate a self-conflict.
    #[test]
    fn exact_match_is_not_flagged_as_conflict() {
        let conn = open_test_brain();
        let stub = StubEmbedder::new();
        insert_memory(&conn, "m_exact", "global_user", "fact", "Use ripgrep", &stub);
        let hits = find_potential_conflicts(
            &conn,
            &MemoryScope::GlobalUser,
            // Same after normalization.
            "use ripgrep",
            &stub,
            DEFAULT_TOP_K,
            0.0, // even at zero threshold, exact-text should be filtered
        )
        .expect("scan");
        assert!(
            hits.is_empty(),
            "exact normalized-text match should be dedup, not conflict"
        );
    }

    /// v0.5.2: a memory with text similar (high cosine) but
    /// different (post-normalization) gets flagged. Uses
    /// StubEmbedder where identical-token-bag inputs cosine to 1.0
    /// — we exploit that to construct a "shared concept, different
    /// wording" pair.
    #[test]
    fn similar_but_different_text_is_flagged() {
        let conn = open_test_brain();
        let stub = StubEmbedder::new();
        // StubEmbedder cosine is driven by tokenized hash buckets.
        // Two strings sharing 3 distinctive tokens out of 4 will
        // score very high cosine while normalizing differently.
        insert_memory(
            &conn,
            "m_existing",
            "global_user",
            "fact",
            "alpha beta gamma delta",
            &stub,
        );
        let hits = find_potential_conflicts(
            &conn,
            &MemoryScope::GlobalUser,
            "alpha beta gamma omega", // 3/4 shared tokens → high cosine
            &stub,
            DEFAULT_TOP_K,
            // Use a permissive threshold; the StubEmbedder cosine is
            // architecture-dependent so we want the test to fire on
            // the substantive overlap, not the exact 0.8.
            0.4,
        )
        .expect("scan");
        assert!(
            !hits.is_empty(),
            "high-cosine + different-normalized text should flag a conflict"
        );
        assert_eq!(hits[0].existing_memory_id, "m_existing");
        assert!(
            hits[0].similarity >= 0.4,
            "similarity should be >= threshold; got {}",
            hits[0].similarity
        );
    }

    /// v0.5.2: record_conflict is idempotent on
    /// (new_memory_id, existing_memory_id) — re-recording the same
    /// pair returns the original conflict_id instead of duplicating.
    #[test]
    fn record_conflict_is_idempotent() {
        let conn = open_test_brain();
        // Seed two memories so the FK-style assumption (memory rows
        // exist) holds for any downstream join.
        let stub = StubEmbedder::new();
        insert_memory(&conn, "m_new", "global_user", "fact", "alpha", &stub);
        insert_memory(&conn, "m_old", "global_user", "fact", "beta", &stub);
        let hit = ConflictHit {
            existing_memory_id: "m_old".to_string(),
            existing_kind: "fact".to_string(),
            existing_text: "beta".to_string(),
            similarity: 0.85,
        };
        let id1 = record_conflict(&conn, "m_new", &MemoryScope::GlobalUser, "fact", &hit)
            .expect("record 1");
        let id2 = record_conflict(&conn, "m_new", &MemoryScope::GlobalUser, "fact", &hit)
            .expect("record 2");
        assert_eq!(id1, id2, "re-recording the same pair must return same id");
        // Confirm only one row landed.
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM memory_conflicts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    /// v0.5.2: list_unresolved_conflicts joins memory text and
    /// returns rows ordered by detected_at DESC. Resolved rows are
    /// excluded.
    #[test]
    fn list_unresolved_excludes_resolved_rows() {
        let conn = open_test_brain();
        let stub = StubEmbedder::new();
        insert_memory(&conn, "m_new1", "global_user", "fact", "use thiserror", &stub);
        insert_memory(&conn, "m_old1", "global_user", "fact", "use anyhow", &stub);
        insert_memory(&conn, "m_new2", "global_user", "fact", "tabs over spaces", &stub);
        insert_memory(&conn, "m_old2", "global_user", "fact", "spaces over tabs", &stub);

        let hit1 = ConflictHit {
            existing_memory_id: "m_old1".to_string(),
            existing_kind: "fact".to_string(),
            existing_text: "use anyhow".to_string(),
            similarity: 0.9,
        };
        let hit2 = ConflictHit {
            existing_memory_id: "m_old2".to_string(),
            existing_kind: "fact".to_string(),
            existing_text: "spaces over tabs".to_string(),
            similarity: 0.85,
        };
        let cid1 =
            record_conflict(&conn, "m_new1", &MemoryScope::GlobalUser, "fact", &hit1).unwrap();
        let _cid2 =
            record_conflict(&conn, "m_new2", &MemoryScope::GlobalUser, "fact", &hit2).unwrap();

        // Resolve the first conflict (kept_both — neither
        // invalidated); both should still be visible only via the
        // second listing.
        assert!(resolve_conflict(&conn, &cid1, "kept_both").unwrap());

        let open = list_unresolved_conflicts(&conn, 50).unwrap();
        assert_eq!(open.len(), 1, "only the unresolved conflict should list");
        assert_eq!(open[0].new_memory_id, "m_new2");
        assert_eq!(open[0].existing_memory_id, "m_old2");
        assert_eq!(open[0].new_text, "tabs over spaces");
        assert_eq!(open[0].existing_text, "spaces over tabs");
    }

    /// v0.5.2: resolve_conflict with `kept_new` invalidates the
    /// existing memory; `kept_existing` invalidates the new one;
    /// `kept_both` leaves both active.
    #[test]
    fn resolve_conflict_invalidates_loser_side() {
        let conn = open_test_brain();
        let stub = StubEmbedder::new();
        for (mid, text) in [
            ("m_keep_new", "alpha"),
            ("m_old_loses", "beta"),
            ("m_new_loses", "gamma"),
            ("m_keep_existing", "delta"),
            ("m_both_a", "epsilon"),
            ("m_both_b", "zeta"),
        ] {
            insert_memory(&conn, mid, "global_user", "fact", text, &stub);
        }
        let mk_hit = |old: &str| ConflictHit {
            existing_memory_id: old.to_string(),
            existing_kind: "fact".to_string(),
            existing_text: "x".to_string(),
            similarity: 0.9,
        };

        let c_kept_new = record_conflict(
            &conn,
            "m_keep_new",
            &MemoryScope::GlobalUser,
            "fact",
            &mk_hit("m_old_loses"),
        )
        .unwrap();
        let c_kept_existing = record_conflict(
            &conn,
            "m_new_loses",
            &MemoryScope::GlobalUser,
            "fact",
            &mk_hit("m_keep_existing"),
        )
        .unwrap();
        let c_both = record_conflict(
            &conn,
            "m_both_a",
            &MemoryScope::GlobalUser,
            "fact",
            &mk_hit("m_both_b"),
        )
        .unwrap();

        assert!(resolve_conflict(&conn, &c_kept_new, "kept_new").unwrap());
        assert!(resolve_conflict(&conn, &c_kept_existing, "kept_existing").unwrap());
        assert!(resolve_conflict(&conn, &c_both, "kept_both").unwrap());

        let invalidated_at: Vec<(String, Option<String>)> = {
            let mut stmt = conn
                .prepare("SELECT memory_id, invalidated_at FROM memories ORDER BY memory_id")
                .unwrap();
            stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
        };

        let map: std::collections::HashMap<_, _> = invalidated_at.into_iter().collect();
        // kept_new → existing invalidated
        assert!(map["m_keep_new"].is_none(), "winner should stay active");
        assert!(
            map["m_old_loses"].is_some(),
            "kept_new must invalidate the existing memory"
        );
        // kept_existing → new invalidated
        assert!(
            map["m_keep_existing"].is_none(),
            "winner (existing) should stay active"
        );
        assert!(
            map["m_new_loses"].is_some(),
            "kept_existing must invalidate the new memory"
        );
        // kept_both → neither invalidated
        assert!(
            map["m_both_a"].is_none() && map["m_both_b"].is_none(),
            "kept_both should leave both memories active"
        );
    }

    /// v0.5.2: re-resolving the same conflict is a no-op (returns
    /// false on the second call) and does NOT re-stamp
    /// `invalidated_at`. Critical so an operator can't accidentally
    /// rewrite history by re-running `resolve`.
    #[test]
    fn resolve_conflict_is_idempotent() {
        let conn = open_test_brain();
        let stub = StubEmbedder::new();
        insert_memory(&conn, "m_new", "global_user", "fact", "x", &stub);
        insert_memory(&conn, "m_old", "global_user", "fact", "y", &stub);
        let hit = ConflictHit {
            existing_memory_id: "m_old".to_string(),
            existing_kind: "fact".to_string(),
            existing_text: "y".to_string(),
            similarity: 0.95,
        };
        let cid = record_conflict(&conn, "m_new", &MemoryScope::GlobalUser, "fact", &hit).unwrap();
        assert!(resolve_conflict(&conn, &cid, "kept_new").unwrap());
        assert!(
            !resolve_conflict(&conn, &cid, "kept_existing").unwrap(),
            "second resolve must return false (already resolved)"
        );
    }

    /// v0.5.2: detect_and_record returns 0 + writes nothing under
    /// NoopEmbedder. End-to-end version of the noop-skip rule.
    #[test]
    fn detect_and_record_noop_writes_nothing() {
        let conn = open_test_brain();
        let stub = StubEmbedder::new();
        insert_memory(&conn, "m_existing", "global_user", "fact", "alpha beta", &stub);
        insert_memory(&conn, "m_new", "global_user", "fact", "alpha gamma", &stub);
        let recorded = detect_and_record(
            &conn,
            "m_new",
            &MemoryScope::GlobalUser,
            "fact",
            "alpha gamma",
            &NoopEmbedder,
        );
        assert_eq!(recorded, 0);
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM memory_conflicts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    /// v0.5.2: invalid resolution strings are rejected before any
    /// DB write happens. Belt-and-suspenders so a typo from the CLI
    /// doesn't silently mark a conflict as "resolved" with garbage.
    #[test]
    fn resolve_conflict_rejects_invalid_resolution_strings() {
        let conn = open_test_brain();
        let err = resolve_conflict(&conn, "ignored", "delete_them_all").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("invalid conflict resolution"), "got: {msg}");
    }
}
