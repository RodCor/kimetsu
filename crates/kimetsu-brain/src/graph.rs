//! #2 knowledge graph: rule-based relation-edge extraction.
//!
//! Consolidation writes `"supersedes"` edges, but those point at superseded
//! memories retrieval already excludes, so on their own they leave the
//! graph-lite / petgraph backends behaving exactly like flat retrieval. This
//! module derives MEANINGFUL `"relates_to"` edges between *active* memories
//! that share a salient entity, so a query that hits memory A can reach a linked
//! memory B it does not directly match (multi-hop retrieval).
//!
//! The rule layer is fully deterministic and model-free: it parses inline
//! `[tags: ...]` markers (via [`crate::consolidate::parse_tags`]) plus a small
//! salient-term pass, indexes memories by entity, and links every pair that
//! shares at least one entity. The optional LLM enrichment layer (`--enrich`)
//! lives in the CLI, where the cheap-model provider is resolved.
//!
//! ## Batch vs incremental (v3.0)
//!
//! [`build_relates_to_edges`] rebuilds the whole graph and is what
//! `kimetsu brain graph build` runs. Because it only ever ran when a user
//! remembered to invoke it, `memory_edges` in practice held nothing but
//! `supersedes` — so the graph-lite backend behaved like flat retrieval, and
//! the published graph-lite benchmark number described a configuration almost
//! nobody was running.
//!
//! [`incremental_edges_for_memory`] closes that gap: one indexed lookup against
//! the `memory_entities` projection, run on every write, bounded by the same
//! fan-out cap. It asks for [`INCREMENTAL_MIN_SHARED_ENTITIES`] shared entities
//! rather than the batch builder's one, because a single shared word on the
//! write path would attach each new memory to half the corpus.
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

/// Where an extracted entity came from. Author-supplied tags are high-signal;
/// salient terms are the extractor's guess. Ranking treats them differently, so
/// the distinction is persisted rather than recomputed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EntitySource {
    /// From an inline `[tags: …]` marker.
    Tag,
    /// A salient bare token picked out of the prose.
    Term,
}

impl EntitySource {
    pub fn as_str(self) -> &'static str {
        match self {
            EntitySource::Tag => "tag",
            EntitySource::Term => "term",
        }
    }
}

/// [`extract_entities`], keeping track of where each entity came from.
///
/// An entity that appears both as a tag and as a salient term is reported as a
/// tag: the author said it out loud, which outranks the extractor guessing it.
/// Returns sorted, deduplicated pairs.
pub fn extract_entities_with_source(text: &str) -> Vec<(String, EntitySource)> {
    let mut sources: BTreeMap<String, EntitySource> = BTreeMap::new();
    for tag in tag_entities(text) {
        sources.insert(tag, EntitySource::Tag);
    }
    for term in term_entities(text) {
        sources.entry(term).or_insert(EntitySource::Term);
    }
    sources.into_iter().collect()
}

/// Entities contributed by inline `[tags: …]` markers.
fn tag_entities(text: &str) -> BTreeSet<String> {
    let mut set = BTreeSet::new();
    // Tags in this codebase are space-separated inside the block
    // (`[tags: rust mutex ann]`), while parse_tags only splits on commas — so
    // split each returned tag on whitespace to recover individual tag words.
    for t in parse_tags(text) {
        for word in t.split_whitespace() {
            let w = word.trim();
            if w.len() >= 3 {
                set.insert(w.to_string());
            }
        }
    }
    set
}

/// Entities contributed by salient bare tokens in the prose.
fn term_entities(text: &str) -> BTreeSet<String> {
    let mut set = BTreeSet::new();
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
    set
}

/// Extract salient entities/keywords from one memory's text. The result is
/// lowercased and de-duplicated. Two sources:
///   1. inline `[tags: ...]` markers (high-signal, author/distiller supplied),
///   2. salient bare tokens — alphanumeric words of length >= `MIN_KEYWORD_LEN`
///      that are not stopwords (lowercased). Capitalized proper nouns are kept
///      regardless of stopword status (they are distinctive).
///
/// Deterministic and pure — no allocation order dependence (returns sorted).
/// Use [`extract_entities_with_source`] when the tag/term distinction matters.
pub fn extract_entities(text: &str) -> Vec<String> {
    let mut set = tag_entities(text);
    set.extend(term_entities(text));
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

// ── v3.0: the entity projection + incremental edge building ─────────────────

/// Minimum shared entities before two memories are linked on the write path.
///
/// The batch builder links on a single shared entity, which is tolerable when
/// you are rebuilding the whole graph and can inspect the result. On the write
/// path a single shared term is too eager — one common word would attach every
/// new memory to half the corpus — so the incremental path asks for two.
pub const INCREMENTAL_MIN_SHARED_ENTITIES: usize = 2;

/// Replace the entity rows for one memory. Pure projection of `text`, so it is
/// safe to call on every accept, merge and rebuild.
pub fn project_entities(conn: &Connection, memory_id: &str, text: &str) -> KimetsuResult<usize> {
    conn.execute(
        "DELETE FROM memory_entities WHERE memory_id = ?1",
        rusqlite::params![memory_id],
    )?;
    let entities = extract_entities_with_source(text);
    let mut stmt = conn.prepare(
        "INSERT OR REPLACE INTO memory_entities (memory_id, entity, source) VALUES (?1, ?2, ?3)",
    )?;
    for (entity, source) in &entities {
        stmt.execute(rusqlite::params![memory_id, entity, source.as_str()])?;
    }
    Ok(entities.len())
}

/// Drop the entity rows for one memory (used when a memory is invalidated or
/// superseded, so the index does not keep routing traffic to a dead memory).
pub fn forget_entities(conn: &Connection, memory_id: &str) -> KimetsuResult<()> {
    conn.execute(
        "DELETE FROM memory_entities WHERE memory_id = ?1",
        rusqlite::params![memory_id],
    )?;
    Ok(())
}

/// Propose `relates_to` edges from one freshly written memory to the active
/// memories it shares entities with.
///
/// This is the write-path counterpart to [`build_relates_to_edges`]. The batch
/// builder is O(corpus²) in the worst case and only ever ran when someone
/// remembered to invoke `kimetsu brain graph build` — which is why the graph
/// was empty in practice and `graph-lite` retrieval quietly behaved like flat.
/// Here the work is one indexed lookup against `memory_entities`, bounded by
/// `max_fan_out`, cheap enough to run on every write.
///
/// Returns proposals with `src < dst` (matching the batch builder's
/// convention), sorted by descending overlap so the fan-out cap keeps the
/// strongest links.
pub fn incremental_edges_for_memory(
    conn: &Connection,
    memory_id: &str,
    max_fan_out: usize,
) -> KimetsuResult<Vec<EdgeProposal>> {
    let cap = if max_fan_out == 0 {
        DEFAULT_MAX_FAN_OUT
    } else {
        max_fan_out
    };

    // Count shared entities with every other ACTIVE memory, strongest first.
    // Ties break on memory_id so the result is deterministic.
    let mut stmt = conn.prepare(
        "SELECT other.memory_id, COUNT(*) AS shared
         FROM memory_entities AS mine
         JOIN memory_entities AS other
           ON other.entity = mine.entity AND other.memory_id != mine.memory_id
         JOIN memories AS m
           ON m.memory_id = other.memory_id
         WHERE mine.memory_id = ?1
           AND m.invalidated_at IS NULL
           AND m.superseded_by IS NULL
         GROUP BY other.memory_id
         HAVING shared >= ?2
         ORDER BY shared DESC, other.memory_id ASC
         LIMIT ?3",
    )?;
    let neighbours = stmt
        .query_map(
            rusqlite::params![
                memory_id,
                INCREMENTAL_MIN_SHARED_ENTITIES as i64,
                cap as i64
            ],
            |row| row.get::<_, String>(0),
        )?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(neighbours
        .into_iter()
        .map(|other| {
            let (src_id, dst_id) = if memory_id < other.as_str() {
                (memory_id.to_string(), other)
            } else {
                (other, memory_id.to_string())
            };
            EdgeProposal {
                src_id,
                dst_id,
                edge_type: RELATES_TO.to_string(),
            }
        })
        .collect())
}

/// Rebuild `memory_entities` for every active memory. Used by
/// `kimetsu brain rebuild` and by the v11 migration backfill, so an existing
/// brain gets an entity index without waiting for its memories to be rewritten.
pub fn reproject_all_entities(conn: &Connection) -> KimetsuResult<usize> {
    let memories = load_active_memories(conn)?;
    let mut total = 0usize;
    for (id, text) in &memories {
        total += project_entities(conn, id, text)?;
    }
    Ok(total)
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

    // ── v3.0: the entity projection + incremental edges ──────────────────

    #[test]
    fn entity_source_prefers_the_author_supplied_tag() {
        let pairs = extract_entities_with_source("[tags: mutex] Holding a mutex across an await");
        let mutex = pairs
            .iter()
            .find(|(e, _)| e == "mutex")
            .expect("mutex must be extracted");
        assert_eq!(
            mutex.1,
            EntitySource::Tag,
            "an entity that is both tagged and mentioned is a tag: the author said it out loud"
        );
        let holding = pairs.iter().find(|(e, _)| e == "holding");
        assert_eq!(holding.map(|(_, s)| *s), Some(EntitySource::Term));
    }

    #[test]
    fn project_entities_replaces_rather_than_accumulates() {
        let conn = make_conn();
        insert_active_memory(&conn, "a", "[tags: sqlite] vacuum reclaims dead pages");
        project_entities(&conn, "a", "[tags: sqlite] vacuum reclaims dead pages").expect("project");
        let first: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memory_entities WHERE memory_id='a'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(first > 0, "entities must land");

        // Reprojecting a shorter text must not leave the old rows behind.
        project_entities(&conn, "a", "[tags: sqlite]").expect("reproject");
        let entities: Vec<String> = conn
            .prepare("SELECT entity FROM memory_entities WHERE memory_id='a' ORDER BY entity")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(entities, vec!["sqlite".to_string()]);
    }

    /// The regression this whole slice exists for: before v3.0 the only edges
    /// ever written were `supersedes`, so `graph-lite` retrieval quietly
    /// behaved like flat unless someone remembered to run `graph build`.
    #[test]
    fn incremental_edges_link_a_new_memory_to_its_neighbours() {
        let conn = make_conn();
        for (id, text) in [
            (
                "a",
                "[tags: sqlite wal] WAL mode needs a checkpoint before backup",
            ),
            (
                "b",
                "[tags: sqlite wal] Opening a WAL database read-only skips recovery",
            ),
            ("c", "[tags: rust] Prefer thiserror for library error types"),
        ] {
            insert_active_memory(&conn, id, text);
            project_entities(&conn, id, text).expect("project");
        }

        let edges = incremental_edges_for_memory(&conn, "b", 0).expect("incremental");
        let linked: Vec<&str> = edges
            .iter()
            .map(|e| {
                if e.src_id == "b" {
                    e.dst_id.as_str()
                } else {
                    e.src_id.as_str()
                }
            })
            .collect();
        assert_eq!(
            linked,
            vec!["a"],
            "two shared entities (sqlite, wal) links a-b; one topic in common does not link c"
        );
        let edge = &edges[0];
        assert!(edge.src_id < edge.dst_id, "edges are stored src < dst");
        assert_eq!(edge.edge_type, RELATES_TO);
    }

    /// One shared word is not a relationship. On the write path a single
    /// common term would attach every new memory to half the corpus.
    #[test]
    fn incremental_edges_need_more_than_one_shared_entity() {
        let conn = make_conn();
        for (id, text) in [
            ("a", "[tags: sqlite] checkpoint before backup"),
            ("b", "[tags: sqlite] different subject entirely"),
        ] {
            insert_active_memory(&conn, id, text);
            project_entities(&conn, id, text).expect("project");
        }
        // Sanity: they really do share exactly one entity.
        let shared: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memory_entities x JOIN memory_entities y
                   ON x.entity = y.entity AND x.memory_id='a' AND y.memory_id='b'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(shared, 1, "fixture must share exactly one entity");

        let edges = incremental_edges_for_memory(&conn, "b", 0).expect("incremental");
        assert!(
            edges.is_empty(),
            "one shared entity is not enough: {edges:?}"
        );
    }

    #[test]
    fn incremental_edges_skip_inactive_neighbours() {
        let conn = make_conn();
        for (id, text) in [
            ("a", "[tags: sqlite wal] WAL mode needs a checkpoint"),
            ("b", "[tags: sqlite wal] WAL recovery on read-only open"),
        ] {
            insert_active_memory(&conn, id, text);
            project_entities(&conn, id, text).expect("project");
        }
        conn.execute(
            "UPDATE memories SET invalidated_at = '2024-06-01T00:00:00Z' WHERE memory_id = 'a'",
            [],
        )
        .unwrap();
        let edges = incremental_edges_for_memory(&conn, "b", 0).expect("incremental");
        assert!(
            edges.is_empty(),
            "an invalidated memory is not a reachable destination: {edges:?}"
        );
    }

    #[test]
    fn incremental_edges_respect_the_fan_out_cap() {
        let conn = make_conn();
        for i in 0..10 {
            let id = format!("m{i}");
            let text = "[tags: sqlite wal] shared topic";
            insert_active_memory(&conn, &id, text);
            project_entities(&conn, &id, text).expect("project");
        }
        let edges = incremental_edges_for_memory(&conn, "m0", 3).expect("incremental");
        assert_eq!(edges.len(), 3, "fan-out cap must bound the write path");
    }

    #[test]
    fn reproject_all_entities_backfills_an_existing_corpus() {
        let conn = make_conn();
        insert_active_memory(&conn, "a", "[tags: sqlite wal] checkpoint before backup");
        insert_active_memory(&conn, "b", "[tags: sqlite wal] recovery on read-only open");
        // Simulates an upgraded brain: rows exist, index does not.
        let before: i64 = conn
            .query_row("SELECT COUNT(*) FROM memory_entities", [], |r| r.get(0))
            .unwrap();
        assert_eq!(before, 0);

        reproject_all_entities(&conn).expect("backfill");
        let edges = incremental_edges_for_memory(&conn, "b", 0).expect("incremental");
        assert_eq!(
            edges.len(),
            1,
            "backfilled index must be usable immediately"
        );
    }

    /// End-to-end on a real project: recording two related memories through
    /// the ordinary write path must leave a traversable `relates_to` edge
    /// behind, with nobody having run `kimetsu brain graph build`.
    #[test]
    fn recording_memories_links_them_without_a_manual_graph_build() {
        use kimetsu_core::memory::{MemoryKind, MemoryScope};

        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("kimetsu-graph-ingest-{ts}"));
        std::fs::create_dir_all(&dir).expect("create tmp");
        kimetsu_core::paths::git_init_boundary(&dir);

        crate::user_brain::with_user_brain_disabled(|| {
            crate::project::init_project(&dir, true).expect("init");
            crate::project::add_memory(
                &dir,
                MemoryScope::Project,
                MemoryKind::Convention,
                "[tags: sqlite wal] Checkpoint the WAL before copying brain.db",
            )
            .expect("add first");
            crate::project::add_memory(
                &dir,
                MemoryScope::Project,
                MemoryKind::FailurePattern,
                "[tags: sqlite wal] Opening a WAL database read-only skips recovery",
            )
            .expect("add second");

            let paths = kimetsu_core::paths::ProjectPaths::discover(&dir).expect("paths");
            let conn = Connection::open(&paths.brain_db).expect("open brain");
            let relates: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM memory_edges WHERE edge_type = 'relates_to'",
                    [],
                    |r| r.get(0),
                )
                .expect("count edges");
            assert!(
                relates >= 1,
                "the write path must link related memories; got {relates} relates_to edges"
            );

            let entities: i64 = conn
                .query_row("SELECT COUNT(*) FROM memory_entities", [], |r| r.get(0))
                .expect("count entities");
            assert!(entities > 0, "entities must be projected on write");
        });

        std::fs::remove_dir_all(dir).ok();
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
