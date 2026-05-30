//! v0.5.3 e2e: v0.5.0 citations end-to-end.
//!
//! Validates that the cite_memory tool, when invoked by the agent
//! loop, surfaces in `report.context.recorded_citations` with the
//! correct turn index. Then projects the event into the brain (the
//! step a real transport — chat / harbor — would perform) and asserts
//! the `memory_citations` table is populated and the strong-signal
//! (+1.0) usefulness delta lands on cited memories vs the weak
//! (+0.1) on silent passengers.
//!
//! This is the "the brain learns from outcomes" path in miniature —
//! tests every wire from agent → harness → projector → memory row.

use kimetsu_brain::projector;
use kimetsu_brain::user_brain::with_user_brain_disabled;
use kimetsu_core::event::Event;
use kimetsu_e2e::prelude::*;

#[test]
fn cite_memory_tool_call_lands_in_report_context_with_turn_index() {
    with_user_brain_disabled(|| {
        let project = TempProject::init("citations_context");

        // Seed a memory the scripted agent will cite. Without this row
        // the run still works (cite_memory is best-effort), but the
        // downstream blame_run + usefulness scoring path needs a real
        // target.
        let memory_id = project::add_memory(
            project.root(),
            MemoryScope::Repo,
            MemoryKind::Convention,
            "always run cargo fmt before commits",
        )
        .expect("add memory");

        let mut provider = ScriptedProvider::new()
            .turn(|t| {
                t.call_tool(
                    "cite_memory",
                    json!({
                        "memory_id": &memory_id,
                        "rationale": "this is the formatting convention"
                    }),
                )
            })
            .turn(|t| t.done("cited the convention; nothing else to do"))
            .build();

        let mut runtime = ToolRuntime::new(project.root(), RunId::new()).expect("runtime");
        let report = run_model_agent(
            "respect the formatting convention",
            &mut runtime,
            &mut provider,
            KimetsuAgentOpts::for_tests(),
            None,
        )
        .expect("run");

        let citations = report
            .context
            .get("recorded_citations")
            .and_then(Value::as_array)
            .cloned()
            .expect("report.context.recorded_citations should be an array");
        assert_eq!(
            citations.len(),
            1,
            "expected 1 recorded citation; got {citations:?}"
        );
        let entry = &citations[0];
        assert_eq!(
            entry.get("memory_id").and_then(Value::as_str),
            Some(memory_id.as_str()),
            "cited memory_id mismatch"
        );
        // Turn index is harness-injected so transports can attribute
        // citations to the specific turn the model emitted them on.
        assert!(
            entry.get("turn").and_then(Value::as_i64).is_some(),
            "harness must inject `turn` into recorded_citations"
        );
    });
}

#[test]
fn cited_memory_earns_strong_signal_after_run_finished_projection() {
    with_user_brain_disabled(|| {
        let project = TempProject::init("citations_strong_signal");

        // Two memories: one will be cited, one is a silent passenger
        // (retrieved into context but never reached for by cite_memory).
        let cited_id = project::add_memory(
            project.root(),
            MemoryScope::Repo,
            MemoryKind::Fact,
            "kimetsu uses bge-small for default embeddings",
        )
        .expect("add cited memory");
        let silent_id = project::add_memory(
            project.root(),
            MemoryScope::Repo,
            MemoryKind::Fact,
            "kimetsu uses sqlite as the brain store",
        )
        .expect("add silent memory");

        // Manually project the brain-side events the chat / harbor
        // transports would normally emit:
        //   1. context.injected — both memories surfaced into the bundle
        //   2. memory.cited — only the cited one
        //   3. run.finished  — clean exit, apply usefulness deltas
        //
        // This mirrors what `run_model_agent`'s caller (chat REPL or
        // kimetsu-harbor-agent binary) does in production. The e2e
        // scope ends at "the agent loop returns a report"; this
        // projection step is the transport's responsibility.
        let conn = project.open_brain();
        let run_id = RunId::new();
        let injected = Event::new(
            run_id,
            "context.injected",
            json!({
                "stage": "localization",
                // Projector reads `memory_ids` (array of strings) — see
                // `collect_injected_memory_ids` in kimetsu-brain.
                "memory_ids": [&cited_id, &silent_id],
            }),
        );
        let cited = Event::new(
            run_id,
            "memory.cited",
            json!({
                "memory_id": &cited_id,
                "turn": 1,
                "rationale": "answering an embeddings question"
            }),
        );
        let finished = Event::new(
            run_id,
            "run.finished",
            json!({"status": "success", "total_cost_usd": 0.0, "total_tool_calls": 0}),
        );
        projector::apply_events(&conn, &[injected, cited, finished]).expect("project events");

        // memory_citations: exactly one row, for the cited memory + turn=1.
        assert_eq!(
            count_citations(&conn),
            1,
            "memory_citations should have exactly one row"
        );
        assert_citation_recorded(&conn, &run_id.to_string(), &cited_id, 1);

        // Usefulness deltas: cited memory got +1.0 (strong), silent
        // got +0.1 (weak). use_count incremented on both.
        let (cited_score, cited_uses): (f64, i64) = conn
            .query_row(
                "SELECT usefulness_score, use_count FROM memories WHERE memory_id = ?1",
                rusqlite::params![&cited_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("query cited");
        let (silent_score, silent_uses): (f64, i64) = conn
            .query_row(
                "SELECT usefulness_score, use_count FROM memories WHERE memory_id = ?1",
                rusqlite::params![&silent_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("query silent");

        assert!(
            (cited_score - 1.0).abs() < 1e-6,
            "cited memory should have usefulness_score = +1.0; got {cited_score}"
        );
        assert!(
            (silent_score - 0.1).abs() < 1e-6,
            "silent passenger should have usefulness_score = +0.1; got {silent_score}"
        );
        assert_eq!(cited_uses, 1, "cited memory use_count should be 1");
        assert_eq!(silent_uses, 1, "silent memory use_count should be 1");
    });
}
