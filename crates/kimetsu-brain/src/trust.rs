//! v3.0: how much a memory's origin is worth.
//!
//! Every memory in the brain has been treated as equally believable regardless
//! of where it came from — a lesson you typed yourself, one a model distilled
//! from a transcript, and one that arrived in a pack downloaded from a URL all
//! rank on relevance and usefulness alone.
//!
//! That is the shape of a real attack. Memory poisoning (OWASP ASI06) differs
//! from prompt injection in exactly the way that matters here: prompt injection
//! is session-scoped and resets, while a poisoned memory persists and
//! influences every future session until someone notices. MINJA
//! (arXiv 2601.05504) reports >95% injection success against memory-backed
//! agents through ordinary, unprivileged interaction — no elevated access, just
//! conversation that induces the agent to write something.
//!
//! Kimetsu's exposure is narrower than a hosted service's — the brain is a
//! local file — but it is not zero, and it grows with exactly the features that
//! make the product good: `brain import` from a URL, `brain sync` across
//! machines, and Kimetsu Remote's shared org brain.
//!
//! ## What this module does
//!
//! It scores *origin*, and nothing else. A [`Provenance`] read off the memory's
//! stored snapshot maps to a [`trust_multiplier`] the broker folds into the
//! composite score, so a corroborated local lesson outranks an anonymous
//! imported one at equal relevance.
//!
//! Two deliberate limits:
//!
//! * **It never blocks retrieval.** Trust is a weight, not a gate. A hard gate
//!   on provenance would make a bad pack import silently delete a user's
//!   working knowledge, which is a worse failure than the one it prevents.
//! * **Corroboration outranks origin.** A memory that has been cited in a
//!   successful local run has been *tested here*, whatever its origin, and
//!   carries no penalty at all from then on. Otherwise an imported pack — the
//!   whole point of which is to share knowledge — would stay second-class
//!   forever.
//!
//! ## Not done here
//!
//! Quarantining imports — routing pack memories into the proposal queue instead
//! of accepting them outright — is the mechanism that actually stops a poisoned
//! pack from influencing anything before a human looks at it, and it is
//! deliberately not in this module: it changes what `brain import` *does*, which
//! belongs in the import path rather than behind a scoring weight. It lives in
//! [`crate::packs::quarantine_memories`], on by default for `http(s)://`
//! sources. This module stays the answer for *history* — a pack imported before
//! quarantine existed is discounted by origin rather than pulled back out of
//! retrieval, because reaching into a working brain on an upgrade is a worse
//! failure than the one quarantine prevents.

use serde::{Deserialize, Serialize};

/// Where a memory came from.
///
/// Ordered least to most trusted, so the enum's ordering *is* the trust
/// ordering and the two cannot drift apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    /// Arrived in a pack (`brain import`), possibly from a URL. The widest
    /// attack surface Kimetsu has: content authored elsewhere, by someone
    /// else, landing in the brain wholesale.
    Pack,
    /// Replicated from another machine or a shared org brain (`brain sync`,
    /// Kimetsu Remote). Authored by someone with access, which is a real
    /// constraint, but not by this user on this machine.
    Remote,
    /// Written by the model — the session-end distiller or reflection. The
    /// content is derived from a real transcript, but no human read it before
    /// it became durable, and a transcript can contain anything the agent was
    /// shown.
    Distilled,
    /// Consolidation output: a staple or merge of memories already in the
    /// brain. Inherits its members' standing and adds no new claims.
    Derived,
    /// Recorded by the user, or by an agent acting on the user's instruction,
    /// on this machine.
    Local,
}

impl Provenance {
    pub fn as_str(self) -> &'static str {
        match self {
            Provenance::Pack => "pack",
            Provenance::Remote => "remote",
            Provenance::Distilled => "distilled",
            Provenance::Derived => "derived",
            Provenance::Local => "local",
        }
    }

    /// Classify a memory's stored `provenance_snapshot_json`.
    ///
    /// Unrecognised or missing provenance reads as [`Provenance::Local`]: every
    /// memory written before this module existed has a snapshot this does not
    /// know, and treating an existing brain's entire contents as untrusted on
    /// upgrade would be a far worse outcome than the attack being defended.
    pub fn from_snapshot(snapshot_json: &str) -> Self {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(snapshot_json) else {
            return Provenance::Local;
        };
        let source = value
            .get("source")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        match source {
            "pack" => Provenance::Pack,
            "remote" | "sync" | "org" => Provenance::Remote,
            "distiller" | "distilled" | "reflection" => Provenance::Distilled,
            "staple" | "merge" | "consolidation" => Provenance::Derived,
            _ => Provenance::Local,
        }
    }
}

/// Multiplier applied to a candidate's composite score.
///
/// `corroborated` means the memory has been cited in a *successful* run on this
/// machine — precisely what `memories.last_useful_at` records, and the reason
/// the signal is a boolean rather than a count: `last_useful_at` is already
/// selected by every candidate query, so reading it costs nothing, whereas
/// counting citations would put a per-row aggregate on the hot path.
///
/// A corroborated memory carries no origin penalty at all, whatever its
/// provenance. It has been tested here; where it was written stops being the
/// most informative thing about it. Without that, an imported pack — the entire
/// point of which is to share knowledge — would stay second-class forever.
///
/// Bounded in `(0, 1]` — trust can only ever hold a memory back, never promote
/// one above its relevance. Promotion is what `usefulness_score` is for, and
/// two mechanisms that both boost would make the composite unreadable.
pub fn trust_multiplier(provenance: Provenance, corroborated: bool) -> f32 {
    if corroborated {
        return 1.0;
    }
    match provenance {
        Provenance::Local | Provenance::Derived => 1.0,
        Provenance::Distilled => 0.95,
        Provenance::Remote => 0.90,
        Provenance::Pack => 0.85,
    }
}

// ── Audit ───────────────────────────────────────────────────────────────────

/// One provenance class in the audit report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProvenanceGroup {
    pub provenance: String,
    pub total: usize,
    /// Cited in a successful run here at least once.
    pub corroborated: usize,
    /// Never corroborated *and* from an external origin: the population a
    /// poisoned memory would be hiding in.
    pub unvetted: usize,
}

/// A suspicious burst of writes.
///
/// Memory poisoning through ordinary interaction tends to arrive as a cluster —
/// MINJA's technique is to induce several related writes in a short window. A
/// human recording lessons does not usually produce thirty memories in a
/// minute; a pack import or a runaway loop does. This flags the shape without
/// claiming to know intent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteBurst {
    /// RFC 3339 minute the burst falls in.
    pub minute: String,
    pub writes: usize,
}

/// The audit report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditReport {
    pub groups: Vec<ProvenanceGroup>,
    pub bursts: Vec<WriteBurst>,
    /// Total active memories considered.
    pub total: usize,
}

/// Writes in one minute above which a cluster is worth a look.
pub const BURST_THRESHOLD: usize = 20;

/// Group the active corpus by provenance and flag write bursts.
///
/// Read-only and non-destructive by design: this reports, and a human decides.
/// An automated purge keyed on a heuristic like "many writes in one minute"
/// would delete a legitimate bulk import, which is a worse outcome than the
/// attack it is guarding against.
pub fn audit(conn: &rusqlite::Connection) -> kimetsu_core::KimetsuResult<AuditReport> {
    use std::collections::BTreeMap;

    let mut stmt = conn.prepare(
        "SELECT provenance_snapshot_json, last_useful_at, created_at
         FROM memories
         WHERE invalidated_at IS NULL AND superseded_by IS NULL",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let mut by_provenance: BTreeMap<Provenance, (usize, usize)> = BTreeMap::new();
    let mut by_minute: BTreeMap<String, usize> = BTreeMap::new();
    for (snapshot, last_useful_at, created_at) in &rows {
        let provenance = Provenance::from_snapshot(snapshot.as_deref().unwrap_or("{}"));
        let entry = by_provenance.entry(provenance).or_insert((0, 0));
        entry.0 += 1;
        if last_useful_at.is_some() {
            entry.1 += 1;
        }
        // RFC 3339 truncated to the minute: "2026-07-24T20:31".
        let minute: String = created_at.chars().take(16).collect();
        *by_minute.entry(minute).or_insert(0) += 1;
    }

    let groups = by_provenance
        .into_iter()
        .map(|(provenance, (total, corroborated))| ProvenanceGroup {
            provenance: provenance.as_str().to_string(),
            total,
            corroborated,
            unvetted: if provenance >= Provenance::Derived {
                0 // local and derived memories have no external origin to vet
            } else {
                total - corroborated
            },
        })
        .collect();

    let mut bursts: Vec<WriteBurst> = by_minute
        .into_iter()
        .filter(|(_, writes)| *writes >= BURST_THRESHOLD)
        .map(|(minute, writes)| WriteBurst { minute, writes })
        .collect();
    bursts.sort_by(|a, b| b.writes.cmp(&a.writes));

    Ok(AuditReport {
        groups,
        bursts,
        total: rows.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_enum_ordering_is_the_trust_ordering() {
        assert!(Provenance::Pack < Provenance::Remote);
        assert!(Provenance::Remote < Provenance::Distilled);
        assert!(Provenance::Distilled < Provenance::Derived);
        assert!(Provenance::Derived < Provenance::Local);

        // …and the multipliers agree with it, so the two cannot drift apart.
        let m = |p| trust_multiplier(p, false);
        assert!(m(Provenance::Pack) < m(Provenance::Remote));
        assert!(m(Provenance::Remote) < m(Provenance::Distilled));
        assert!(m(Provenance::Distilled) <= m(Provenance::Derived));
        assert_eq!(m(Provenance::Local), 1.0);
    }

    /// The upgrade-safety property: an existing brain full of memories written
    /// before provenance was a concept must not become untrusted overnight.
    #[test]
    fn unknown_provenance_reads_as_local() {
        assert_eq!(Provenance::from_snapshot("{}"), Provenance::Local);
        assert_eq!(Provenance::from_snapshot("not json"), Provenance::Local);
        assert_eq!(
            Provenance::from_snapshot(r#"{"source":"manual_cli"}"#),
            Provenance::Local
        );
        assert_eq!(
            Provenance::from_snapshot(r#"{"source":"something-from-the-future"}"#),
            Provenance::Local
        );
    }

    #[test]
    fn known_sources_classify() {
        for (json, expected) in [
            (r#"{"source":"pack"}"#, Provenance::Pack),
            (r#"{"source":"sync"}"#, Provenance::Remote),
            (r#"{"source":"org"}"#, Provenance::Remote),
            (r#"{"source":"distiller"}"#, Provenance::Distilled),
            (r#"{"source":"staple"}"#, Provenance::Derived),
        ] {
            assert_eq!(Provenance::from_snapshot(json), expected, "for {json}");
        }
    }

    /// A shared pack is the whole point of packs. A memory that has proven
    /// itself locally must stop being treated as an outsider.
    #[test]
    fn corroboration_erases_the_origin_penalty() {
        let cold = trust_multiplier(Provenance::Pack, false);
        let proven = trust_multiplier(Provenance::Pack, true);
        assert!(cold < 1.0, "an uncorroborated pack memory is discounted");
        assert!(
            (proven - 1.0).abs() < 1e-6,
            "once cited in a successful run here, origin stops mattering: {proven}"
        );
        for provenance in [
            Provenance::Pack,
            Provenance::Remote,
            Provenance::Distilled,
            Provenance::Derived,
            Provenance::Local,
        ] {
            assert_eq!(trust_multiplier(provenance, true), 1.0, "{provenance:?}");
        }
    }

    /// Trust holds a memory back; it never promotes one. Two mechanisms that
    /// both boost would make the composite score unreadable.
    #[test]
    fn trust_never_exceeds_one() {
        for provenance in [
            Provenance::Pack,
            Provenance::Remote,
            Provenance::Distilled,
            Provenance::Derived,
            Provenance::Local,
        ] {
            for corroborated in [false, true] {
                let m = trust_multiplier(provenance, corroborated);
                assert!(
                    m > 0.0 && m <= 1.0,
                    "{provenance:?} corroborated={corroborated} gave {m}"
                );
            }
        }
    }

    // ── Audit ────────────────────────────────────────────────────────────

    fn audit_conn() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().expect("open");
        crate::schema::initialize(&conn).expect("schema");
        conn
    }

    fn insert(conn: &rusqlite::Connection, id: &str, source: &str, corroborated: bool, at: &str) {
        conn.execute(
            "INSERT INTO memories
             (memory_id, scope, kind, text, normalized_text, confidence,
              provenance_snapshot_json, created_at, last_useful_at)
             VALUES (?1, 'project', 'fact', ?1, ?1, 0.9, ?2, ?3, ?4)",
            rusqlite::params![
                id,
                format!(r#"{{"source":"{source}"}}"#),
                at,
                corroborated.then(|| at.to_string()),
            ],
        )
        .expect("insert");
    }

    /// The population a poisoned memory hides in: external origin, never
    /// corroborated. Local memories are not "unvetted" — there is no external
    /// origin to vet.
    #[test]
    fn audit_counts_the_unvetted_external_population() {
        let conn = audit_conn();
        insert(
            &conn,
            "local-1",
            "manual_cli",
            false,
            "2026-01-01T00:00:00Z",
        );
        insert(&conn, "pack-1", "pack", false, "2026-01-01T00:00:00Z");
        insert(&conn, "pack-2", "pack", true, "2026-01-01T00:00:00Z");
        insert(
            &conn,
            "distilled-1",
            "distiller",
            false,
            "2026-01-01T00:00:00Z",
        );

        let report = audit(&conn).expect("audit");
        assert_eq!(report.total, 4);

        let group = |name: &str| {
            report
                .groups
                .iter()
                .find(|g| g.provenance == name)
                .unwrap_or_else(|| panic!("missing group {name}"))
                .clone()
        };
        assert_eq!(group("local").unvetted, 0, "nothing external to vet");
        assert_eq!(group("pack").total, 2);
        assert_eq!(group("pack").corroborated, 1);
        assert_eq!(
            group("pack").unvetted,
            1,
            "the corroborated one has been tested here"
        );
        assert_eq!(group("distilled").unvetted, 1);
    }

    #[test]
    fn audit_flags_a_write_burst_and_ignores_ordinary_writing() {
        let conn = audit_conn();
        // A human recording lessons across the day.
        for i in 0..10 {
            insert(
                &conn,
                &format!("slow-{i}"),
                "manual_cli",
                false,
                &format!("2026-01-01T{:02}:00:00Z", i),
            );
        }
        assert!(
            audit(&conn).expect("audit").bursts.is_empty(),
            "ordinary writing is not a burst"
        );

        // …and a cluster that arrived faster than anyone types.
        for i in 0..BURST_THRESHOLD {
            insert(
                &conn,
                &format!("burst-{i}"),
                "pack",
                false,
                "2026-02-02T03:04:00Z",
            );
        }
        let bursts = audit(&conn).expect("audit").bursts;
        assert_eq!(bursts.len(), 1, "got: {bursts:?}");
        assert_eq!(bursts[0].minute, "2026-02-02T03:04");
        assert_eq!(bursts[0].writes, BURST_THRESHOLD);
    }

    #[test]
    fn audit_of_an_empty_brain_is_empty_not_an_error() {
        let report = audit(&audit_conn()).expect("audit");
        assert_eq!(report.total, 0);
        assert!(report.groups.is_empty());
        assert!(report.bursts.is_empty());
    }
}
