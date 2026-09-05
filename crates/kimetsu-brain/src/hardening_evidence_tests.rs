use crate::{episode, projector, schema};
use kimetsu_core::{event::Event, ids::RunId};
use rusqlite::Connection;
use serde_json::json;
fn event(kind: &str, payload: serde_json::Value) -> Event {
    Event::new(RunId::new(), kind, payload)
}
fn conn() -> Connection {
    let c = Connection::open_in_memory().unwrap();
    schema::initialize(&c).unwrap();
    c
}
fn accepted(id: &str) -> Event {
    event(
        "memory.accepted",
        json!({"memory_id":id,"scope":"project","kind":"fact","text":format!("quokka {id}")}),
    )
}
#[test]
fn hardening_concurrent_episode_lanes_replay() {
    let c = conn();
    for (lane, note) in [
        ("a", "first"),
        ("b", "other"),
        ("a", "latest"),
        ("", "legacy"),
    ] {
        projector::apply_events(
            &c,
            &[event(
                "work.episode",
                serde_json::to_value(episode::EpisodePayload {
                    repo_root: "repo".into(),
                    identity: lane.into(),
                    note: note.into(),
                    ..Default::default()
                })
                .unwrap(),
            )],
        )
        .unwrap();
    }
    for replay in [false, true] {
        if replay {
            projector::rebuild_in_place(&c).unwrap();
        }
        assert_eq!(
            episode::load_live_episode_scoped(&c, "repo", "a")
                .unwrap()
                .unwrap()
                .note,
            "latest"
        );
        assert_eq!(
            episode::load_live_episode_scoped(&c, "repo", "b")
                .unwrap()
                .unwrap()
                .note,
            "other"
        );
        assert!(
            episode::load_live_episode_scoped(&c, "repo", "missing")
                .unwrap()
                .is_none()
        );
        assert_eq!(
            episode::load_live_episode(&c, "repo")
                .unwrap()
                .unwrap()
                .note,
            "legacy"
        );
    }
}
#[test]
fn hardening_archive_restore_replay_preserves_expiry_and_invalidity() {
    let c = conn();
    projector::apply_events(
        &c,
        &[
            accepted("archive"),
            accepted("invalid"),
            accepted("superseded"),
            event(
                "memory.temporal",
                json!({"memory_id":"archive","valid_to":"2025-01-01T00:00:00Z"}),
            ),
            event(
                "memory.invalidated",
                json!({"memory_id":"archive","reason":"forgotten"}),
            ),
            event(
                "memory.invalidated",
                json!({"memory_id":"invalid","reason":"incorrect"}),
            ),
            event(
                "memory.invalidated",
                json!({"memory_id":"superseded","reason":"forgotten"}),
            ),
            event(
                "memory.superseded",
                json!({"memory_id":"superseded","survivor_id":"archive"}),
            ),
            event("memory.restored", json!({"memory_id":"archive"})),
            event(
                "memory.invalidated",
                json!({"memory_id":"invalid","reason":"forgotten"}),
            ),
            event("memory.restored", json!({"memory_id":"invalid"})),
            event("memory.restored", json!({"memory_id":"superseded"})),
        ],
    )
    .unwrap();
    for replay in [false, true] {
        if replay {
            projector::rebuild_in_place(&c).unwrap();
        }
        assert!(c.query_row("SELECT invalidated_at IS NULL AND valid_to='2025-01-01T00:00:00Z' FROM memories WHERE memory_id='archive'",[],|r|r.get::<_,bool>(0)).unwrap());
        assert_eq!(c.query_row("SELECT COUNT(*) FROM memories WHERE memory_id IN ('invalid','superseded') AND invalidated_at IS NOT NULL",[],|r|r.get::<_,i64>(0)).unwrap(),2);
        assert_eq!(
            c.query_row(
                "SELECT COUNT(*) FROM memories_fts WHERE memory_id='archive'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
    }
}
#[test]
fn hardening_manual_conflict_replay_and_atomic_validation() {
    let c = conn();
    projector::apply_events(&c, &[accepted("a"), accepted("b"), accepted("c")]).unwrap();
    for (id, a, b) in [("ab", "a", "b"), ("ac", "a", "c")] {
        c.execute("INSERT INTO memory_conflicts(conflict_id,new_memory_id,existing_memory_id,scope,kind,similarity,detected_at) VALUES(?1,?2,?3,'project','fact',0.95,'2026-01-01T00:00:00Z')",[id,a,b]).unwrap();
    }
    assert!(crate::conflict::resolve_conflict(&c, "ab", "kept_new").unwrap());
    assert!(crate::conflict::resolve_conflict(&c, "ac", "kept_both").unwrap());
    assert!(!crate::conflict::resolve_conflict(&c, "ab", "kept_existing").unwrap());
    projector::rebuild_in_place(&c).unwrap();
    assert_eq!(
        c.query_row(
            "SELECT COUNT(*) FROM memory_conflicts WHERE resolved_at IS NOT NULL",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        2
    );
    assert!(
        c.query_row(
            "SELECT invalidated_at IS NOT NULL FROM memories WHERE memory_id='b'",
            [],
            |r| r.get::<_, bool>(0)
        )
        .unwrap()
    );
    assert!(
        c.query_row(
            "SELECT invalidated_at IS NULL FROM memories WHERE memory_id='c'",
            [],
            |r| r.get::<_, bool>(0)
        )
        .unwrap()
    );
    let before: i64 = c
        .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
        .unwrap();
    assert!(projector::apply_events(&c,&[event("conflict.resolved",json!({"conflict_id":"ab","new_memory_id":"b","existing_memory_id":"a","resolution":"kept_both"}))]).is_err());
    assert_eq!(
        c.query_row("SELECT COUNT(*) FROM events", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        before
    );
}
#[test]
fn hardening_exposure_no_invention_stale_feedback_and_unknown_outcome() {
    crate::user_brain::with_user_brain_disabled(|| {
        use crate::project;
        use kimetsu_core::memory::{MemoryKind, MemoryScope};
        let root = std::env::temp_dir().join(format!("kimetsu-exposure-{}", ulid::Ulid::new()));
        kimetsu_core::paths::git_init_boundary(&root);
        project::init_project(&root, false).unwrap();
        let id = project::add_memory(&root, MemoryScope::Project, MemoryKind::Fact, "quokka fact")
            .unwrap();
        assert_eq!(
            crate::reinforce::credit_benchmark_outcome(&root, "quokka", true, 3).unwrap(),
            0
        );
        let exposure = event(
            "context.injected",
            json!({"memory_ids":[id],"memory_revisions":{id.clone():format!("baseline:{id}")}}),
        );
        project::record_context_exposure(&root, &exposure).unwrap();
        assert_eq!(
            project::record_exposure_outcome(&root, &exposure.event_id.to_string(), None).unwrap(),
            0
        );
        assert!(
            project::record_exposure_citation(
                &root,
                &exposure.event_id.to_string(),
                "unknown",
                None
            )
            .is_err()
        );
        project::record_exposure_citation(&root, &exposure.event_id.to_string(), &id, None)
            .unwrap();
        let (_, _, c) = project::load_project(&root).unwrap();
        assert_eq!(
            c.query_row(
                "SELECT use_count FROM memories WHERE memory_id=?1",
                [&id],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        projector::apply_events(
            &c,
            &[event(
                "memory.corrected",
                json!({"memory_id":id,"text":"replacement quokka"}),
            )],
        )
        .unwrap();
        project::record_exposure_outcome(&root, &exposure.event_id.to_string(), Some(true))
            .unwrap();
        assert_eq!(
            c.query_row(
                "SELECT use_count FROM memories WHERE memory_id=?1",
                [&id],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        assert!(
            c.query_row(
                "SELECT last_useful_at IS NULL FROM memories WHERE memory_id=?1",
                [&id],
                |r| r.get::<_, bool>(0)
            )
            .unwrap()
        );
        let empty = event(
            "context.injected",
            json!({"memory_ids":[],"memory_revisions":{}}),
        );
        project::record_context_exposure(&root, &empty).unwrap();
        assert_eq!(
            project::record_exposure_outcome(&root, &empty.event_id.to_string(), Some(true))
                .unwrap(),
            0
        );
        let revision = projector::claim_revision_at(&c, &id, None).unwrap();
        let current = event(
            "context.injected",
            json!({"memory_ids":[id],"memory_revisions":{id.clone():revision}}),
        );
        project::record_context_exposure(&root, &current).unwrap();
        project::record_exposure_citation(&root, &current.event_id.to_string(), &id, None).unwrap();
        assert_eq!(
            project::record_exposure_outcome(&root, &current.event_id.to_string(), Some(true))
                .unwrap(),
            1
        );
        assert_eq!(
            project::record_exposure_outcome(&root, &current.event_id.to_string(), Some(true))
                .unwrap(),
            0
        );
        projector::rebuild_in_place(&c).unwrap();
        assert_eq!(
            c.query_row(
                "SELECT use_count FROM memories WHERE memory_id=?1",
                [&id],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        drop(c);
        std::fs::remove_dir_all(root).unwrap();
    });
}

#[test]
fn hardening_roi_labels_assumptions_and_keeps_delivery_units_separate() {
    let c = conn();
    projector::apply_events(&c,&[event("context.injected",json!({"memory_ids":[],"memory_revisions":{},"used_tokens":100,"cost_unit":"serialized_utf8_byte_bound"})),event("context.injected",json!({"memory_ids":[],"memory_revisions":{},"used_tokens":20}))]).unwrap();
    let report =
        crate::roi::roi_report(&c, crate::roi::RoiWindow::All, "test-model", None).unwrap();
    assert!(report.estimate_label.contains("not measured"));
    assert_eq!(report.model, "test-model");
    assert_eq!(
        report.delivered_cost_by_unit["serialized_utf8_byte_bound"],
        100
    );
    assert_eq!(report.delivered_cost_by_unit["legacy_token_estimate"], 20);
    assert_eq!(report.assumptions["tokens_per_citation"]["fact"], 500);
}

#[test]
fn hardening_mixed_revision_run_is_not_current_claim_evidence() {
    let c = conn();
    let run = RunId::new();
    projector::apply_events(&c, &[accepted("m")]).unwrap();
    let old = Event::new(
        run,
        "context.injected",
        json!({"memory_ids":["m"],"memory_revisions":{"m":"baseline:m"}}),
    );
    projector::apply_events(
        &c,
        &[
            old,
            event(
                "memory.corrected",
                json!({"memory_id":"m","text":"replacement"}),
            ),
        ],
    )
    .unwrap();
    let revision = projector::claim_revision_at(&c, "m", None).unwrap();
    projector::apply_events(
        &c,
        &[
            Event::new(
                run,
                "context.injected",
                json!({"memory_ids":["m"],"memory_revisions":{"m":revision}}),
            ),
            Event::new(run, "run.finished", json!({})),
        ],
    )
    .unwrap();
    assert_eq!(
        c.query_row(
            "SELECT use_count FROM memories WHERE memory_id='m'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
}

#[test]
fn hardening_explicit_revision_cannot_override_actual_delivery() {
    let c = conn();
    let run = RunId::new();
    projector::apply_events(&c, &[accepted("m")]).unwrap();
    let exposure = Event::new(
        run,
        "context.injected",
        json!({"memory_ids":["m"],"memory_revisions":{"m":"baseline:m"}}),
    );
    let exposure_id = exposure.event_id.to_string();
    projector::apply_events(
        &c,
        &[
            exposure,
            event(
                "memory.corrected",
                json!({"memory_id":"m","text":"replacement"}),
            ),
        ],
    )
    .unwrap();
    let revision = projector::claim_revision_at(&c, "m", None).unwrap();
    projector::apply_events(
        &c,
        &[Event::new(
            run,
            "memory.cited",
            json!({"memory_id":"m","exposure_id":exposure_id,"revision_event_id":revision}),
        )],
    )
    .unwrap();
    assert_eq!(
        c.query_row("SELECT count(*) FROM memory_citations", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
}
#[test]
fn hardening_partial_episode_preserves_explicit_lane() {
    let c = conn();
    projector::apply_events(
        &c,
        &[event(
            "work.episode",
            json!({"repo_root":"repo","identity":"task-a","note":"checkpoint"}),
        )],
    )
    .unwrap();
    assert_eq!(
        episode::load_live_episode_scoped(&c, "repo", "task-a")
            .unwrap()
            .unwrap()
            .note,
        "checkpoint"
    );
    assert!(episode::load_live_episode(&c, "repo").unwrap().is_none());
}
