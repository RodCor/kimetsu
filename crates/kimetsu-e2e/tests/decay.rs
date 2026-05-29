//! v0.5.3 e2e: v0.5.1 usefulness decay end-to-end.
//!
//! Validates that the half-life decay curve actually changes broker
//! retrieval order in a real project. Seeds two memories with
//! identical text + identical usefulness history but different
//! `last_useful_at` timestamps (one a day ago, one a year ago), then
//! asks the broker for context and asserts the recent one ranks
//! first.
//!
//! Decay attenuates the *deviation from neutral* of the usefulness
//! multiplier (1.0 + (raw - 1.0) * decay). A year-old +1.5-boosted
//! memory slides back toward 1.0, behind a day-old +1.5-boosted one.

use kimetsu_brain::context::{ContextRequest, retrieve_context};
use kimetsu_brain::user_brain::with_user_brain_disabled;
use kimetsu_core::config::BrokerWeights;
use kimetsu_core::memory::normalize_memory_text;
use kimetsu_e2e::prelude::*;
use time::OffsetDateTime;

#[test]
fn aged_cited_memory_ranks_below_recently_cited_under_default_half_life() {
    with_user_brain_disabled(|| {
        let project = TempProject::init("decay_ranking");
        let conn = project.open_brain();

        let now = OffsetDateTime::now_utc();
        let fmt = &time::format_description::well_known::Rfc3339;
        let one_day_ago = (now - time::Duration::seconds(86_400))
            .format(fmt)
            .expect("format");
        let one_year_ago = (now - time::Duration::seconds(365 * 86_400))
            .format(fmt)
            .expect("format");

        // Both memories carry max usefulness (+1.5 multiplier) and
        // identical lexical match. Only `last_useful_at` differs.
        for (mid, last_useful) in [
            ("m_recent_e2e", &one_day_ago),
            ("m_aged_e2e", &one_year_ago),
        ] {
            let text = "kimetsu uses anthropic prompt caching";
            let normalized = normalize_memory_text(text);
            conn.execute(
                "
                INSERT INTO memories (
                    memory_id, scope, kind, text, normalized_text, confidence,
                    source_event_id, provenance_snapshot_json, created_at,
                    use_count, usefulness_score, last_useful_at
                )
                VALUES (?1, 'repo', 'fact', ?2, ?3, 1.0, NULL, '{}',
                        '2024-01-01T00:00:00Z', 5, 5.0, ?4)
                ",
                rusqlite::params![mid, text, normalized, last_useful],
            )
            .expect("seed memory");
            conn.execute(
                "INSERT INTO memories_fts (memory_id, text, kind, scope)
                 VALUES (?1, ?2, 'fact', 'repo')",
                rusqlite::params![mid, text],
            )
            .expect("seed fts");
        }

        // Default 30-day half-life → 1 year ≈ 12 half-lives → aged
        // memory's deviation collapses essentially to zero. Recent
        // memory keeps the full +0.5 deviation.
        let weights = BrokerWeights::default();
        let bundle = retrieve_context(
            &conn,
            project.root().to_string_lossy().as_ref(),
            &weights,
            ContextRequest {
                stage: "localization".to_string(),
                query: "anthropic caching".to_string(),
                budget_tokens: 4000,
                ..Default::default()
            },
        )
        .expect("retrieve");

        let memory_order: Vec<String> = bundle
            .capsules
            .iter()
            .filter_map(|c| c.expansion_handle.strip_prefix("memory:").map(String::from))
            .collect();

        assert_eq!(
            memory_order.first().map(String::as_str),
            Some("m_recent_e2e"),
            "recent-cited memory must rank first under 30-day half-life; got order {memory_order:?}"
        );
    });
}

#[test]
fn decay_can_be_disabled_via_broker_weights() {
    with_user_brain_disabled(|| {
        let project = TempProject::init("decay_disabled");
        let conn = project.open_brain();

        let now = OffsetDateTime::now_utc();
        let fmt = &time::format_description::well_known::Rfc3339;
        let recent = (now - time::Duration::seconds(86_400))
            .format(fmt)
            .expect("format");
        let aged = (now - time::Duration::seconds(365 * 86_400))
            .format(fmt)
            .expect("format");

        for (mid, last_useful) in [
            ("m_recent_off", &recent),
            ("m_aged_off", &aged),
        ] {
            let text = "regression guard for the disable-decay knob";
            let normalized = normalize_memory_text(text);
            conn.execute(
                "
                INSERT INTO memories (
                    memory_id, scope, kind, text, normalized_text, confidence,
                    source_event_id, provenance_snapshot_json, created_at,
                    use_count, usefulness_score, last_useful_at
                )
                VALUES (?1, 'repo', 'fact', ?2, ?3, 1.0, NULL, '{}',
                        '2024-01-01T00:00:00Z', 5, 5.0, ?4)
                ",
                rusqlite::params![mid, text, normalized, last_useful],
            )
            .expect("seed");
            conn.execute(
                "INSERT INTO memories_fts (memory_id, text, kind, scope)
                 VALUES (?1, ?2, 'fact', 'repo')",
                rusqlite::params![mid, text],
            )
            .expect("seed fts");
        }

        // Set half_life_days = 0 → decay is the constant 1.0 → both
        // memories tie on score, no ranking flip.
        let weights = BrokerWeights {
            decay_half_life_days: 0.0,
            ..BrokerWeights::default()
        };

        let bundle = retrieve_context(
            &conn,
            project.root().to_string_lossy().as_ref(),
            &weights,
            ContextRequest {
                stage: "localization".to_string(),
                query: "disable decay knob".to_string(),
                budget_tokens: 4000,
                ..Default::default()
            },
        )
        .expect("retrieve");

        // With decay off, the two memories should produce equal scores.
        let scores: Vec<(String, f32)> = bundle
            .capsules
            .iter()
            .filter_map(|c| {
                c.expansion_handle
                    .strip_prefix("memory:")
                    .map(|id| (id.to_string(), c.score))
            })
            .collect();

        assert_eq!(scores.len(), 2, "both memories must surface");
        let recent_score = scores
            .iter()
            .find(|(id, _)| id == "m_recent_off")
            .map(|(_, s)| *s)
            .expect("recent present");
        let aged_score = scores
            .iter()
            .find(|(id, _)| id == "m_aged_off")
            .map(|(_, s)| *s)
            .expect("aged present");

        assert!(
            (recent_score - aged_score).abs() < 1e-4,
            "with decay disabled, both memories should tie on score: recent={recent_score} aged={aged_score}"
        );
    });
}
