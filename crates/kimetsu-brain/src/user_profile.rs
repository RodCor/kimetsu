//! v3.0: the user's standing preferences, delivered unconditionally.
//!
//! Preference following is Kimetsu's second-weakest measured ability
//! (LongMemEval 66.7%), and the benchmark page already diagnoses why: *"a
//! preference is a small aside semantically far from the question."*
//!
//! That diagnosis rules out the obvious fix. If "I prefer `thiserror` for
//! library errors" is semantically distant from "add error handling to the
//! parser", then no amount of re-ranking surfaces it — the candidate never
//! enters the pool. Boosting preference-kind memories, adding a profile term to
//! the composite score, tuning the floors: all of them operate on candidates
//! retrieval already found, and this one it did not.
//!
//! So the profile does not go through retrieval. It rides the warm start,
//! which every host now receives, and is therefore in context before the first
//! question is asked — which is what a standing preference *is*. PPRO
//! (arXiv 2607.00017) reaches the same conclusion from the other direction: it
//! derives a user profile from accumulated memories and uses it as an explicit
//! prior rather than as one more retrieval signal.
//!
//! ## What counts as a preference
//!
//! `MemoryKind::Preference` memories, in either the project brain or the
//! cross-project user brain. Ranked by proven usefulness, then recency, and
//! hard-capped — a profile that grows without bound stops being a profile and
//! becomes a second corpus in every prompt.
//!
//! Model-free: this is a `SELECT` and a budget.

use kimetsu_core::KimetsuResult;
use rusqlite::Connection;

/// How many preferences the profile may carry.
///
/// Small on purpose. This is injected on every session, so its cost is paid
/// unconditionally; the top handful of proven preferences is what makes the
/// difference, and the tail is what makes users turn warm start off.
pub const MAX_PREFERENCES: usize = 8;

/// Character budget for the rendered block (~100 tokens), on the same footing
/// as the digest's ~400.
pub const PROFILE_CHAR_BUDGET: usize = 400;

/// Longest single preference kept, before it is skipped as an essay rather
/// than a preference.
const MAX_PREFERENCE_CHARS: usize = 160;

/// One standing preference.
#[derive(Debug, Clone, PartialEq)]
pub struct Preference {
    pub memory_id: String,
    pub text: String,
    /// True when this came from the cross-project user brain rather than this
    /// project — worth knowing, because a global preference should not be
    /// silently overridden by a project convention without the reader noticing.
    pub global: bool,
}

/// Read the user's standing preferences from one brain, strongest first.
///
/// Ordered by proven usefulness before recency: a preference the agent has
/// actually been rewarded for following outranks one stated yesterday.
pub fn preferences(
    conn: &Connection,
    global: bool,
    limit: usize,
) -> KimetsuResult<Vec<Preference>> {
    let mut stmt = conn.prepare(
        "SELECT memory_id, text
         FROM memories
         WHERE kind = 'preference'
           AND invalidated_at IS NULL
           AND superseded_by IS NULL
           AND (valid_to IS NULL OR valid_to > datetime('now'))
         ORDER BY usefulness_score DESC, created_at DESC
         LIMIT ?1",
    )?;
    let rows = stmt
        .query_map(rusqlite::params![limit as i64], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows
        .into_iter()
        .map(|(memory_id, text)| Preference {
            memory_id,
            text,
            global,
        })
        .collect())
}

/// Merge project and global preferences into one profile.
///
/// Project preferences lead: a preference stated for *this* repo is more
/// specific than one carried across every project, and specificity should win
/// when the budget runs out.
pub fn build_profile(
    project_conn: &Connection,
    user_conn: Option<&Connection>,
) -> KimetsuResult<Vec<Preference>> {
    let mut profile = preferences(project_conn, false, MAX_PREFERENCES)?;
    if let Some(user_conn) = user_conn {
        let remaining = MAX_PREFERENCES.saturating_sub(profile.len());
        if remaining > 0 {
            let global = preferences(user_conn, true, remaining)?;
            // A global preference whose text duplicates a project one adds
            // nothing but tokens.
            for pref in global {
                if !profile.iter().any(|p| p.text == pref.text) {
                    profile.push(pref);
                }
            }
        }
    }
    Ok(profile)
}

/// Render the profile as a warm-start section, or `None` when there is nothing
/// to say.
///
/// Framed as instructions rather than as retrieved facts, because that is what
/// they are: the reader should follow them without being asked, which is the
/// whole difference between a preference and a memory.
pub fn render_profile(preferences: &[Preference]) -> Option<String> {
    if preferences.is_empty() {
        return None;
    }
    let mut lines = Vec::new();
    let mut used = 0usize;
    for pref in preferences {
        let text = pref.text.trim();
        if text.is_empty() || text.chars().count() > MAX_PREFERENCE_CHARS {
            continue;
        }
        let line = if pref.global {
            format!("- {text} (across all your projects)")
        } else {
            format!("- {text}")
        };
        if used + line.len() > PROFILE_CHAR_BUDGET {
            break;
        }
        used += line.len();
        lines.push(line);
    }
    if lines.is_empty() {
        return None;
    }
    Some(format!(
        "Standing preferences — follow these without being asked:\n{}",
        lines.join("\n")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        let conn = Connection::open_in_memory().expect("open");
        crate::schema::initialize(&conn).expect("schema");
        conn
    }

    fn insert(conn: &Connection, id: &str, kind: &str, text: &str, usefulness: f32) {
        conn.execute(
            "INSERT INTO memories
             (memory_id, scope, kind, text, normalized_text, confidence,
              provenance_snapshot_json, created_at, usefulness_score)
             VALUES (?1, 'project', ?2, ?3, ?3, 0.9, '{}', '2026-01-01T00:00:00Z', ?4)",
            rusqlite::params![id, kind, text, usefulness],
        )
        .expect("insert");
    }

    #[test]
    fn only_preference_memories_make_the_profile() {
        let c = conn();
        insert(
            &c,
            "p",
            "preference",
            "prefer thiserror for library errors",
            0.0,
        );
        insert(&c, "c", "convention", "always run cargo fmt", 0.0);
        insert(&c, "f", "fact", "the schema is at v11", 0.0);

        let profile = preferences(&c, false, 10).expect("preferences");
        assert_eq!(profile.len(), 1, "got: {profile:?}");
        assert!(profile[0].text.contains("thiserror"));
    }

    /// A preference the agent has been rewarded for following outranks one
    /// merely stated.
    #[test]
    fn proven_preferences_come_first() {
        let c = conn();
        insert(&c, "unproven", "preference", "prefer tabs", 0.0);
        insert(&c, "proven", "preference", "prefer thiserror", 5.0);
        let profile = preferences(&c, false, 10).expect("preferences");
        assert_eq!(profile[0].memory_id, "proven", "got: {profile:?}");
    }

    #[test]
    fn retired_preferences_are_excluded() {
        let c = conn();
        insert(&c, "live", "preference", "prefer thiserror", 0.0);
        insert(&c, "dead", "preference", "prefer failure crate", 0.0);
        c.execute(
            "UPDATE memories SET invalidated_at = '2026-02-01T00:00:00Z' WHERE memory_id = 'dead'",
            [],
        )
        .unwrap();
        let profile = preferences(&c, false, 10).expect("preferences");
        assert_eq!(profile.len(), 1);
        assert_eq!(profile[0].memory_id, "live");
    }

    /// Specificity wins when the budget runs out: a preference stated for this
    /// repo beats one carried across every project.
    #[test]
    fn project_preferences_lead_and_globals_fill_the_remainder() {
        let project = conn();
        let user = conn();
        insert(&project, "proj", "preference", "prefer thiserror here", 0.0);
        insert(
            &user,
            "glob",
            "preference",
            "prefer 2-space indent everywhere",
            0.0,
        );

        let profile = build_profile(&project, Some(&user)).expect("profile");
        assert_eq!(profile.len(), 2);
        assert!(!profile[0].global, "project first: {profile:?}");
        assert!(profile[1].global);
    }

    #[test]
    fn a_global_duplicate_is_not_repeated() {
        let project = conn();
        let user = conn();
        insert(&project, "proj", "preference", "prefer thiserror", 0.0);
        insert(&user, "glob", "preference", "prefer thiserror", 0.0);
        let profile = build_profile(&project, Some(&user)).expect("profile");
        assert_eq!(profile.len(), 1, "got: {profile:?}");
    }

    #[test]
    fn the_profile_is_capped() {
        let c = conn();
        for i in 0..(MAX_PREFERENCES * 3) {
            insert(
                &c,
                &format!("p{i}"),
                "preference",
                &format!("preference {i}"),
                0.0,
            );
        }
        let profile = build_profile(&c, None).expect("profile");
        assert_eq!(profile.len(), MAX_PREFERENCES);
    }

    // ── Rendering ────────────────────────────────────────────────────────

    #[test]
    fn an_empty_profile_renders_nothing() {
        assert!(render_profile(&[]).is_none());
    }

    /// Framed as instructions, not as retrieved facts — following them without
    /// being asked is the whole difference between a preference and a memory.
    #[test]
    fn the_profile_reads_as_instructions() {
        let rendered = render_profile(&[Preference {
            memory_id: "p".into(),
            text: "prefer thiserror for library errors".into(),
            global: false,
        }])
        .expect("rendered");
        assert!(
            rendered.contains("follow these without being asked"),
            "got: {rendered}"
        );
        assert!(rendered.contains("thiserror"));
    }

    #[test]
    fn a_global_preference_is_marked_as_such() {
        let rendered = render_profile(&[Preference {
            memory_id: "p".into(),
            text: "prefer 2-space indent".into(),
            global: true,
        }])
        .expect("rendered");
        assert!(
            rendered.contains("across all your projects"),
            "got: {rendered}"
        );
    }

    /// An essay is not a preference, and a runaway profile is what makes users
    /// turn warm start off.
    #[test]
    fn overlong_preferences_are_skipped_and_the_block_is_budgeted() {
        let essay = Preference {
            memory_id: "long".into(),
            text: "x".repeat(MAX_PREFERENCE_CHARS + 1),
            global: false,
        };
        assert!(
            render_profile(std::slice::from_ref(&essay)).is_none(),
            "a 160+ char 'preference' is an essay"
        );

        let many: Vec<Preference> = (0..MAX_PREFERENCES)
            .map(|i| Preference {
                memory_id: format!("p{i}"),
                text: "prefer something quite specific and reasonably wordy".repeat(2),
                global: false,
            })
            .collect();
        let rendered = render_profile(&many).expect("rendered");
        assert!(
            rendered.len() <= PROFILE_CHAR_BUDGET + 80,
            "block must stay budgeted: {} chars",
            rendered.len()
        );
    }
}
