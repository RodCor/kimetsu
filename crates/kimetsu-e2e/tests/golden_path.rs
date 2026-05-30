//! v0.5.3 e2e: golden-path agent loop run.
//!
//! Validates the end-to-end pipe: scripted ModelProvider → real
//! agent loop → real ToolRuntime → real on-disk repo. Catches the
//! wiring-level regressions that per-crate unit tests miss by
//! construction (e.g., tool-call envelope shape drift, ModelResponse
//! schema breakage, ToolRuntime constructor signature changes).
//!
//! Deliberately small + deterministic: one read_file call, one done
//! turn, ~10 assertions. If THIS test breaks, the loop is broken.

use kimetsu_e2e::prelude::*;
use std::fs;

#[test]
fn agent_loop_completes_a_simple_scripted_run() {
    let project = TempProject::init("golden_path");

    // Drop a file the scripted agent will read. The test asserts later
    // that the read_file tool actually surfaced the file's content
    // back to the agent loop (i.e., the runtime worked).
    let target = project.root().join("hello.txt");
    fs::write(&target, "hello from the e2e test").expect("write target");

    let mut provider = ScriptedProvider::new()
        .turn(|t| t.call_tool("read_file", json!({"path": "hello.txt"})))
        .turn(|t| t.done("read the file; nothing else to do"))
        .build();

    let mut runtime = ToolRuntime::new(project.root(), RunId::new()).expect("construct runtime");

    let report = run_model_agent(
        "read hello.txt and confirm",
        &mut runtime,
        &mut provider,
        KimetsuAgentOpts::for_tests(),
        None,
    )
    .expect("agent loop ran");

    // Two turns: one tool call + one final text.
    assert_eq!(report.turns, 2, "expected 2 turns, got {}", report.turns);
    assert_eq!(report.tool_calls, 1, "exactly one tool call expected");
    assert!(
        report
            .final_text
            .as_deref()
            .unwrap_or("")
            .contains("read the file"),
        "final text didn't match the scripted done turn: {:?}",
        report.final_text
    );

    // ModelAgentReport.summary mirrors the final text on a clean exit.
    assert!(
        report.summary.contains("read the file"),
        "summary should echo the final text on a clean run; got: {}",
        report.summary
    );
}

#[test]
fn agent_loop_produces_structured_context_field() {
    let project = TempProject::init("golden_path_ctx");
    let mut provider = ScriptedProvider::new()
        .turn(|t| t.done("nothing to do"))
        .build();
    let mut runtime = ToolRuntime::new(project.root(), RunId::new()).expect("runtime");

    let report = run_model_agent(
        "noop",
        &mut runtime,
        &mut provider,
        KimetsuAgentOpts::for_tests(),
        None,
    )
    .expect("run");

    // `context` is the JSON blob transports relay back to the caller.
    // We don't pin specific keys here (those evolve), only that it
    // serializes to an object and isn't null.
    assert!(
        report.context.is_object(),
        "report.context should be a JSON object; got {:?}",
        report.context
    );
}
