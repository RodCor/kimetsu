//! #2 knowledge graph: rule-based relation-edge extraction.
//!
//! Today the only edges in `memory_edges` are `"supersedes"` (written by
//! consolidation), and those point at superseded memories that retrieval already
//! excludes — so the graph-lite / petgraph backends behave like flat retrieval.
//! This module derives MEANINGFUL `"relates_to"` edges between *active* memories
//! that share a salient entity, so a query that hits memory A can reach a linked
//! memory B it does not directly match (multi-hop retrieval).
//!
//! The rule layer is fully deterministic and model-free: it parses inline
//! `[tags: ...]` markers (via [`crate::consolidate::parse_tags`]) plus a small
//! salient-term pass, indexes memories by entity, and links every pair that
//! shares at least one entity. The optional LLM enrichment layer (`--enrich`)
//! lives in the CLI, where the cheap-model provider is resolved.
//!
//! Edges are persisted as `memory.edge` events via
//! [`crate::projector::add_memory_edges`], so they are rebuild-safe.

use std::collections::{BTreeMap, BTreeSet};

use kimetsu_core::KimetsuResult;
use rusqlite::Connection;

use crate::consolidate::parse_tags;

/// A proposed relation edge between two active memories.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct EdgeProposal {
    pub src_id: String,
    pub dst_id: String,
    pub edge_type: String,
}

/// The rule-layer edge type.
pub const RELATES_TO: &str = "relates_to";

/// Default cap on how many edges any single memory may originate, to stop a
/// common entity (shared by many memories) from producing a quadratic hairball.
pub const DEFAULT_MAX_FAN_OUT: usize = 8;

/// Minimum length for a salient bare keyword to count as an entity. Short tokens
/// ("the", "a", "is") carry no linking signal.
const MIN_KEYWORD_LEN: usize = 5;

/// A small stop-list of common-but-uninformative long-ish words that would
/// otherwise link unrelated memories. Kept deliberately tiny and lowercase.
const STOPWORDS: &[&str] = &[
    "about", "above", "after", "again", "against", "always", "because", "before", "being", "below",
    "between", "could", "default", "during", "every", "first", "found", "their", "there", "these",
    "thing", "things", "those", "through", "under", "until", "using", "value", "where", "which",
    "while", "would", "should", "while",
];

/// Extract salient entities/keywords from one memory's text. The result is
/// lowercased and de-duplicated. Two sources:
///   1. inline `[tags: ...]` markers (high-signal, author/distiller supplied),
///   2. salient bare tokens — alphanumeric words of length >= `MIN_KEYWORD_LEN`
///      that are not stopwords (lowercased). Capitalized proper nouns are kept
///      regardless of stopword status (they are distinctive).
///
/// Deterministic and pure — no allocation order dependence (returns sorted).
pub fn extract_entities(text: &str) -> Vec<String> {
    let mut set: BTreeSet<String> = BTreeSet::new();

    // 1. Inline tags (already lowercased + deduped by parse_tags). Tags in this
    //    codebase are space-separated inside the block (`[tags: rust mutex ann]`),
    //    while parse_tags only splits on commas — so split each returned tag on
    //    whitespace to recover individual high-signal tag words.
    for t in parse_tags(text) {
        for word in t.split_whitespace() {
            let w = word.trim();
            if w.len() >= 3 {
                set.insert(w.to_string());
            }
        }
    }

    // 2. Salient bare tokens. Split on non-alphanumeric; keep informative ones.
    for raw in text.split(|c: char| !c.is_alphanumeric()) {
        if raw.is_empty() {
            continue;
        }
        let is_proper = raw.chars().next().is_some_and(|c| c.is_uppercase())
            && raw.chars().skip(1).any(|c| c.is_lowercase());
        let lower = raw.to_ascii_lowercase();
        // Distinctive proper noun (kept even if short), OR an informative long
        // token that is not a stopword.
        let proper_kept = is_proper && lower.len() >= 3;
        let informative = lower.len() >= MIN_KEYWORD_LEN
            && !STOPWORDS.contains(&lower.as_str())
            && lower.chars().any(|c| c.is_alphabetic());
        if proper_kept || informative {
            set.insert(lower);
        }
    }

    set.into_iter().collect()
}

/// Load every active (not invalidated, not superseded) memory as `(id, text)`,
/// ordered by id for deterministic edge generation.
fn load_active_memories(conn: &Connection) -> KimetsuResult<Vec<(String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT memory_id, text
         FROM memories
         WHERE invalidated_at IS NULL AND superseded_by IS NULL
         ORDER BY memory_id",
    )?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Build rule-based `relates_to` edge proposals over all active memories: any two
/// memories sharing >= 1 extracted entity are linked. Edges are undirected in
/// meaning but stored once as `src < dst` (graph-lite traverses both directions),
/// so each related pair yields exactly one proposal. `max_fan_out` caps the
/// number of edges per source memory (0 = use [`DEFAULT_MAX_FAN_OUT`]).
///
/// Returns proposals sorted and de-duplicated; deterministic for a given brain
/// state. Pure read — does not write anything (the caller persists via
/// [`crate::projector::add_memory_edges`]).
pub fn build_relates_to_edges(
    conn: &Connection,
    max_fan_out: usize,
) -> KimetsuResult<Vec<EdgeProposal>> {
    let cap = if max_fan_out == 0 {
        DEFAULT_MAX_FAN_OUT
    } else {
        max_fan_out
    };
    let memories = load_active_memories(conn)?;

    // entity -> sorted list of memory ids that mention it.
    let mut by_entity: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (id, text) in &memories {
        for entity in extract_entities(text) {
            by_entity.entry(entity).or_default().push(id.clone());
        }
    }

    // Collect undirected pairs (a < b) that co-mention any entity.
    let mut pairs: BTreeSet<(String, String)> = BTreeSet::new();
    for ids in by_entity.values() {
        // Skip ubiquitous entities: if a single entity is shared by a large
        // fraction of memories it is noise, not signal. Cap the group size.
        if ids.len() < 2 || ids.len() > cap.max(2) * 4 {
            continue;
        }
        for i in 0..ids.len() {
            for j in (i + 1)..ids.len() {
                let (a, b) = if ids[i] < ids[j] {
                    (ids[i].clone(), ids[j].clone())
                } else if ids[i] > ids[j] {
                    (ids[j].clone(), ids[i].clone())
                } else {
                    continue; // same id under one entity (shouldn't happen)
                };
                pairs.insert((a, b));
            }
        }
    }

    // Enforce per-source fan-out cap deterministically (pairs are already sorted).
    let mut fan_out: BTreeMap<String, usize> = BTreeMap::new();
    let mut proposals: Vec<EdgeProposal> = Vec::new();
    for (a, b) in pairs {
        let ca = fan_out.entry(a.clone()).or_insert(0);
        if *ca >= cap {
            continue;
        }
        *ca += 1;
        proposals.push(EdgeProposal {
            src_id: a,
            dst_id: b,
            edge_type: RELATES_TO.to_string(),
        });
    }
    Ok(proposals)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projector::add_memory_edges;
    use crate::schema;
    use rusqlite::params;

    fn make_conn() -> Connection {
        let conn = Connection::open_in_memory().expect("open_in_memory");
        schema::initialize(&conn).expect("schema::initialize");
        conn
    }

    fn insert_active_memory(conn: &Connection, id: &str, text: &str) {
        conn.execute(
            "INSERT INTO memories
             (memory_id, scope, kind, text, normalized_text, confidence, provenance_snapshot_json, created_at)
             VALUES (?1, 'global_user', 'fact', ?2, ?2, 0.85, '{}', '2024-01-01T00:00:00Z')",
            params![id, text],
        )
        .expect("insert memory");
    }

    #[test]
    fn extract_entities_picks_tags_and_salient_terms() {
        let ents = extract_entities("[tags: rust mutex] Holding a Mutex across an await deadlocks");
        // Inline tags present.
        assert!(ents.contains(&"rust".to_string()));
        assert!(ents.contains(&"mutex".to_string()));
        // Salient long token kept; short stopword-ish dropped.
        assert!(ents.contains(&"deadlocks".to_string()));
        assert!(!ents.contains(&"a".to_string()));
        assert!(!ents.contains(&"an".to_string()));
    }

    #[test]
    fn extract_entities_is_sorted_and_deduped() {
        let ents = extract_entities("Docker docker DOCKER mount mount");
        let mut sorted = ents.clone();
        sorted.sort();
        assert_eq!(ents, sorted, "entities must be returned sorted");
        let set: BTreeSet<&String> = ents.iter().collect();
        assert_eq!(set.len(), ents.len(), "no duplicates");
    }

    #[test]
    fn build_edges_links_shared_entity_and_skips_unrelated() {
        let conn = make_conn();
        // a & b share "deadlock"; c is unrelated.
        insert_active_memory(
            &conn,
            "a",
            "[tags: deadlock] holding a mutex guard deadlock risk",
        );
        insert_active_memory(
            &conn,
            "b",
            "the async runtime can deadlock under contention",
        );
        insert_active_memory(
            &conn,
            "c",
            "the website landing page uses a teal gradient hero",
        );

        let edges = build_relates_to_edges(&conn, 0).expect("build");
        // Exactly one undirected pair (a,b), stored as src<dst.
        assert_eq!(edges.len(), 1, "got {edges:?}");
        assert_eq!(edges[0].src_id, "a");
        assert_eq!(edges[0].dst_id, "b");
        assert_eq!(edges[0].edge_type, RELATES_TO);
    }

    #[test]
    fn build_edges_persist_roundtrip() {
        let conn = make_conn();
        insert_active_memory(&conn, "a", "windows docker named pipe mount rule");
        insert_active_memory(&conn, "b", "docker mount breaks under a tcp host");

        let edges = build_relates_to_edges(&conn, 0).expect("build");
        assert!(!edges.is_empty());
        let tuples: Vec<(String, String, String)> = edges
            .iter()
            .map(|e| (e.src_id.clone(), e.dst_id.clone(), e.edge_type.clone()))
            .collect();
        let written = add_memory_edges(&conn, &tuples).expect("persist");
        assert_eq!(written, edges.len());

        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memory_edges WHERE edge_type='relates_to'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n as usize, edges.len());
    }

    #[test]
    fn build_edges_excludes_superseded() {
        let conn = make_conn();
        insert_active_memory(&conn, "a", "shared topic alpha beta gamma");
        insert_active_memory(&conn, "b", "shared topic alpha beta gamma too");
        // Supersede b: it must drop out of the active set, leaving no pair.
        conn.execute(
            "UPDATE memories SET superseded_by = 'a' WHERE memory_id = 'b'",
            [],
        )
        .unwrap();
        let edges = build_relates_to_edges(&conn, 0).expect("build");
        assert!(edges.is_empty(), "superseded memory must not be linked");
    }
}
