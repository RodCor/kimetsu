//! Protocol-agnostic Kimetsu agent harness.
//!
//! This module owns the shared agent loop, model prompt contract, tool
//! registry, dispatcher, and MP-18 verify behavior used by product surfaces
//! such as `kimetsu chat` and by benchmark adapters. Terminal-Bench / Harbor
//! transport code lives in `kimetsu-harbor-rs`; this module does not own
//! JSON-RPC wire types, benchmark sessions, or benchmark stubs.
use kimetsu_core::KimetsuResult;
use serde_json::{Value, json};

use crate::tools::{CommandSpec, ListFilesInput, MultiReadLinesInput, ReadFileLinesInput};

const KIMETSU_HARNESS_VERSION: &str = "0.1";

/// MP-7d / MP-13b / MP-17m.2: hard ceiling on model <-> tool ping-pong.
///
/// The number's bumped 25 -> 40 -> 80 across phases as we saw real tasks
/// run out before finishing. MP-17m.2 makes this a SAFETY NET rather
/// than the primary stop condition; the real cutoff is now activity-
/// based via `STALL_WINDOW_TURNS` below. A model making steady useful
/// tool calls can run all 80 turns; a model spinning in pure
/// deliberation gets cut off at the stall window.
pub const DEFAULT_MODEL_TURN_BUDGET: u32 = 80;

/// MP-18: how many times we ask the model to "verify before finish"
/// before giving up and accepting the finish unconditionally. Set low
/// because each iteration adds at minimum one model turn (the verify
/// response) and at maximum several more turns (workspace inspection +
/// fix-ups). Two iterations means: model tries to finish -> verify nudge
/// -> model checks / fixes -> tries to finish again -> verify nudge ->
/// model finishes -> accepted. So up to 3 finish-attempts per task.
pub const MAX_VERIFY_ITERATIONS: u32 = 2;

/// MP-17m.2: outer-loop heartbeat. If the model hasn't produced ANY
/// useful tool call (touches workspace state - shell, file, git) in
/// this many consecutive turns, treat it as stalled and wrap up.
/// Pure-deliberation tools (think, plan) don't count as activity
/// because a sequence of them is exactly the loop pattern we want to
/// catch. The persistence + self-verify gates already cover the
/// "no tool calls at all" case before finish; this catches the
/// "lots of think/plan, no real work" case during the run.
pub const STALL_WINDOW_TURNS: u32 = 10;

/// MP-17m.2: tool names that count as "useful" - they touch workspace
/// state or read it back. Anything not in this set (think, plan,
/// view_image-as-passive-metadata, future no-op tools) is silently
/// counted as deliberation and doesn't reset the stall counter.
const USEFUL_TOOL_NAMES: &[&str] = &[
    "shell_command",
    "shell_background",
    "shell_status",
    "shell_output",
    "shell_stop",
    "read_file",
    "multi_read",
    "list_files",
    "glob",
    "search_files",
    "write_file",
    "edit_file",
    "apply_patch",
    "move_file",
    "delete_file",
    "git_status",
    "git_diff",
];

fn is_useful_tool(name: &str) -> bool {
    USEFUL_TOOL_NAMES.contains(&name)
}

/// MP-7d: report returned by `run_model_agent`. v0.3.1 Phase-2: also
/// carries the summary + context a caller may emit through its own
/// transport. This decouples the agent loop from any specific I/O surface.
#[derive(Debug)]
pub struct ModelAgentReport {
    pub turns: u32,
    pub tool_calls: u32,
    pub stop_reason: crate::model::StopReason,
    pub final_text: Option<String>,
    pub usage: crate::model::TokenUsage,
    /// Summary string for the caller's transport response: either the
    /// model's final plain-text response or a budget-exhaustion sentinel
    /// built from the loop stop reason.
    pub summary: String,
    /// Structured context for the caller's transport response (turns, cost,
    /// deviations, etc.).
    pub context: serde_json::Value,
}

/// MP-7d: the real agent loop. Wires a `ModelProvider` (claude_code,
/// anthropic, or a `MockProvider` in tests) to a `ToolRuntime`. The model
/// issues tool calls based on the task; we run them via the runtime, feed
/// the result back as a tool result message, and loop until the model
/// returns plain text or we exhaust the turn budget.
///
/// MP-13: opts for tuning the Kimetsu agent loop. Defaults are neutral for
/// product usage; benchmark adapters should pass their own mode/source.
/// Tests use `KimetsuAgentOpts::for_tests()` to disable the auto-orient
/// pre-shell + persistence gate so scripted MockProvider responses aren't
/// perturbed.
#[derive(Debug, Clone, Copy)]
pub struct KimetsuAgentOpts {
    pub turn_budget: u32,
    pub auto_orient: bool,
    pub min_actions_before_finish: u32,
    /// Transport label used in prompt text and telemetry. Harbor mode
    /// keeps the Terminal-Bench wording; chat uses a product-facing label.
    pub mode: &'static str,
    pub task_source: &'static str,
    /// Strict verify policy. None keeps the legacy env-controlled behavior
    /// for Harbor runs; product surfaces pass an explicit value.
    pub strict_verify: Option<bool>,
    /// MP-17c: fire a one-time self-verify reminder right before the
    /// model's first finish attempt. Disabled in tests so scripted
    /// MockProvider responses aren't perturbed.
    pub self_verify_nudge_enabled: bool,
}

impl Default for KimetsuAgentOpts {
    fn default() -> Self {
        Self {
            turn_budget: DEFAULT_MODEL_TURN_BUDGET,
            auto_orient: true,
            min_actions_before_finish: 3,
            mode: "kimetsu",
            task_source: "user",
            strict_verify: Some(false),
            self_verify_nudge_enabled: true,
        }
    }
}

impl KimetsuAgentOpts {
    pub fn for_harbor() -> Self {
        Self {
            mode: "harbor",
            task_source: "Harbor / Terminal-Bench",
            strict_verify: None,
            ..Self::default()
        }
    }

    #[cfg(test)]
    pub fn for_tests() -> Self {
        Self {
            turn_budget: DEFAULT_MODEL_TURN_BUDGET,
            auto_orient: false,
            min_actions_before_finish: 0,
            mode: "test",
            task_source: "test",
            strict_verify: Some(false),
            self_verify_nudge_enabled: false,
        }
    }
}

/// v0.3.1 Phase-2: agent loop is now transport-agnostic. Doesn't take a
/// session anymore - callers (benchmark adapters, chat mode, future transports)
/// inspect the returned `ModelAgentReport.summary` and `.context` and
/// emit them in whatever protocol they speak.
pub fn run_model_agent(
    task: &str,
    runtime: &mut crate::tools::ToolRuntime,
    provider: &mut dyn crate::model::ModelProvider,
    opts: KimetsuAgentOpts,
    brain_context: Option<&str>,
) -> KimetsuResult<ModelAgentReport> {
    let turn_budget = opts.turn_budget;
    let auto_orient = opts.auto_orient;
    let min_actions_before_finish = opts.min_actions_before_finish;
    let mode = opts.mode;
    let task_source = opts.task_source;
    use crate::model::{ModelMessage, ModelRequest, StopReason, ToolChoice};

    // MP-9 (path B): Claude Code 2.x in `-p` mode injects its own
    // agentic harness over our --system-prompt and tells the model it
    // has Monitor / PushNotification / RemoteTrigger tools. The MP-8
    // gauntlet caught the model trusting CC's harness over our system
    // prompt and never invoking shell_command.
    //
    // The fix is to make the envelope contract authoritative in the
    // USER message (which the model reads last, right before
    // responding). The system prompt is still descriptive; the user
    // message carries the actual task PLUS an explicit override note
    // that:
    //   1. tells the model to ignore any other tool catalog CC may have
    //      mentioned;
    //   2. repeats the envelope grammar verbatim;
    //   3. asks for a tool_call envelope as the first response.
    // This is the v0.1 envelope pattern adapted to compete with CC's
    // harness override.
    let system = MessageMessage::system_prompt_for_mode(mode);

    // MP-13a (auto-orient): bare Claude Code's agentic harness orients
    // the model implicitly - directory listing, README peek, build
    // system sniff - before the first model turn. The kimetsu wrapper
    // doesn't do that, so the model spends its first 3-5 turns on
    // `pwd && ls && cat README*` orientation. On a 25-40 turn budget
    // that's a ~15-20% tax on every task.
    //
    // Fix: run one composite shell command up front and inject the
    // result into the user message as "Initial workspace state". The
    // model arrives at the task description already knowing pwd,
    // top-level layout, and the likely task-instructions file. Saves
    // 1-3 turns per task and gives the model better grounding for its
    // first tool call.
    let orient_block = if auto_orient {
        match collect_workspace_orientation(runtime) {
            Some(text) if !text.trim().is_empty() => format!(
                "=== Initial workspace state (Kimetsu auto-orientation) ===\n\
                 {text}\n\n",
            ),
            _ => String::new(),
        }
    } else {
        String::new()
    };

    // MP-11 (brain mode): if a kimetsu project supplied broker context
    // for this task - curated memories, prior-run capsules - render it
    // as a "Prior context" section the model sees BEFORE the task.
    // This is the kimetsu-brain leg of the v0.2 falsifiable claim. In
    // no-brain mode `brain_context` is None and the rendered section is
    // omitted entirely (no "empty memories" stub that would dilute the
    // model's attention).
    let prior_block = match brain_context {
        Some(text) if !text.trim().is_empty() => format!(
            "=== Prior context (from Kimetsu's broker - curated memories \
             and prior-run capsules retrieved for this task) ===\n\
             {text}\n\n",
        ),
        _ => String::new(),
    };

    // MP-12: full v0.1-comparable tool surface (7 tools instead of just
    // shell_command). Each composed-tool implementation dispatches through
    // the configured ToolRuntime, then assembles a structured result the
    // model can rely on (read_file gets line
    // numbers + truncation; list_files gets a sized listing; etc.).
    // The model sees these as first-class JSON tools, which (a) cuts
    // the "I need to remember the exact bash invocation" overhead and
    // (b) collapses multi-step idioms (read-then-modify) into single
    // turns.
    let user = ModelMessage::user_text(format!(
        "{orient_block}\
         {prior_block}\
          Task (from {task_source}):\n\
         {task}\n\n\
         === Important runtime override ===\n\
         You are running inside the Kimetsu wrapper. Any tool catalog the\n\
         Claude Code runtime advertises (Monitor, PushNotification,\n\
         RemoteTrigger, Bash, Edit, etc.) is NOT real here.\n\
         The only way to take action is to emit a JSON envelope that\n\
         Kimetsu will parse out of your response text and execute. The\n\
         tool set below is what's available; ignore everything else.\n\
         \n\
         === Tools (pick the most specific one; fall back to shell_command) ===\n\
         - read_file:    {{path, offset?, limit?, max_lines?}} -> {{content, lines_total, truncated}}\n\
                         (use offset+limit to read a SLICE of a big file - much cheaper than full read)\n\
         - multi_read:   {{paths:[...]}} or {{files:[{{path, offset?, limit?}}]}} -> {{files:[...]}}\n\
                         (batch N file reads in one call; cheaper than N read_file calls)\n\
         - list_files:   {{path?, max_depth?, max_entries?}} -> {{entries}}\n\
         - glob:         {{pattern, path?, max_entries?}} -> {{matches}}\n\
                         (find by path pattern: '**/*.rs', 'src/**/test_*.py')\n\
         - search_files: {{pattern, path?, max_matches?, glob?}} -> {{matches}}\n\
                         (grep file CONTENT; use `glob` instead when you only need names)\n\
         - edit_file:    {{path, old_string, new_string, replace_all?}} or {{path, edits:[...]}} -> {{bytes_delta}}\n\
         - write_file:   {{path, content}} -> {{path, bytes_written}}  (use ONLY for new files / full rewrites)\n\
         - apply_patch:  {{diff, cwd?, strip?}} -> {{files_patched, hunks_failed}}  (unified diff across files)\n\
         - move_file:    {{from, to}} -> {{moved}}\n\
         - delete_file:  {{path, recursive?}} -> {{deleted}}\n\
         - git_status:   {{cwd?}} -> {{porcelain}}\n\
         - git_diff:     {{paths?, cwd?}} -> {{diff}}\n\
         - shell_command:{{program, args, cwd_relative?, timeout_secs?}} -> {{exit_code, stdout, stderr}}\n\
         - shell_background:{{program, args, cwd_relative?}} -> {{handle, pid}}\n\
                         (fire-and-forget for long builds / training; non-blocking)\n\
         - shell_status: {{handle}} -> {{running, runtime_sec, exit_code?, bytes_stdout, bytes_stderr}}\n\
         - shell_output: {{handle, tail_bytes?}} -> {{stdout_tail, stderr_tail}}\n\
                         (poll latest output without blocking the background process)\n\
         - shell_stop:   {{handle, signal?}} -> {{stopped, exit_code}}\n\
         - view_image:   {{path, include_base64?, max_bytes?}} -> {{format, width, height, size_bytes, sha256, base64?}}\n\
                         (read image metadata + optional bytes for workspace images)\n\
         - plan:         {{todos:[{{content,status,activeForm?}}]}} -> {{todos}}\n\
                         (maintain a checklist across turns on multi-step tasks)\n\
         - think:        {{thought}} -> {{ack:true}}\n\
                         (pure deliberation slot; no I/O, no state change)\n\
         \n\
         Response format (one JSON object per reply, no prose, no\n\
         markdown, no backticks):\n\
         \n\
         To call a single tool:\n\
         {{\"thought\": \"<short rationale>\",\n\
          \"tool_call\": {{\"name\": \"<tool>\", \"input\": <object matching the schema>}}}}\n\
         \n\
         To batch several independent tools in one turn (saves model\n\
         round-trips when calls don't depend on each other's output):\n\
         {{\"thought\": \"<short rationale>\",\n\
          \"tool_calls\": [\n\
            {{\"name\": \"<tool>\", \"input\": <object>}},\n\
            {{\"name\": \"<tool>\", \"input\": <object>}}\n\
          ]}}\n\
         Each call is dispatched and its result returned before the next\n\
         model turn. Only batch when the inputs are independent - if call\n\
         B needs to see B's output first, use the single form.\n\
         \n\
         To finish and report the final answer:\n\
         {{\"thought\": \"<short rationale>\",\n\
          \"finish\": {{\"summary\": \"<one-line outcome the verifier should see>\"}}}}\n\
         \n\
         === Tool selection heuristics (read this before responding) ===\n\
         \n\
         LONG-RUNNING COMMANDS (>60s expected):\n\
         Use shell_background, NOT shell_command. The wrapper kills any\n\
         single foreground call that runs longer than ~1500s wall-clock,\n\
         so big builds / training / large test suites MUST be backgrounded.\n\
         Examples that need shell_background:\n\
           - `make`, `make -j`, `cargo build --release`, `cmake --build`\n\
           - `pip install` of heavy packages, `npm install` in big trees\n\
           - Training scripts (python train.py, caffe train ...)\n\
           - Long test suites, ray tracers, simulators\n\
           - Anything where you'd watch a progress bar\n\
         Pattern:\n\
           1) shell_background {{program, args}}        -> {{handle, pid}}\n\
           2) shell_status     {{handle}}                -> {{running, runtime_sec, exit_code?}}\n\
           3) shell_output     {{handle, tail_bytes:4096}} -> {{stdout_tail, stderr_tail}}\n\
              Loop 2+3 as needed. Insert `think` calls between polls to\n\
              plan next steps instead of polling tighter than ~10s.\n\
           4) shell_stop {{handle}} only if you decide to give up early.\n\
         \n\
         FILE EDITS - pick the cheapest tool that fits:\n\
           - edit_file: changing a few lines in an existing file. ~50x\n\
             cheaper than write_file for small edits. Hash-checked.\n\
           - apply_patch: coordinated changes across multiple files.\n\
             Accepts BOTH standard unified diff AND Codex starred-patch\n\
             format (*** Begin Patch / *** Update File: / *** Add File:).\n\
           - write_file: ONLY for new files or full rewrites. Using it\n\
             to change a few lines wastes tokens and risks corrupting\n\
             unrelated content.\n\
         \n\
         FILE READS - slice big files, don't dump them:\n\
           - read_file {{offset, limit}}: read lines N..M of a big file.\n\
             A 2000-line source dump costs ~10k tokens of input;\n\
             lines 400-450 costs ~50 tokens. Pick the slice.\n\
           - multi_read: 2+ files at once in one tool call. Cheaper than\n\
             N separate read_file calls because the shell round-trip is\n\
             paid once.\n\
           - glob: find files by NAME pattern ('**/*.py'). Use this\n\
             instead of search_files when you don't need content search.\n\
           - search_files: grep file CONTENT. Slower than glob; only use\n\
             when you need to find code by what it contains.\n\
         \n\
         PLANNING & DELIBERATION:\n\
           - plan: when the task has >=3 distinct steps, call plan FIRST\n\
             with the breakdown. Update statuses as you progress. The\n\
             plan survives across turns in your conversation history.\n\
           - think: pure reasoning slot - no I/O. Use when you need to\n\
             work through a problem before acting. Cheaper than a probe\n\
             tool call.\n\
         \n\
         PARALLEL CALLS (when independent):\n\
         Batch reads / status checks / unrelated edits via the parallel\n\
         tool_calls form. Saves one full model turn per extra call.\n\
         Don't batch dependent calls (e.g. read after edit) - those need\n\
         to be serialized.\n\
         \n\
         === Common pitfalls (don't do these) ===\n\
           - Running `make` via shell_command for a multi-minute build\n\
             (it will time out - use shell_background).\n\
           - Using write_file to change 3 lines (use edit_file).\n\
           - Reading a 2000-line file when you only need ~50 lines\n\
             (use offset+limit).\n\
           - Polling shell_status every 1s without a `think` between\n\
             polls (wasteful; the model burns turns on no-op probes).\n\
           - Emitting plain text without a JSON envelope when you have\n\
             more work to do (Kimetsu will nudge you to keep going).\n\
           - Calling finish before producing the artifact the verifier\n\
             expects (the verifier reads files / runs scripts; an empty\n\
             summary alone won't pass).\n\
         \n\
         Workspace paths are relative to the task's starting directory.\n\
         Begin with one tool_call (or one parallel tool_calls batch).\n\
         Do not narrate. Do not output plain prose. Always wrap in JSON."
    ));
    let mut messages = vec![system, user];

    let tool_defs = kimetsu_tool_definitions();

    let mut tool_calls_total = 0u32;
    // v0.3.4a: was `last_usage` (overwrite per turn). For multi-turn
    // agent loops that meant the chat cost meter only saw the FINAL
    // model turn's cost - under-billing the budget. Cumulative now,
    // so cost.record_turn(report.usage.cost_usd) is accurate AND the
    // cache stats reflect the full session, not just the wrap-up turn.
    let mut total_usage = crate::model::TokenUsage::default();
    let mut final_text: Option<String> = None;
    let mut stop_reason = StopReason::EndTurn;
    let mut turn = 0u32;
    // MP-13d: flag so the persistence gate fires at most once per task.
    let mut persistence_nudge_sent = false;
    // MP-17c -> MP-18: replaced one-shot self-verify with iterative goal verify.
    // verify_iterations counts how many times the model tried to finish and
    // got pushed back to verify. After MAX_VERIFY_ITERATIONS attempts we
    // accept the finish unconditionally so we never deadlock.
    let mut verify_iterations: u32 = 0;
    // MP-18: capture the latest plan tool call so the verify nudge can recap
    // it back to the model. last-write-wins; the model can re-call plan to
    // refine and we always show the most-recent version.
    let mut task_plan: Option<Value> = None;
    // MP-18: accumulate record_deviation calls across the run. Surfaced in
    // agent.done's context for downstream brain-curation flows.
    let mut recorded_deviations: Vec<Value> = Vec::new();
    // MP-17m.2: outer-loop stall heartbeat. Counts consecutive turns
    // since the model last fired a USEFUL tool call. Reset on workspace-
    // touching tool calls (shell/file/git). Pure think/plan turns or
    // turns with no tool calls increment it. When it crosses
    // STALL_WINDOW_TURNS we break out - we don't kill the trial outright;
    // we exit the loop with whatever stop_reason the model emitted and
    // let `agent.done` carry the summary.
    let mut turns_since_useful = 0u32;

    while turn < turn_budget {
        turn += 1;

        // MP-17m.2: cheap stall check before another expensive model call.
        if turns_since_useful >= STALL_WINDOW_TURNS {
            stop_reason = StopReason::Error;
            break;
        }

        let request = ModelRequest {
            messages: messages.clone(),
            tools: tool_defs.clone(),
            tool_choice: ToolChoice::Auto,
            max_output_tokens: 4096,
            temperature: 0.0,
            metadata: serde_json::json!({
                "kimetsu_mode": mode,
                "turn": turn,
                "task_preview": preview_text(task, 120),
            }),
        };

        let response = provider.complete(request)?;
        // v0.3.4a: accumulate instead of overwrite. Saturating-add so a
        // pathological provider that returns u32::MAX can't wrap into 0.
        total_usage.input_tokens = total_usage
            .input_tokens
            .saturating_add(response.usage.input_tokens);
        total_usage.output_tokens = total_usage
            .output_tokens
            .saturating_add(response.usage.output_tokens);
        total_usage.cost_usd += response.usage.cost_usd;
        total_usage.cache_creation_input_tokens = total_usage
            .cache_creation_input_tokens
            .saturating_add(response.usage.cache_creation_input_tokens);
        total_usage.cache_read_input_tokens = total_usage
            .cache_read_input_tokens
            .saturating_add(response.usage.cache_read_input_tokens);
        stop_reason = response.stop_reason.clone();

        if !response.tool_calls.is_empty() {
            messages.push(ModelMessage::assistant_tool_calls(
                response.tool_calls.clone(),
            ));

            // MP-17m.2: did this turn include at least one workspace-touching
            // tool call? If yes, reset the stall counter; if not (think/plan
            // only), increment so a long string of deliberation-only turns
            // gets caught.
            let any_useful = response.tool_calls.iter().any(|c| is_useful_tool(&c.name));
            if any_useful {
                turns_since_useful = 0;
            } else {
                turns_since_useful = turns_since_useful.saturating_add(1);
            }

            for call in response.tool_calls {
                tool_calls_total += 1;
                let name = call.name.clone();
                // MP-18: capture goal artifacts before dispatching.
                //
                // `plan` - last-write-wins record of the model's current plan.
                //   Used by the verify nudge to remind the model what it
                //   committed to.
                //
                // `record_deviation` - accumulate into recorded_deviations.
                //   Will be surfaced in agent.done so the brain pipeline can
                //   propose the `lesson_for_next_time` as a failure_pattern
                //   memory.
                if name == "plan" {
                    task_plan = Some(call.input.clone());
                } else if name == "record_deviation" {
                    recorded_deviations.push(call.input.clone());
                }
                let result_value = kimetsu_dispatch_tool(runtime, &name, call.input.clone());
                messages.push(ModelMessage::tool_result(call.id, name, result_value));
            }
            continue;
        }

        // No tool calls -> the model is trying to finish.
        //
        // MP-13d (persistence gate): on real Terminal-Bench tasks we
        // observed the model emitting `finish` after 0-1 tool calls on
        // hard tasks like make-mips-interpreter - basically giving up
        // before even trying. Per the v0.2 plan's "kimetsu wraps the
        // model with a persistent harness" promise, we reject premature
        // finishes ONCE and push the model to actually try something.
        // Second time around we let the finish through so we don't loop
        // forever on a model that's genuinely stuck.
        if tool_calls_total < min_actions_before_finish && !persistence_nudge_sent {
            persistence_nudge_sent = true;
            let persistence_guidance = if mode == "harbor" {
                "Terminal-Bench rarely accepts answers without inspection"
            } else {
                "Kimetsu should not declare coding tasks done without inspection"
            };
            let nudge = format!(
                "You have only taken {tool_calls_total} action(s) on this task. \
                 {persistence_guidance} - at minimum read the relevant files, run the verifier or \
                 reproduce the task setup before declaring done. Please \
                 keep working: emit another tool_call envelope. If you \
                 genuinely believe the task requires no shell action, \
                 explain briefly in `thought` and then emit finish on \
                 your next turn."
            );
            if let Some(text) = response.text.clone() {
                messages.push(ModelMessage::assistant_text(text));
            }
            messages.push(ModelMessage::user_text(nudge));
            continue;
        }

        // MP-18: brain-powered iterative goal verify.
        //
        // Replaces the MP-17c one-shot nudge. When the model tries to finish
        // we recap the original task instruction + the most recent plan and
        // ask two structured questions: WHAT is missing and WHERE did the
        // execution deviate? The model self-inspects the workspace (list_files
        // / cat / shell_command) and either confirms "all good" -> we accept
        // on the next finish, or identifies a deviation -> optionally calls
        // record_deviation (strict mode: required; lenient default: optional)
        // -> fixes -> finishes again.
        //
        // Capped at MAX_VERIFY_ITERATIONS = 2 so a stuck model never deadlocks.
        // The persistence gate above already covered the "didn't even try"
        // case; this loop covers the "tried but didn't actually solve it"
        // class (overfull-hbox / video-processing / install-windows-3-11
        // pattern in MP-17m).
        if verify_iterations < MAX_VERIFY_ITERATIONS && opts.self_verify_nudge_enabled {
            let strict = opts.strict_verify.unwrap_or_else(|| {
                std::env::var("KIMETSU_HARBOR_VERIFY_STRICT")
                    .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE" | "yes"))
                    .unwrap_or(false)
            });
            verify_iterations += 1;
            let nudge = render_verify_nudge(task, task_plan.as_ref(), verify_iterations, strict);
            if let Some(text) = response.text.clone() {
                messages.push(ModelMessage::assistant_text(text));
            }
            messages.push(ModelMessage::user_text(nudge));
            continue;
        }

        if let Some(text) = response.text.clone() {
            final_text = Some(text.clone());
            messages.push(ModelMessage::assistant_text(text));
        } else {
            // Empty response with no tool call: stop, surface the
            // protocol-level stop reason so MP-8 telemetry can see what
            // went wrong (refusal? max_tokens? api error?).
            final_text = None;
        }
        break;
    }

    let summary = final_text.clone().unwrap_or_else(|| {
        format!(
            "kimetsu MP-7d agent ran {turn} turn(s) without producing a final answer (stop_reason={stop_reason:?})"
        )
    });

    // v0.3.1 Phase-2: agent loop no longer emits the done frame itself.
    // Callers receive the summary + context via the returned report and
    // emit (over JSON-RPC, REPL stdout, etc.) in whatever protocol the
    // transport speaks.
    let context = serde_json::json!({
        "mode": "model_agent",
        "turns": turn,
        "tool_calls": tool_calls_total,
        "stop_reason": format!("{stop_reason:?}"),
        "input_tokens": total_usage.input_tokens,
        "output_tokens": total_usage.output_tokens,
        "cost_usd": total_usage.cost_usd,
        // v0.3.4a: surface Anthropic prompt-cache stats so callers
        // (chat REPL, future telemetry) can see whether the cache is
        // landing. cache_read growing turn-over-turn is the success
        // signal; cache_creation dominating cache_read across turns
        // means we're paying full input freight every time.
        "cache_creation_input_tokens": total_usage.cache_creation_input_tokens,
        "cache_read_input_tokens": total_usage.cache_read_input_tokens,
        "harness_version": KIMETSU_HARNESS_VERSION,
        // MP-18: surface the model's self-reflections so the brain
        // curation pipeline can propose them as memory rows.
        "verify_iterations": verify_iterations,
        "recorded_deviations": recorded_deviations,
    });

    Ok(ModelAgentReport {
        turns: turn,
        tool_calls: tool_calls_total,
        stop_reason,
        final_text,
        usage: total_usage,
        summary,
        context,
    })
}

fn preview_text(text: &str, limit: usize) -> String {
    let mut clipped: String = text.chars().take(limit).collect();
    if text.chars().count() > limit {
        clipped.push_str("...");
    }
    clipped
}

/// MP-18: construct the iterative goal-verify nudge. Recaps the task
/// instruction + (if available) the most-recent plan the model committed
/// to, asks the two structured questions, and either *suggests* or
/// *requires* a `record_deviation` call depending on `strict`.
///
/// Iteration N=1 is gentle ("first verify pass"); N=2 is firmer ("you've
/// been asked once; now really check"). After MAX_VERIFY_ITERATIONS the
/// caller stops looping entirely.
fn render_verify_nudge(task: &str, plan: Option<&Value>, iteration: u32, strict: bool) -> String {
    let task_recap = preview_text(task, 800);
    let plan_recap = match plan {
        Some(p) => {
            // The `plan` tool input is {todos:[{content,status,activeForm?}]}.
            // We render as a numbered list with statuses so the model can
            // re-read its own commitments.
            let mut out = String::from("Your most recent plan:\n");
            if let Some(todos) = p.get("todos").and_then(Value::as_array) {
                for (i, t) in todos.iter().enumerate() {
                    let content = t.get("content").and_then(Value::as_str).unwrap_or("?");
                    let status = t.get("status").and_then(Value::as_str).unwrap_or("?");
                    out.push_str(&format!("  {}. [{}] {}\n", i + 1, status, content));
                }
            } else {
                out.push_str("  (plan recorded but todos field unreadable)\n");
            }
            out
        }
        None => String::from("(no plan was recorded for this task)\n"),
    };
    let deviation_clause = if strict {
        "If anything is missing or wrong, you MUST call `record_deviation` \
         describing what is missing, where in your plan you deviated, and a \
         one-sentence generalizable lesson. THEN fix the issue and try \
         finish again."
    } else {
        "If anything is missing or wrong, fix it. If the gap is something \
         that would help future tasks (a reusable lesson), optionally call \
         `record_deviation` so it can become a brain memory. Then try \
         finish again."
    };
    let urgency = if iteration == 1 {
        "Before I accept finish:"
    } else {
        "Second verify pass - your previous finish attempt was rejected. \
         Take this seriously:"
    };
    format!(
        "{urgency}\n\
         \n\
         === Goal recap (task instruction) ===\n\
         {task_recap}\n\
         \n\
         === {plan_recap}\n\
         === Inspect, then answer two questions ===\n\
         1. WHAT IS MISSING - inspect your workspace (list_files, cat, \
            shell_command). For each artifact the task asked for, confirm \
            it exists and matches the spec.\n\
         2. WHERE YOU DEVIATED - if anything is missing, identify which \
            step of your plan (or which decision) was the divergence point.\n\
         \n\
         {deviation_clause}\n\
         \n\
         If everything checks out, emit `finish` on your next turn and we're done."
    )
}

/// MP-7d: namespace helper so the system prompt is one focused location
/// rather than scattered as `format!` calls throughout the loop. The
/// prompt is intentionally short; transport-specific wording is selected
/// by mode.
struct MessageMessage;
impl MessageMessage {
    /// MP-9 (path B): minimal role description for the agent. All format
    /// rules - envelope grammar, tool catalog, the
    /// override notice that Claude Code's internal harness tools
    /// (Monitor / PushNotification / RemoteTrigger / Bash / Edit) are
    /// NOT real here - live in the user message in `run_model_agent`,
    /// not here. The system prompt is whatever Claude Code's `-p`
    /// runtime allows; the user message is what the model reads last
    /// before responding, and that's where the authority needs to be.
    fn system_prompt_for_mode(mode: &str) -> crate::model::ModelMessage {
        let surface = match mode {
            "harbor" => "a sandboxed Linux shell inside Harbor / Terminal-Bench",
            "chat" => "tools against the user's workspace",
            "test" => "a test tool runtime",
            _ => "the Kimetsu tool runtime",
        };
        crate::model::ModelMessage {
            role: crate::model::MessageRole::System,
            content: vec![crate::model::MessageContent::Text {
                text: format!(
                    "You are Kimetsu, a coding agent driving {surface}. \
                     Follow the response format described in the user message \
                     exactly. Be concise; no narration between actions."
                ),
            }],
        }
    }
}

// =====================================================================
// MP-12: composed-tool surface for the Kimetsu harness.
//
// The bare Claude-Code agent in MP-10b had 18.75 pp accuracy advantage
// over the kimetsu wrapper. The docs/MP-11-RESULTS.md verdict traced it to
// a tool-surface gap: bare CC exposes Bash + Edit + Read + Glob + Grep
// + Write etc.; kimetsu exposed only `shell_command`. Common operations
// (read file -> edit -> verify) required 3-4 shell turns instead of one
// structured tool call.
//
// MP-12 closes the gap with first-class file/search/edit tools. Benchmark
// adapters can still route shell commands through their own ShellExecutor,
// while product surfaces use the local runtime directly.
// =====================================================================

use crate::model::ToolDefinition;

const READ_FILE_DEFAULT_MAX_LINES: u32 = 800;
const LIST_FILES_DEFAULT_MAX_DEPTH: u32 = 3;
const LIST_FILES_DEFAULT_MAX_ENTRIES: u32 = 200;
const SEARCH_FILES_DEFAULT_MAX_MATCHES: u32 = 100;
const TOOL_DEFAULT_TIMEOUT_SECS: u64 = 60;

/// MP-12: the tool surface the Kimetsu model sees. Order matters for the
/// model's first scan; put the most-used ones first (`read_file`,
/// `list_files`, `search_files`) so the catalog is read top-down.
pub fn kimetsu_tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "read_file".to_string(),
            description: "Read a UTF-8 text file from the workspace. Returns content, \
                total line count, and a `truncated` flag if the slice was capped. \
                MP-14e: use `offset` (1-based start line) and `limit` (max lines to \
                return) to read a SLICE of a large file - far cheaper in input \
                tokens than fetching the whole thing. `max_lines` (legacy) caps the \
                slice from line 1; prefer `offset`+`limit` for big files."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "offset": { "type": "integer", "description": "1-based start line. Default 1." },
                    "limit": { "type": "integer", "description": "Max lines to return from `offset`. Default 800." },
                    "max_lines": { "type": "integer", "description": "Legacy: cap on lines from line 1. Use offset+limit instead." }
                },
                "required": ["path"],
            }),
        },
        ToolDefinition {
            name: "list_files".to_string(),
            description: "List files under `path` up to `max_depth` directories deep. \
                Returns up to `max_entries` paths sorted alphabetically."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Workspace-relative root; default '.'." },
                    "max_depth": { "type": "integer", "description": "Default 3." },
                    "max_entries": { "type": "integer", "description": "Default 200." }
                },
            }),
        },
        ToolDefinition {
            name: "search_files".to_string(),
            description: "Grep for `pattern` (regex) under `path`. Returns up to \
                `max_matches` hits as {file, line, text}."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string" },
                    "path": { "type": "string", "description": "Default '.'." },
                    "max_matches": { "type": "integer", "description": "Default 100." },
                    "glob": { "type": "string", "description": "Optional filename glob, e.g. '*.py'." }
                },
                "required": ["pattern"],
            }),
        },
        ToolDefinition {
            name: "write_file".to_string(),
            description: "Create or overwrite a file at `path` with `content`. \
                Returns `bytes_written`. Use this for NEW files or FULL rewrites; \
                for changing a few lines in an existing file use edit_file instead \
                (cheaper, less likely to corrupt unrelated parts)."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"],
            }),
        },
        ToolDefinition {
            name: "edit_file".to_string(),
            description: "In-place replace `old_string` -> `new_string` in `path`. \
                `old_string` must occur exactly once unless `replace_all: true`. \
                Use this whenever you want to change a few lines in an existing \
                file - much cheaper than write_file's full rewrite. For multiple \
                edits in one call, pass `edits: [{old_string, new_string, replace_all?}, ...]` \
                instead of the single old_string/new_string pair."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "old_string": { "type": "string", "description": "Single-edit form: exact bytes to replace." },
                    "new_string": { "type": "string", "description": "Single-edit form: replacement bytes." },
                    "replace_all": { "type": "boolean", "description": "If old_string occurs more than once, replace every match. Default false." },
                    "edits": {
                        "type": "array",
                        "description": "Multi-edit form: array of {old_string, new_string, replace_all?}. Applied in order.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "old_string": { "type": "string" },
                                "new_string": { "type": "string" },
                                "replace_all": { "type": "boolean" }
                            },
                            "required": ["old_string", "new_string"],
                        }
                    }
                },
                "required": ["path"],
            }),
        },
        ToolDefinition {
            name: "apply_patch".to_string(),
            description: "Apply a diff to one or more files. Accepts EITHER \
                standard unified-diff format (`--- a/old\\n+++ b/new\\n@@ ...`) \
                OR Codex starred-patch format (MP-16b):\n  \
                  *** Begin Patch\n  \
                  *** Update File: path/to/file\n  \
                  @@ optional context\n  \
                  -old line\n  \
                  +new line\n  \
                  *** End Patch\n\
                Also supports `*** Add File: path` (followed by `+`-prefixed \
                lines for new content) and `*** Delete File: path` for \
                removals. `strip` defaults to 0 (workspace-relative paths); \
                set to 1 if your diff has `a/` / `b/` prefixes. Returns the \
                list of files patched and any hunk failures."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "diff": { "type": "string", "description": "Unified diff OR Codex starred-patch text." },
                    "cwd": { "type": "string", "description": "Workspace-relative directory to apply the patch in; default workspace root." },
                    "strip": { "type": "integer", "description": "patch -p<N> prefix-strip. Default 0 (ignored for Codex starred-patch)." }
                },
                "required": ["diff"],
            }),
        },
        ToolDefinition {
            name: "git_status".to_string(),
            description: "Run `git status --porcelain` in `cwd`. Returns the raw \
                porcelain output."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "cwd": { "type": "string", "description": "Default workspace root." }
                },
            }),
        },
        ToolDefinition {
            name: "git_diff".to_string(),
            description: "Run `git diff` (working tree vs HEAD) for the given paths. \
                If `paths` is omitted, diffs everything."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "paths": { "type": "array", "items": { "type": "string" } },
                    "cwd": { "type": "string" }
                },
            }),
        },
        ToolDefinition {
            name: "shell_command".to_string(),
            description: "Escape hatch for anything the named tools don't cover: \
                builds, tests, package installs, running the verifier. \
                Returns exit_code, stdout, stderr."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "program": { "type": "string" },
                    "args": { "type": "array", "items": { "type": "string" } },
                    "cwd_relative": { "type": "string" },
                    "timeout_secs": { "type": "integer" }
                },
                "required": ["program"],
            }),
        },
        // ---------- MP-14e additions ----------
        ToolDefinition {
            name: "glob".to_string(),
            description: "Find files whose path matches a glob pattern (cheap \
                pattern-based discovery; complements search_files which greps \
                content). Supports `**` for recursive descent. Example: \
                `{pattern: \"**/*.rs\"}`. Returns matching paths sorted by \
                modification time (most recent first) so the model sees what \
                changed most recently first."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Glob like '**/*.py' or 'src/**/test_*.go'." },
                    "path": { "type": "string", "description": "Workspace-relative root; default '.'." },
                    "max_entries": { "type": "integer", "description": "Default 200." }
                },
                "required": ["pattern"],
            }),
        },
        ToolDefinition {
            name: "multi_read".to_string(),
            description: "Read several files in a single tool call. Cheaper than \
                N separate read_file calls because the runtime handles the \
                batch natively. Each entry can carry its own offset/limit slice. \
                Returns `files: [{path, content, lines_total, truncated, error?}]`."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "paths": {
                        "type": "array",
                        "description": "Simple form: array of workspace-relative paths. Each read uses full default slice.",
                        "items": { "type": "string" }
                    },
                    "files": {
                        "type": "array",
                        "description": "Rich form: array of {path, offset?, limit?} for per-file slicing.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": { "type": "string" },
                                "offset": { "type": "integer" },
                                "limit": { "type": "integer" }
                            },
                            "required": ["path"],
                        }
                    },
                    "limit": { "type": "integer", "description": "Default per-file slice cap when using `paths`. Default 400." }
                },
            }),
        },
        ToolDefinition {
            name: "move_file".to_string(),
            description: "Rename or move a file or directory inside the workspace. \
                Refuses absolute paths, `..` traversal, and empty operands."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "from": { "type": "string", "description": "Workspace-relative source path." },
                    "to":   { "type": "string", "description": "Workspace-relative destination path." }
                },
                "required": ["from", "to"],
            }),
        },
        ToolDefinition {
            name: "delete_file".to_string(),
            description: "Delete a file or directory under the workspace. Set \
                `recursive: true` for directories. Refuses absolute paths, \
                `..` traversal, and the workspace root itself."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "recursive": { "type": "boolean", "description": "Required for non-empty directories. Default false." }
                },
                "required": ["path"],
            }),
        },
        ToolDefinition {
            name: "plan".to_string(),
            description: "Record or update a structured plan / todo list for this \
                task. The plan is echoed back in the tool result and remains in \
                the conversation history, giving you a visible checklist across \
                turns. Use it on multi-step tasks: enumerate the steps up front, \
                then update statuses as you go. Status must be one of: pending, \
                in_progress, completed."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "content": { "type": "string", "description": "Imperative step description." },
                                "status":  { "type": "string", "enum": ["pending", "in_progress", "completed"] },
                                "activeForm": { "type": "string", "description": "Optional present-continuous form for display." }
                            },
                            "required": ["content", "status"],
                        }
                    }
                },
                "required": ["todos"],
            }),
        },
        ToolDefinition {
            name: "think".to_string(),
            description: "Pure deliberation slot: pass a `thought` string, get an \
                acknowledgement back. No shell, no I/O, no state change. Use \
                this when you need to reason about what to do next without \
                burning a tool call on an action. Cheap; the thought is \
                preserved in the conversation history."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "thought": { "type": "string", "description": "Free-form reasoning, plan, or observation." }
                },
                "required": ["thought"],
            }),
        },
        // ---------- MP-18: brain-powered goal verify ----------
        ToolDefinition {
            name: "record_deviation".to_string(),
            description: "Capture a self-reflection on a gap between your output \
                and the task goal. Call this when, during the verify loop, you \
                discover something missing or wrong with your output. The \
                `lesson_for_next_time` field becomes a candidate memory for \
                future tasks (reviewed via `kimetsu brain memory review`). \
                Lenient by default - only call when there's a generalizable \
                lesson worth surfacing. When strict verify is enabled, the \
                wrapper will require this call before accepting any fix-up cycle."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "missing": {
                        "type": "string",
                        "description": "Concrete description of what the goal asks for that your output doesn't satisfy."
                    },
                    "where_deviated": {
                        "type": "string",
                        "description": "Which step of your plan (or which decision) was the divergence point."
                    },
                    "lesson_for_next_time": {
                        "type": "string",
                        "description": "Generalizable one-sentence lesson. Will be proposed as a `failure_pattern` memory if non-trivial."
                    }
                },
                "required": ["missing", "where_deviated", "lesson_for_next_time"],
            }),
        },
        // ---------- MP-16a: background shell quartet ----------
        // For long-running tasks (builds, training, multi-step compilers)
        // where a single foreground shell_command would blow the
        // claude_code provider wall-clock (MP-15a default 1500s). Pattern:
        // fire shell_background -> get handle -> poll shell_status /
        // shell_output every model turn -> shell_stop if needed. State
        // lives in /tmp/kimetsu-bg-<handle>.{meta,out,err,exitcode} so the
        // composed-shell model has continuity across tool calls.
        ToolDefinition {
            name: "shell_background".to_string(),
            description: "Spawn a long-running command in the background and \
                return a handle. The process runs detached; stdout/stderr \
                land in /tmp/kimetsu-bg-<handle>.{out,err}. Use this for \
                anything you'd otherwise want to leave running while you \
                think (compiles, training, large test suites). Poll with \
                shell_status / shell_output; reap with shell_stop or by \
                letting it exit naturally."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "program": { "type": "string" },
                    "args":    { "type": "array", "items": { "type": "string" } },
                    "cwd_relative": { "type": "string", "description": "Optional workspace-relative cwd." }
                },
                "required": ["program"],
            }),
        },
        ToolDefinition {
            name: "shell_status".to_string(),
            description: "Check whether a background process from \
                shell_background is still running. Returns \
                {running, runtime_sec, exit_code?, bytes_stdout, bytes_stderr}. \
                Non-blocking - call it freely."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "handle": { "type": "string" }
                },
                "required": ["handle"],
            }),
        },
        ToolDefinition {
            name: "shell_output".to_string(),
            description: "Fetch the latest stdout/stderr from a background \
                process. `tail_bytes` (default 8192) caps the size returned \
                so a chatty build doesn't blow the response budget. The \
                process keeps running; this is read-only."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "handle":     { "type": "string" },
                    "tail_bytes": { "type": "integer", "description": "Default 8192." }
                },
                "required": ["handle"],
            }),
        },
        ToolDefinition {
            name: "shell_stop".to_string(),
            description: "Send a signal to a background process. Default \
                SIGTERM (15); pass `signal: 9` for SIGKILL. Returns \
                {stopped, exit_code, runtime_sec}. After stop, log files \
                remain until the task ends."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "handle": { "type": "string" },
                    "signal": { "type": "integer", "description": "POSIX signal number; default 15 (SIGTERM)." }
                },
                "required": ["handle"],
            }),
        },
        // ---------- MP-16c: view_image ----------
        ToolDefinition {
            name: "view_image".to_string(),
            description: "Read an image file from the workspace and return \
                metadata plus optional base64 content. Useful when a task \
                ships PNG/JPEG/PDF/etc. as part of the workspace and you \
                need to verify size / hash / dimensions, or pipe the bytes \
                to a script. By default returns metadata only; set \
                `include_base64: true` to also return the bytes (capped at \
                `max_bytes`, default 256KB)."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path":           { "type": "string" },
                    "include_base64": { "type": "boolean", "description": "Default false (metadata only)." },
                    "max_bytes":      { "type": "integer", "description": "Cap when include_base64=true; default 262144." }
                },
                "required": ["path"],
            }),
        },
    ]
}

/// MP-12: route a model-emitted tool call to its composed-shell impl.
/// Returns a structured JSON value the agent loop hands back as a
/// tool_result message. Unknown tool names get an `error` object so
/// the model can self-correct without aborting the run.
pub fn kimetsu_dispatch_tool(
    runtime: &mut crate::tools::ToolRuntime,
    name: &str,
    input: Value,
) -> Value {
    match name {
        "read_file" => kimetsu_read_file(runtime, &input),
        "list_files" => kimetsu_list_files(runtime, &input),
        "search_files" => kimetsu_search_files(runtime, &input),
        "write_file" => kimetsu_write_file(runtime, &input),
        "edit_file" => kimetsu_edit_file(runtime, &input),
        "apply_patch" => kimetsu_apply_patch(runtime, &input),
        "git_status" => kimetsu_git_status(runtime, &input),
        "git_diff" => kimetsu_git_diff(runtime, &input),
        "shell_command" => kimetsu_shell_command(runtime, &input),
        // MP-14e additions
        "glob" => kimetsu_glob(runtime, &input),
        "multi_read" => kimetsu_multi_read(runtime, &input),
        "move_file" => kimetsu_move_file(runtime, &input),
        "delete_file" => kimetsu_delete_file(runtime, &input),
        "plan" => kimetsu_plan(&input),
        "think" => kimetsu_think(&input),
        // MP-16 additions
        "shell_background" => kimetsu_shell_background(runtime, &input),
        "shell_status" => kimetsu_shell_status(runtime, &input),
        "shell_output" => kimetsu_shell_output(runtime, &input),
        "shell_stop" => kimetsu_shell_stop(runtime, &input),
        "view_image" => kimetsu_view_image(runtime, &input),
        // MP-18: brain-powered goal verify
        "record_deviation" => kimetsu_record_deviation(&input),
        other => json!({
            "error": format!(
                "unsupported tool `{other}`; pick one of \
                 read_file / list_files / search_files / write_file / \
                 edit_file / apply_patch / git_status / git_diff / shell_command / \
                 glob / multi_read / move_file / delete_file / plan / think / \
                 shell_background / shell_status / shell_output / shell_stop / \
                 view_image / record_deviation"
            ),
        }),
    }
}

fn input_str<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input.get(key).and_then(Value::as_str)
}
fn input_u32(input: &Value, key: &str, default: u32) -> u32 {
    input
        .get(key)
        .and_then(Value::as_u64)
        .map(|n| n as u32)
        .unwrap_or(default)
}

fn run_shell(
    runtime: &mut crate::tools::ToolRuntime,
    program: &str,
    args: Vec<String>,
    cwd_relative: Option<String>,
    timeout_secs: Option<u64>,
) -> Result<crate::tools::ShellCommandOutput, String> {
    let spec = CommandSpec {
        program: program.to_string(),
        args,
        cwd_relative,
        timeout_secs: Some(timeout_secs.unwrap_or(TOOL_DEFAULT_TIMEOUT_SECS)),
        expected_exit: None,
    };
    runtime.shell_command(spec).map_err(|e| e.to_string())
}

fn kimetsu_read_file(runtime: &mut crate::tools::ToolRuntime, input: &Value) -> Value {
    let Some(path) = input_str(input, "path") else {
        return json!({ "error": "read_file requires `path`" });
    };
    // MP-14e: offset/limit semantics, with backwards-compat to max_lines.
    // - explicit `offset`/`limit` wins
    // - else fall back to max_lines from line 1
    // - both defaults: 1..=800
    let has_slice = input.get("offset").is_some() || input.get("limit").is_some();
    let offset = input_u32(input, "offset", 1).max(1);
    let limit = if has_slice {
        input_u32(input, "limit", READ_FILE_DEFAULT_MAX_LINES)
    } else {
        input_u32(input, "max_lines", READ_FILE_DEFAULT_MAX_LINES)
    };

    match runtime.read_file_lines(ReadFileLinesInput {
        path: path.to_string(),
        offset,
        limit,
    }) {
        Ok(output) => serde_json::to_value(output).unwrap_or_else(
            |err| json!({ "error": format!("read_file serialization failed: {err}") }),
        ),
        Err(err) => json!({
            "error": err.to_string(),
            "path": path,
        }),
    }
}

fn kimetsu_list_files(runtime: &mut crate::tools::ToolRuntime, input: &Value) -> Value {
    let path = input_str(input, "path").unwrap_or(".");
    let max_depth = input_u32(input, "max_depth", LIST_FILES_DEFAULT_MAX_DEPTH);
    let max_entries = input_u32(input, "max_entries", LIST_FILES_DEFAULT_MAX_ENTRIES);

    let listed = match runtime.list_files(ListFilesInput {
        dir: Some(path.to_string()),
        depth: Some(max_depth),
        glob: None,
    }) {
        Ok(listed) => listed,
        Err(err) => return json!({ "error": err.to_string(), "path": path }),
    };
    let mut entries: Vec<String> = listed
        .entries
        .into_iter()
        .filter(|entry| entry.kind == "file")
        .map(|entry| entry.path)
        .collect();
    entries.sort();
    let total = entries.len() as u32;
    let truncated = total > max_entries;
    entries.truncate(max_entries as usize);
    json!({
        "path": path,
        "max_depth": max_depth,
        "entries": entries,
        "entries_returned": entries.len() as u32,
        "entries_total": total,
        "truncated": truncated,
    })
}

fn kimetsu_search_files(runtime: &mut crate::tools::ToolRuntime, input: &Value) -> Value {
    let Some(pattern) = input_str(input, "pattern") else {
        return json!({ "error": "search_files requires `pattern`" });
    };
    let path = input_str(input, "path").unwrap_or(".");
    let max_matches = input_u32(input, "max_matches", SEARCH_FILES_DEFAULT_MAX_MATCHES);
    let glob = input_str(input, "glob");

    // Use grep -rnE; --include for globs; head -n for cap. -I skips binary.
    let mut cmd = String::from("grep -rnEI ");
    if let Some(g) = glob {
        cmd.push_str(&format!("--include={} ", shell_quote(g)));
    }
    cmd.push_str(&shell_quote(pattern));
    cmd.push(' ');
    cmd.push_str(&shell_quote(path));
    cmd.push_str(&format!(" 2>/dev/null | head -n {max_matches} || true"));

    let out = match run_shell(
        runtime,
        "bash",
        vec!["-c".into(), cmd],
        None,
        Some(TOOL_DEFAULT_TIMEOUT_SECS),
    ) {
        Ok(o) => o,
        Err(e) => return json!({ "error": format!("search_files shell failed: {e}") }),
    };
    let matches: Vec<Value> = out
        .stdout_summary
        .lines()
        .filter_map(parse_grep_line)
        .collect();
    json!({
        "pattern": pattern,
        "path": path,
        "matches": matches,
        "matches_returned": matches.len() as u32,
        "truncated": matches.len() as u32 >= max_matches,
    })
}

fn kimetsu_write_file(runtime: &mut crate::tools::ToolRuntime, input: &Value) -> Value {
    let Some(path) = input_str(input, "path") else {
        return json!({ "error": "write_file requires `path`" });
    };
    let Some(content) = input_str(input, "content") else {
        return json!({ "error": "write_file requires `content`" });
    };
    // Heredoc with a fixed sentinel; base64 the content so any content
    // is safe (no need to escape backticks, $, etc.).
    let encoded = base64_encode(content.as_bytes());
    let cmd = format!(
        "set -e; mkdir -p \"$(dirname -- {0})\"; echo {1} | base64 -d > {0}; wc -c < {0}",
        shell_quote(path),
        shell_quote(&encoded)
    );
    let out = match run_shell(
        runtime,
        "bash",
        vec!["-c".into(), cmd],
        None,
        Some(TOOL_DEFAULT_TIMEOUT_SECS),
    ) {
        Ok(o) => o,
        Err(e) => return json!({ "error": format!("write_file shell failed: {e}") }),
    };
    if out.exit_code != 0 {
        return json!({
            "error": format!("write_file: exit {}; stderr: {}", out.exit_code, truncate(&out.stderr_summary, 400)),
            "path": path,
        });
    }
    let bytes_written: u64 = out.stdout_summary.trim().parse().unwrap_or(0);
    json!({
        "path": path,
        "bytes_written": bytes_written,
        "duration_ms": out.duration_ms,
    })
}

/// MP-14a: in-place replace `old_string` -> `new_string` in `path`.
///
/// CC's Edit semantics: `old_string` must occur exactly once in the
/// file unless `replace_all = true`. The file is read, transformed
/// in Rust (avoiding shell quoting issues for arbitrary content),
/// and written back via the same base64 heredoc path as write_file
/// (so binaries / weird quotes stay safe).
///
/// Cheaper than write_file when the model only wants to change a
/// few lines: the model sends two short strings instead of the whole
/// file content, and the wire-format saves output tokens
/// proportional to file size.
fn kimetsu_edit_file(runtime: &mut crate::tools::ToolRuntime, input: &Value) -> Value {
    let Some(path) = input_str(input, "path") else {
        return json!({ "error": "edit_file requires `path`" });
    };
    // Single-edit shape: {path, old_string, new_string, replace_all?}.
    // Multi-edit shape: {path, edits: [{old_string, new_string, replace_all?}, ...]}.
    let edits: Vec<EditOp> = if let Some(arr) = input.get("edits").and_then(Value::as_array) {
        let mut ops = Vec::with_capacity(arr.len());
        for (idx, e) in arr.iter().enumerate() {
            let Some(old) = e.get("old_string").and_then(Value::as_str) else {
                return json!({ "error": format!("edit_file: edits[{idx}].old_string required") });
            };
            let Some(new) = e.get("new_string").and_then(Value::as_str) else {
                return json!({ "error": format!("edit_file: edits[{idx}].new_string required") });
            };
            let replace_all = e
                .get("replace_all")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            ops.push(EditOp {
                old: old.to_string(),
                new: new.to_string(),
                replace_all,
            });
        }
        ops
    } else {
        let Some(old) = input_str(input, "old_string") else {
            return json!({ "error": "edit_file requires `old_string` or `edits` array" });
        };
        let Some(new) = input_str(input, "new_string") else {
            return json!({ "error": "edit_file requires `new_string` or `edits` array" });
        };
        let replace_all = input
            .get("replace_all")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        vec![EditOp {
            old: old.to_string(),
            new: new.to_string(),
            replace_all,
        }]
    };

    // Read the file via `cat | base64` so we can round-trip arbitrary
    // content (binary-safe). The base64 wrapper means the model's
    // edit strings are exact-bytes against the file content the user
    // sees from `cat`.
    let read_cmd = format!(
        "set -e; if [ ! -f {0} ]; then echo '__KIMETSU_MISSING__'; exit 0; fi; base64 -w 0 {0}",
        shell_quote(path)
    );
    let read_out = match run_shell(
        runtime,
        "bash",
        vec!["-c".into(), read_cmd],
        None,
        Some(TOOL_DEFAULT_TIMEOUT_SECS),
    ) {
        Ok(o) => o,
        Err(e) => return json!({ "error": format!("edit_file read shell failed: {e}") }),
    };
    if read_out.exit_code != 0 {
        return json!({
            "error": format!("edit_file: read failed exit {}; stderr: {}", read_out.exit_code, truncate(&read_out.stderr_summary, 400)),
            "path": path,
        });
    }
    let stdout = read_out.stdout_summary.trim();
    if stdout == "__KIMETSU_MISSING__" {
        return json!({
            "error": format!("edit_file: file not found at `{path}`. To create a new file, use write_file."),
            "path": path,
        });
    }
    let original_bytes = match base64_decode(stdout) {
        Ok(b) => b,
        Err(e) => return json!({ "error": format!("edit_file: base64 decode failed: {e}") }),
    };
    // We assume UTF-8 for edits; binary files should be left alone.
    let original = match std::str::from_utf8(&original_bytes) {
        Ok(s) => s.to_string(),
        Err(_) => {
            return json!({
                "error": format!("edit_file: `{path}` is not valid UTF-8; can't safely apply string replacements. Use write_file for full rewrites."),
                "path": path,
            });
        }
    };

    let mut content = original.clone();
    let mut applied = Vec::with_capacity(edits.len());
    for (idx, op) in edits.iter().enumerate() {
        if op.old.is_empty() {
            return json!({
                "error": format!("edit_file: edits[{idx}].old_string is empty"),
            });
        }
        if op.old == op.new {
            return json!({
                "error": format!("edit_file: edits[{idx}] old_string == new_string (no-op)"),
            });
        }
        let count = content.matches(&op.old).count();
        if count == 0 {
            return json!({
                "error": format!(
                    "edit_file: edits[{idx}].old_string not found in `{path}`. Read the file first to confirm the exact text (including whitespace / line endings)."
                ),
                "path": path,
            });
        }
        if count > 1 && !op.replace_all {
            return json!({
                "error": format!(
                    "edit_file: edits[{idx}].old_string occurs {count} times in `{path}`. Add `replace_all: true` to replace every occurrence, or include surrounding context to make the match unique."
                ),
                "path": path,
                "occurrences": count,
            });
        }
        if op.replace_all {
            content = content.replace(&op.old, &op.new);
        } else {
            // Replace exactly once.
            content = content.replacen(&op.old, &op.new, 1);
        }
        applied.push(json!({
            "edit_index": idx,
            "occurrences_replaced": if op.replace_all { count } else { 1 },
        }));
    }

    // Write the new content back through the same base64 heredoc as
    // write_file. This keeps binary-safety + permission inheritance
    // identical to a fresh write_file.
    let encoded = base64_encode(content.as_bytes());
    let write_cmd = format!(
        "set -e; mkdir -p \"$(dirname -- {0})\"; echo {1} | base64 -d > {0}; wc -c < {0}",
        shell_quote(path),
        shell_quote(&encoded)
    );
    let write_out = match run_shell(
        runtime,
        "bash",
        vec!["-c".into(), write_cmd],
        None,
        Some(TOOL_DEFAULT_TIMEOUT_SECS),
    ) {
        Ok(o) => o,
        Err(e) => return json!({ "error": format!("edit_file write shell failed: {e}") }),
    };
    if write_out.exit_code != 0 {
        return json!({
            "error": format!("edit_file: write failed exit {}; stderr: {}", write_out.exit_code, truncate(&write_out.stderr_summary, 400)),
            "path": path,
        });
    }
    let bytes_written: u64 = write_out.stdout_summary.trim().parse().unwrap_or(0);
    json!({
        "path": path,
        "edits_applied": applied,
        "bytes_before": original.len() as u64,
        "bytes_after": bytes_written,
        "bytes_delta": bytes_written as i64 - original.len() as i64,
    })
}

struct EditOp {
    old: String,
    new: String,
    replace_all: bool,
}

/// MP-14b: apply a unified diff to one or more files via container
/// `patch -p<strip>`. Codex's signature operation - lets the model
/// accumulate multi-file edits in one envelope instead of one
/// edit_file call per file.
///
/// Implementation:
///   1. base64-encode the diff (so quoting is safe).
///   2. pipe `echo $b64 | base64 -d | patch -p<strip> --batch -d <cwd>`.
///   3. parse stdout for "patching file X" lines to report which
///      files actually changed.
///
/// `strip` defaults to 0 (paths in the diff are workspace-relative).
/// Use 1 if your diff has a `a/` / `b/` prefix.
fn kimetsu_apply_patch(runtime: &mut crate::tools::ToolRuntime, input: &Value) -> Value {
    let Some(diff_raw) = input_str(input, "diff") else {
        return json!({ "error": "apply_patch requires `diff` (unified-diff or Codex starred-patch text)" });
    };
    let cwd = input_str(input, "cwd").map(str::to_string);
    let mut strip = input_u32(input, "strip", 0);

    // MP-16b: detect Codex's starred-patch format and translate to
    // standard unified diff. The Codex format opens with `*** Begin
    // Patch`; the translator produces `--- a/X` / `+++ b/X` headers so
    // GNU patch accepts it, which forces strip=1 regardless of the
    // caller's `strip` arg.
    let (diff, format_used) = if diff_raw.contains("*** Begin Patch") {
        match translate_codex_patch(diff_raw) {
            Ok(translated) => {
                strip = 1;
                (translated, "codex_starred")
            }
            Err(err) => {
                return json!({
                    "error": format!("apply_patch: Codex starred-patch translation failed: {err}"),
                });
            }
        }
    } else {
        (diff_raw.to_string(), "unified")
    };

    let encoded = base64_encode(diff.as_bytes());
    let cmd = format!(
        "echo {} | base64 -d | patch -p{} --batch 2>&1",
        shell_quote(&encoded),
        strip
    );
    let out = match run_shell(
        runtime,
        "bash",
        vec!["-c".into(), cmd],
        cwd.clone(),
        Some(TOOL_DEFAULT_TIMEOUT_SECS * 2), // patches can touch many files
    ) {
        Ok(o) => o,
        Err(e) => return json!({ "error": format!("apply_patch shell failed: {e}") }),
    };

    // MP-17 #10: if GNU `patch` is not installed in this container, fall
    // back to a Rust-side unified-diff applier that re-routes each hunk
    // through kimetsu_edit_file (base64 round-trip read+edit+write through
    // the composed-shell layer). Detection: exit 127 ("command not found"
    // by convention) or stderr explicitly says so.
    let patch_not_found = out.exit_code == 127
        || out.stderr_summary.contains("command not found")
        || out.stdout_summary.contains("command not found");
    if patch_not_found {
        return kimetsu_apply_patch_rust_fallback(runtime, &diff, cwd.as_deref(), format_used);
    }

    let mut patched_files: Vec<String> = Vec::new();
    let mut hunks_failed = 0u32;
    let mut rejects: Vec<String> = Vec::new();
    for line in out.stdout_summary.lines() {
        // GNU patch output: "patching file X" / "patching file X (Y hunk(s) succeeded ...)" / "X.rej created"
        if let Some(rest) = line.strip_prefix("patching file ") {
            // Strip optional trailing " (...)" info.
            let file = rest.split_once(' ').map(|(f, _)| f).unwrap_or(rest);
            patched_files.push(file.trim().to_string());
        } else if line.contains("FAILED") {
            hunks_failed += 1;
        } else if let Some(rest) = line.strip_prefix("Hunk") {
            if rest.contains("FAILED") {
                hunks_failed += 1;
            }
        } else if line.contains(".rej") {
            rejects.push(line.to_string());
        }
    }

    if out.exit_code != 0 {
        return json!({
            "error": format!("apply_patch: patch exited {}; output: {}", out.exit_code, truncate(&out.stdout_summary, 800)),
            "format": format_used,
            "files_attempted": patched_files,
            "hunks_failed": hunks_failed,
            "rejects": rejects,
            "exit_code": out.exit_code,
        });
    }

    json!({
        "ok": true,
        "format": format_used,
        "files_patched": patched_files,
        "hunks_failed": hunks_failed,
        "rejects": rejects,
        "output_excerpt": truncate(&out.stdout_summary, 800),
    })
}

// ---------------------------------------------------------------------------
// MP-17 #10: pure-Rust unified-diff applier (fallback when patch(1) absent).
//
// Strategy: parse the diff into per-file sections + per-hunk old/new pairs,
// then re-route each hunk through kimetsu_edit_file. edit_file keeps the
// mutation behind the same workspace/path checks as the rest of the runtime,
// so we don't depend on patch(1) being installed.
//
// Limitations vs GNU patch:
//   - No fuzz matching: the reconstructed OLD must occur in the file
//     exactly once (edit_file's hash-check semantics).
//   - No --reverse / --merge mode.
//   - No automatic .rej generation; the response carries `files_failed`
//     entries so the model can self-correct.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct DiffFile {
    path: String,
    /// is_new: true if header was `--- /dev/null` (creation)
    is_new: bool,
    /// is_delete: true if header was `+++ /dev/null` (deletion)
    is_delete: bool,
    hunks: Vec<DiffHunk>,
}

#[derive(Debug, Clone)]
struct DiffHunk {
    /// Reconstructed OLD text (context + removal lines).
    old: String,
    /// Reconstructed NEW text (context + addition lines).
    new: String,
}

fn parse_unified_diff(diff: &str) -> Result<Vec<DiffFile>, String> {
    let mut files: Vec<DiffFile> = Vec::new();
    let mut iter = diff.lines().peekable();
    while let Some(line) = iter.next() {
        if !line.starts_with("--- ") {
            continue;
        }
        let minus_header = line;
        let plus_header = iter.next().unwrap_or("");
        if !plus_header.starts_with("+++ ") {
            return Err(format!(
                "expected `+++ ` after `--- `, got: {}",
                truncate(plus_header, 80)
            ));
        }
        let is_new = minus_header.trim_start_matches("--- ").trim() == "/dev/null";
        let is_delete = plus_header.trim_start_matches("+++ ").trim() == "/dev/null";
        let path = if is_delete {
            extract_diff_path(minus_header.trim_start_matches("--- "))
        } else {
            extract_diff_path(plus_header.trim_start_matches("+++ "))
        };
        let mut hunks = Vec::new();
        // Walk lines until the next `--- ` (next file) or EOF, collecting hunks.
        while let Some(next) = iter.peek() {
            if next.starts_with("--- ") {
                break;
            }
            let line = iter.next().unwrap();
            if line.starts_with("@@") {
                // Start a new hunk; collect body until next `@@` or `--- `.
                let mut old_lines = String::new();
                let mut new_lines = String::new();
                while let Some(peek) = iter.peek() {
                    if peek.starts_with("@@") || peek.starts_with("--- ") {
                        break;
                    }
                    let body = iter.next().unwrap();
                    if let Some(rest) = body.strip_prefix('-') {
                        if body.starts_with("---") {
                            break;
                        }
                        old_lines.push_str(rest);
                        old_lines.push('\n');
                    } else if let Some(rest) = body.strip_prefix('+') {
                        if body.starts_with("+++") {
                            break;
                        }
                        new_lines.push_str(rest);
                        new_lines.push('\n');
                    } else if let Some(rest) = body.strip_prefix(' ') {
                        old_lines.push_str(rest);
                        old_lines.push('\n');
                        new_lines.push_str(rest);
                        new_lines.push('\n');
                    } else if body.is_empty() {
                        old_lines.push('\n');
                        new_lines.push('\n');
                    } else if body.starts_with('\\') {
                        // "\ No newline at end of file" - skip the marker.
                    } else {
                        // Unknown line type; treat as context to be safe.
                        old_lines.push_str(body);
                        old_lines.push('\n');
                        new_lines.push_str(body);
                        new_lines.push('\n');
                    }
                }
                hunks.push(DiffHunk {
                    old: old_lines,
                    new: new_lines,
                });
            }
        }
        files.push(DiffFile {
            path,
            is_new,
            is_delete,
            hunks,
        });
    }
    if files.is_empty() {
        return Err("no per-file sections found (no `--- ` headers)".to_string());
    }
    Ok(files)
}

fn extract_diff_path(raw: &str) -> String {
    let trimmed = raw.trim();
    // Strip GNU patch's a/ or b/ prefix when present.
    if let Some(rest) = trimmed.strip_prefix("a/") {
        return rest.to_string();
    }
    if let Some(rest) = trimmed.strip_prefix("b/") {
        return rest.to_string();
    }
    trimmed.to_string()
}

fn kimetsu_apply_patch_rust_fallback(
    runtime: &mut crate::tools::ToolRuntime,
    diff: &str,
    cwd: Option<&str>,
    format_used: &'static str,
) -> Value {
    let files = match parse_unified_diff(diff) {
        Ok(f) => f,
        Err(e) => {
            return json!({
                "error": format!("apply_patch (rust fallback): diff parse failed: {e}"),
                "format": format_used,
            });
        }
    };

    let mut files_patched: Vec<String> = Vec::new();
    let mut files_failed: Vec<Value> = Vec::new();
    let mut hunks_failed = 0u32;

    for f in files {
        let full_path = match cwd {
            Some(c) if !c.is_empty() => format!("{}/{}", c.trim_end_matches('/'), f.path),
            _ => f.path.clone(),
        };

        if f.is_new {
            // Reconstruct the new-file content from the hunks' `new` text.
            let content: String = f.hunks.iter().map(|h| h.new.clone()).collect();
            let write_input = json!({ "path": full_path, "content": content });
            let res = kimetsu_write_file(runtime, &write_input);
            if res.get("error").is_some() {
                files_failed.push(json!({
                    "path": full_path,
                    "kind": "add_file",
                    "error": res["error"].clone(),
                }));
            } else {
                files_patched.push(full_path);
            }
            continue;
        }
        if f.is_delete {
            let del_input = json!({ "path": full_path, "recursive": false });
            let res = kimetsu_delete_file(runtime, &del_input);
            if res.get("error").is_some() {
                files_failed.push(json!({
                    "path": full_path,
                    "kind": "delete_file",
                    "error": res["error"].clone(),
                }));
            } else {
                files_patched.push(full_path);
            }
            continue;
        }
        // Modify case: run each hunk through edit_file in order.
        let mut file_ok = true;
        for (i, hunk) in f.hunks.iter().enumerate() {
            // Skip trailing newline difference quirks: edit_file uses exact match.
            let edit_input = json!({
                "path": full_path,
                "old_string": hunk.old.trim_end_matches('\n'),
                "new_string": hunk.new.trim_end_matches('\n'),
            });
            let res = kimetsu_edit_file(runtime, &edit_input);
            if res.get("error").is_some() {
                files_failed.push(json!({
                    "path": full_path,
                    "hunk": i,
                    "error": res["error"].clone(),
                }));
                hunks_failed += 1;
                file_ok = false;
                // Don't bail on the file - subsequent hunks may still apply.
            }
        }
        if file_ok {
            files_patched.push(full_path);
        }
    }

    if files_patched.is_empty() && !files_failed.is_empty() {
        return json!({
            "error": "apply_patch (rust fallback): no files patched cleanly",
            "format": format_used,
            "fallback": "rust",
            "files_patched": files_patched,
            "files_failed": files_failed,
            "hunks_failed": hunks_failed,
        });
    }

    json!({
        "ok": files_failed.is_empty(),
        "format": format_used,
        "fallback": "rust",
        "files_patched": files_patched,
        "files_failed": files_failed,
        "hunks_failed": hunks_failed,
    })
}

fn kimetsu_git_status(runtime: &mut crate::tools::ToolRuntime, input: &Value) -> Value {
    let cwd = input_str(input, "cwd").map(str::to_string);
    let out = match run_shell(
        runtime,
        "git",
        vec!["status".into(), "--porcelain".into()],
        cwd,
        Some(TOOL_DEFAULT_TIMEOUT_SECS),
    ) {
        Ok(o) => o,
        Err(e) => return json!({ "error": format!("git_status shell failed: {e}") }),
    };
    if out.exit_code != 0 {
        return json!({
            "error": format!("git_status: exit {}; stderr: {}", out.exit_code, truncate(&out.stderr_summary, 400)),
        });
    }
    json!({
        "porcelain": out.stdout_summary,
        "clean": out.stdout_summary.trim().is_empty(),
    })
}

fn kimetsu_git_diff(runtime: &mut crate::tools::ToolRuntime, input: &Value) -> Value {
    let cwd = input_str(input, "cwd").map(str::to_string);
    let mut args = vec!["diff".to_string()];
    if let Some(arr) = input.get("paths").and_then(Value::as_array) {
        args.push("--".to_string());
        for p in arr {
            if let Some(s) = p.as_str() {
                args.push(s.to_string());
            }
        }
    }
    let out = match run_shell(runtime, "git", args, cwd, Some(TOOL_DEFAULT_TIMEOUT_SECS)) {
        Ok(o) => o,
        Err(e) => return json!({ "error": format!("git_diff shell failed: {e}") }),
    };
    if out.exit_code != 0 {
        return json!({
            "error": format!("git_diff: exit {}; stderr: {}", out.exit_code, truncate(&out.stderr_summary, 400)),
        });
    }
    json!({
        "diff": out.stdout_summary,
        "empty": out.stdout_summary.trim().is_empty(),
    })
}

fn kimetsu_shell_command(runtime: &mut crate::tools::ToolRuntime, input: &Value) -> Value {
    let spec_value = input.clone();
    match serde_json::from_value::<CommandSpec>(spec_value.clone()) {
        Ok(spec) => match runtime.shell_command(spec) {
            Ok(output) => serde_json::to_value(output)
                .unwrap_or_else(|err| json!({ "error": err.to_string() })),
            Err(err) => json!({ "error": format!("shell_command failed: {err}") }),
        },
        Err(err) => json!({
            "error": format!("invalid shell_command input: {err}; got {spec_value}"),
        }),
    }
}

// ---------------------------------------------------------------------------
// MP-14e: pattern file-find, batch read, typed move/delete, plan, think.
// All six exist to cut per-turn cost or per-turn waste vs composing the same
// thing out of raw shell_command calls.
// ---------------------------------------------------------------------------

const GLOB_DEFAULT_MAX_ENTRIES: u32 = 200;
const MULTI_READ_DEFAULT_LIMIT: u32 = 400;
const MULTI_READ_MAX_FILES: usize = 25;

/// MP-14e: glob - pattern-based file discovery (complement to search_files's
/// content grep). Maps the glob to `find -path`. `**` is rewritten to `*`
/// because POSIX find doesn't grok `**` natively; we still recurse because
/// `find` recurses by default unless `-maxdepth` says otherwise.
fn kimetsu_glob(runtime: &mut crate::tools::ToolRuntime, input: &Value) -> Value {
    let Some(pattern) = input_str(input, "pattern") else {
        return json!({ "error": "glob requires `pattern`" });
    };
    let path = input_str(input, "path").unwrap_or(".");
    let max_entries = input_u32(input, "max_entries", GLOB_DEFAULT_MAX_ENTRIES);
    // POSIX find treats `**` as `*/*` rather than recursive-everything. We
    // recurse by default; collapse `**` to `*` for matching purposes.
    let find_pattern = pattern.replace("**", "*");
    // Use `find ... -path '*<pat>'` so a relative pattern like '*.rs' still
    // matches files in nested directories.
    let path_pattern = if find_pattern.starts_with('/') || find_pattern.starts_with("./") {
        find_pattern.clone()
    } else {
        format!("*/{find_pattern}")
    };
    // -printf '%T@ %p\n' gives mtime-prefixed paths for sort -nr.
    let cmd = format!(
        "find {0} -type f \\( -path {1} -o -path {2} \\) -printf '%T@ %p\\n' 2>/dev/null \
         | sort -nr | head -n {3} | cut -d' ' -f2-",
        shell_quote(path),
        shell_quote(&path_pattern),
        shell_quote(&find_pattern),
        max_entries
    );
    let out = match run_shell(
        runtime,
        "bash",
        vec!["-c".into(), cmd],
        None,
        Some(TOOL_DEFAULT_TIMEOUT_SECS),
    ) {
        Ok(o) => o,
        Err(e) => return json!({ "error": format!("glob shell failed: {e}") }),
    };
    let matches: Vec<&str> = out
        .stdout_summary
        .lines()
        .filter(|l| !l.is_empty())
        .collect();
    let returned = matches.len() as u32;
    json!({
        "pattern": pattern,
        "path": path,
        "matches": matches,
        "matches_returned": returned,
        "truncated": returned >= max_entries,
    })
}

/// MP-14e: multi_read - batch N file reads in one tool call. Accepts either
/// `paths: ["a", "b"]` (each with the default slice) or `files: [{path,
/// offset?, limit?}]` (per-file slice). Caps at MULTI_READ_MAX_FILES to keep
/// any single result envelope bounded.
fn kimetsu_multi_read(runtime: &mut crate::tools::ToolRuntime, input: &Value) -> Value {
    #[derive(Debug)]
    struct Req {
        path: String,
        offset: u32,
        limit: u32,
    }

    let default_limit = input_u32(input, "limit", MULTI_READ_DEFAULT_LIMIT);
    let mut reqs: Vec<Req> = Vec::new();

    if let Some(arr) = input.get("paths").and_then(Value::as_array) {
        for v in arr {
            if let Some(p) = v.as_str() {
                reqs.push(Req {
                    path: p.to_string(),
                    offset: 1,
                    limit: default_limit,
                });
            }
        }
    }
    if let Some(arr) = input.get("files").and_then(Value::as_array) {
        for v in arr {
            let Some(path) = v.get("path").and_then(Value::as_str) else {
                continue;
            };
            let offset = v
                .get("offset")
                .and_then(Value::as_u64)
                .map(|n| n as u32)
                .unwrap_or(1)
                .max(1);
            let limit = v
                .get("limit")
                .and_then(Value::as_u64)
                .map(|n| n as u32)
                .unwrap_or(default_limit);
            reqs.push(Req {
                path: path.to_string(),
                offset,
                limit,
            });
        }
    }
    if reqs.is_empty() {
        return json!({ "error": "multi_read requires `paths` or `files`" });
    }
    if reqs.len() > MULTI_READ_MAX_FILES {
        return json!({
            "error": format!(
                "multi_read accepts at most {MULTI_READ_MAX_FILES} files per call (got {})",
                reqs.len()
            )
        });
    }

    let files = reqs
        .into_iter()
        .map(|req| ReadFileLinesInput {
            path: req.path,
            offset: req.offset,
            limit: req.limit,
        })
        .collect();

    match runtime.multi_read_lines(MultiReadLinesInput { files }) {
        Ok(output) => serde_json::to_value(output).unwrap_or_else(
            |err| json!({ "error": format!("multi_read serialization failed: {err}") }),
        ),
        Err(err) => json!({ "error": err.to_string() }),
    }
}

/// MP-14e: move_file - typed rename inside the workspace. Refuses absolute
/// paths and `..` traversal so a typo can't escape the sandbox.
fn kimetsu_move_file(runtime: &mut crate::tools::ToolRuntime, input: &Value) -> Value {
    let Some(from) = input_str(input, "from") else {
        return json!({ "error": "move_file requires `from`" });
    };
    let Some(to) = input_str(input, "to") else {
        return json!({ "error": "move_file requires `to`" });
    };
    if let Err(err) = check_workspace_path("from", from) {
        return json!({ "error": err });
    }
    if let Err(err) = check_workspace_path("to", to) {
        return json!({ "error": err });
    }
    let out = match run_shell(
        runtime,
        "mv",
        vec![from.to_string(), to.to_string()],
        None,
        Some(TOOL_DEFAULT_TIMEOUT_SECS),
    ) {
        Ok(o) => o,
        Err(e) => return json!({ "error": format!("move_file shell failed: {e}") }),
    };
    if out.exit_code != 0 {
        return json!({
            "error": format!("move_file: exit {}; stderr: {}", out.exit_code, truncate(&out.stderr_summary, 400)),
            "from": from,
            "to": to,
        });
    }
    json!({ "moved": true, "from": from, "to": to })
}

/// MP-14e: delete_file - typed remove inside the workspace. `recursive: true`
/// is required for non-empty directories. Refuses workspace root, absolute
/// paths, and `..` traversal.
fn kimetsu_delete_file(runtime: &mut crate::tools::ToolRuntime, input: &Value) -> Value {
    let Some(path) = input_str(input, "path") else {
        return json!({ "error": "delete_file requires `path`" });
    };
    if path == "." || path == "./" {
        return json!({ "error": "delete_file: refusing to delete the workspace root" });
    }
    if let Err(err) = check_workspace_path("path", path) {
        return json!({ "error": err });
    }
    let recursive = input
        .get("recursive")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut args: Vec<String> = Vec::with_capacity(2);
    if recursive {
        args.push("-rf".to_string());
    } else {
        args.push("-f".to_string());
    }
    args.push(path.to_string());
    let out = match run_shell(runtime, "rm", args, None, Some(TOOL_DEFAULT_TIMEOUT_SECS)) {
        Ok(o) => o,
        Err(e) => return json!({ "error": format!("delete_file shell failed: {e}") }),
    };
    if out.exit_code != 0 {
        return json!({
            "error": format!("delete_file: exit {}; stderr: {}", out.exit_code, truncate(&out.stderr_summary, 400)),
            "path": path,
        });
    }
    json!({ "deleted": true, "path": path, "recursive": recursive })
}

/// MP-14e: plan - record a todo list. We don't persist it server-side; the
/// tool simply validates + echoes it. The fact that the result sits in the
/// conversation history is enough for the model to "see" the plan on every
/// subsequent turn. Status must be pending|in_progress|completed.
fn kimetsu_plan(input: &Value) -> Value {
    let Some(todos) = input.get("todos").and_then(Value::as_array) else {
        return json!({ "error": "plan requires `todos: [{content,status,activeForm?}]`" });
    };
    let mut normalized: Vec<Value> = Vec::with_capacity(todos.len());
    let mut counts = (0u32, 0u32, 0u32); // (pending, in_progress, completed)
    for (i, item) in todos.iter().enumerate() {
        let Some(content) = item.get("content").and_then(Value::as_str) else {
            return json!({ "error": format!("plan: todo[{i}] missing `content`") });
        };
        let Some(status) = item.get("status").and_then(Value::as_str) else {
            return json!({ "error": format!("plan: todo[{i}] missing `status`") });
        };
        match status {
            "pending" => counts.0 += 1,
            "in_progress" => counts.1 += 1,
            "completed" => counts.2 += 1,
            other => {
                return json!({
                    "error": format!(
                        "plan: todo[{i}] status={other:?}; must be pending|in_progress|completed"
                    )
                });
            }
        }
        let active_form = item
            .get("activeForm")
            .and_then(Value::as_str)
            .unwrap_or(content);
        normalized.push(json!({
            "content": content,
            "status": status,
            "activeForm": active_form,
        }));
    }
    json!({
        "todos": normalized,
        "pending": counts.0,
        "in_progress": counts.1,
        "completed": counts.2,
        "total": normalized.len() as u32,
    })
}

/// MP-14e: think - pure ack. No shell, no I/O. The thought lives in the
/// conversation history; that's the entire point.
fn kimetsu_think(input: &Value) -> Value {
    let Some(thought) = input_str(input, "thought") else {
        return json!({ "error": "think requires `thought`" });
    };
    json!({
        "ack": true,
        "thought_len": thought.len() as u32,
    })
}

/// MP-18: brain-powered goal verify - capture the model's self-reflection
/// on a gap between its output and the task goal. The handler validates +
/// echoes the input back. The agent loop sweeps `record_deviation` calls
/// out of the tool-call stream and accumulates them in agent state;
/// they're surfaced in `agent.done` at task end and become candidate
/// memory proposals (kind=failure_pattern) via the existing brain
/// curation flow.
fn kimetsu_record_deviation(input: &Value) -> Value {
    let Some(missing) = input_str(input, "missing") else {
        return json!({ "error": "record_deviation requires `missing`" });
    };
    let Some(where_deviated) = input_str(input, "where_deviated") else {
        return json!({ "error": "record_deviation requires `where_deviated`" });
    };
    let Some(lesson) = input_str(input, "lesson_for_next_time") else {
        return json!({ "error": "record_deviation requires `lesson_for_next_time`" });
    };
    if missing.trim().is_empty() || where_deviated.trim().is_empty() || lesson.trim().is_empty() {
        return json!({ "error": "record_deviation: all three fields must be non-empty" });
    }
    json!({
        "recorded": true,
        "missing": missing,
        "where_deviated": where_deviated,
        "lesson_for_next_time": lesson,
    })
}

/// MP-17j (tuned in MP-17m.1, superseded by MP-18): cheap keyword check
/// on the task description.
///
/// MP-18 replaced the conditional self-verify nudge with always-on
/// iterative goal verify, so this helper is no longer called from the
/// agent loop. Kept under `#[allow(dead_code)]` because the test suite
/// still exercises it as a documented helper, and we may want to
/// re-enable it as an optimization later (e.g. "skip the verify loop
/// on tasks where keywords say the task is verification-implicit").
///
/// MP-17m.1: the initial trigger list was too eager. Words like "ensure
/// that", "check that", "i will check" appear in generic task instructions
/// where verification IS the task, not an extra step. New triggers are
/// scoped to: explicit verifier identifiers (`pytest`, `cargo test`,
/// `make test`), explicit deliverable specifications ("should output",
/// "should print", "should return"), and pass/fail language tied to a
/// clear test step ("must pass", "should pass").
#[allow(dead_code)]
fn task_signals_verification(task: &str) -> bool {
    let lower = task.to_ascii_lowercase();
    const HINTS: &[&str] = &[
        // explicit verifier identifier
        "verifier",
        "pytest",
        "cargo test",
        "make test",
        "test suite",
        "run the test",
        // explicit deliverable specification with a concrete word
        "should output",
        "should print",
        "should return",
        "should produce",
        // pass/fail tied to a test
        "must pass",
        "should pass",
        // very specific verifier mention
        "the verifier",
    ];
    HINTS.iter().any(|h| lower.contains(h))
}

/// Workspace-path safety: workspace-relative, no absolute prefix, no `..`
/// segment. Used by move_file / delete_file to keep a typo from reaching
/// outside the task sandbox.
fn check_workspace_path(label: &str, p: &str) -> Result<(), String> {
    if p.is_empty() {
        return Err(format!("`{label}` must not be empty"));
    }
    if p.starts_with('/') {
        return Err(format!(
            "`{label}` must be workspace-relative, not absolute"
        ));
    }
    for seg in p.split('/') {
        if seg == ".." {
            return Err(format!("`{label}` may not contain `..` segments"));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// MP-16b: translate Codex starred-patch -> standard unified diff.
//
// Codex grammar:
//   *** Begin Patch
//   *** Update File: <path>
//   @@ optional anchor
//   -old
//   +new
//   *** Add File: <path>
//   +line
//   +line
//   *** Delete File: <path>
//   *** End Patch
//
// We translate each per-file section into:
//   --- a/<path>
//   +++ b/<path>
//   <hunk>
// then GNU patch consumes it with -p1 (a/ / b/ prefixes stripped).
//
// This is intentionally permissive - Codex's emitted diffs vary in
// whether they include `@@` markers, blank lines, etc. We synthesize a
// generic `@@ -0,0 +1,N @@` for Add File and `@@ -1,N +0,0 @@` for
// Delete File so GNU patch is happy.
fn translate_codex_patch(raw: &str) -> Result<String, String> {
    let mut out = String::new();
    let mut current: Option<CodexSection> = None;
    let mut saw_begin = false;
    let mut saw_end = false;

    fn flush(section: CodexSection, out: &mut String) -> Result<(), String> {
        match section {
            CodexSection::Update { path, body } => {
                out.push_str(&format!("--- a/{path}\n+++ b/{path}\n"));
                // If body doesn't include any @@ line, GNU patch in --batch
                // mode still accepts it as a flat hunk if we prepend one.
                // We compute crude counts so the hunk header isn't a lie.
                let has_hunk = body.lines().any(|l| l.starts_with("@@"));
                if !has_hunk {
                    let (minus, plus) = body.lines().fold((0u32, 0u32), |(m, p), l| {
                        if l.starts_with('-') && !l.starts_with("---") {
                            (m + 1, p)
                        } else if l.starts_with('+') && !l.starts_with("+++") {
                            (m, p + 1)
                        } else if !l.is_empty() {
                            (m + 1, p + 1) // context line counted both sides
                        } else {
                            (m, p)
                        }
                    });
                    out.push_str(&format!("@@ -1,{minus} +1,{plus} @@\n"));
                }
                out.push_str(&body);
                if !body.ends_with('\n') {
                    out.push('\n');
                }
            }
            CodexSection::Add { path, body } => {
                let plus = body.lines().count() as u32;
                out.push_str(&format!("--- /dev/null\n+++ b/{path}\n"));
                out.push_str(&format!("@@ -0,0 +1,{plus} @@\n"));
                // Body lines should already be prefixed with `+`. If not,
                // add the prefix so GNU patch reads them as additions.
                for line in body.lines() {
                    if line.starts_with('+') {
                        out.push_str(line);
                    } else {
                        out.push('+');
                        out.push_str(line);
                    }
                    out.push('\n');
                }
            }
            CodexSection::Delete { path } => {
                // Best-effort delete header. We don't know the original
                // line count; GNU patch with --batch accepts this form
                // and removes the file outright when `--remove-empty-files`
                // could be set, but to stay safe we emit a single-line
                // negative hunk.
                out.push_str(&format!("--- a/{path}\n+++ /dev/null\n"));
                out.push_str("@@ -1,1 +0,0 @@\n-\n");
            }
        }
        Ok(())
    }

    for line in raw.lines() {
        if line.starts_with("*** Begin Patch") {
            saw_begin = true;
            continue;
        }
        if line.starts_with("*** End Patch") {
            saw_end = true;
            if let Some(sec) = current.take() {
                flush(sec, &mut out)?;
            }
            break;
        }
        if let Some(rest) = line.strip_prefix("*** Update File: ") {
            if let Some(sec) = current.take() {
                flush(sec, &mut out)?;
            }
            current = Some(CodexSection::Update {
                path: rest.trim().to_string(),
                body: String::new(),
            });
            continue;
        }
        if let Some(rest) = line.strip_prefix("*** Add File: ") {
            if let Some(sec) = current.take() {
                flush(sec, &mut out)?;
            }
            current = Some(CodexSection::Add {
                path: rest.trim().to_string(),
                body: String::new(),
            });
            continue;
        }
        if let Some(rest) = line.strip_prefix("*** Delete File: ") {
            if let Some(sec) = current.take() {
                flush(sec, &mut out)?;
            }
            current = Some(CodexSection::Delete {
                path: rest.trim().to_string(),
            });
            continue;
        }
        if let Some(sec) = current.as_mut() {
            match sec {
                CodexSection::Update { body, .. } | CodexSection::Add { body, .. } => {
                    body.push_str(line);
                    body.push('\n');
                }
                CodexSection::Delete { .. } => { /* drop trailing lines for Delete */ }
            }
        }
    }

    if !saw_begin {
        return Err("missing `*** Begin Patch` header".to_string());
    }
    if !saw_end {
        // Permissive: if `*** End Patch` was forgotten, flush whatever
        // section was open. Don't outright fail.
        if let Some(sec) = current.take() {
            flush(sec, &mut out)?;
        }
    }
    if out.is_empty() {
        return Err("no per-file sections found".to_string());
    }
    Ok(out)
}

enum CodexSection {
    Update { path: String, body: String },
    Add { path: String, body: String },
    Delete { path: String },
}

// ---------------------------------------------------------------------------
// MP-16a: background shell quartet.
//
// State lives in /tmp for the active shell backend, since each tool call is
// a fresh shell invocation from our Rust process. Per-handle layout:
//
//   /tmp/kimetsu-bg-<handle>.meta      JSON {handle, pid, started_at_sec,
//                                            program, cwd}
//   /tmp/kimetsu-bg-<handle>.out       stdout drain
//   /tmp/kimetsu-bg-<handle>.err       stderr drain
//   /tmp/kimetsu-bg-<handle>.exitcode  written when the process exits
//
// Handles look like `bg-<epoch_ns>-<rand4>` and are opaque to the model.
// `setsid` detaches the spawn from the parent's process group so killing
// our own subprocess doesn't reap the background tree.

const BG_HANDLE_PREFIX: &str = "bg-";

fn validate_bg_handle(handle: &str) -> Result<(), String> {
    if !handle.starts_with(BG_HANDLE_PREFIX) {
        return Err(format!(
            "invalid handle: expected `{BG_HANDLE_PREFIX}*`, got {handle:?}"
        ));
    }
    if handle.len() > 128
        || handle
            .chars()
            .any(|c| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
    {
        return Err(format!("invalid handle characters: {handle:?}"));
    }
    Ok(())
}

fn kimetsu_shell_background(runtime: &mut crate::tools::ToolRuntime, input: &Value) -> Value {
    let Some(program) = input_str(input, "program") else {
        return json!({ "error": "shell_background requires `program`" });
    };
    let args: Vec<String> = input
        .get("args")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let cwd_relative = input_str(input, "cwd_relative").map(str::to_string);

    // Build the shell-side command: write meta + spawn + record exit code.
    // We use bash because we need $! / wait / setsid. The composed script
    // returns the handle on stdout for the model to capture.
    let mut argv = shell_quote(program);
    for a in &args {
        argv.push(' ');
        argv.push_str(&shell_quote(a));
    }
    let script = format!(
        r#"
set -e
HANDLE="{prefix}$(date +%s%N)-$(printf '%04x' $((RANDOM * RANDOM % 65536)))"
BASE="/tmp/kimetsu-bg-$HANDLE"
META="$BASE.meta"
OUT="$BASE.out"
ERR="$BASE.err"
EXIT="$BASE.exitcode"
: > "$OUT"
: > "$ERR"
rm -f "$EXIT"
( setsid bash -c '{argv}; echo $? > '"$EXIT" \
    > "$OUT" 2> "$ERR" < /dev/null ) &
PID=$!
disown
printf '{{"handle":"%s","pid":%s,"started_at_sec":%s,"program":%s,"cwd":%s}}\n' \
    "$HANDLE" "$PID" "$(date +%s)" {prog_json} {cwd_json} > "$META"
echo "$HANDLE $PID"
"#,
        prefix = BG_HANDLE_PREFIX,
        argv = argv.replace('\'', "'\\''"),
        prog_json = shell_quote(&format!("\"{program}\"")),
        cwd_json = shell_quote(&format!("\"{}\"", cwd_relative.as_deref().unwrap_or("."))),
    );
    let out = match run_shell(
        runtime,
        "bash",
        vec!["-c".into(), script],
        cwd_relative.clone(),
        Some(TOOL_DEFAULT_TIMEOUT_SECS),
    ) {
        Ok(o) => o,
        Err(e) => return json!({ "error": format!("shell_background shell failed: {e}") }),
    };
    if out.exit_code != 0 {
        return json!({
            "error": format!("shell_background launch failed: exit {}; stderr {}", out.exit_code, truncate(&out.stderr_summary, 400))
        });
    }
    // Parse "HANDLE PID\n"
    let first_line = out.stdout_summary.lines().next().unwrap_or("").trim();
    let mut parts = first_line.splitn(2, ' ');
    let handle = parts.next().unwrap_or("").to_string();
    let pid: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    if handle.is_empty() || pid == 0 {
        return json!({
            "error": format!("shell_background: could not parse launcher output: {first_line:?}")
        });
    }
    json!({
        "handle": handle,
        "pid": pid,
        "program": program,
        "started": true,
        "stdout_path": format!("/tmp/kimetsu-bg-{handle}.out"),
        "stderr_path": format!("/tmp/kimetsu-bg-{handle}.err"),
    })
}

fn kimetsu_shell_status(runtime: &mut crate::tools::ToolRuntime, input: &Value) -> Value {
    let Some(handle) = input_str(input, "handle") else {
        return json!({ "error": "shell_status requires `handle`" });
    };
    if let Err(e) = validate_bg_handle(handle) {
        return json!({ "error": e });
    }
    let script = format!(
        r#"
BASE=/tmp/kimetsu-bg-{handle}
META="$BASE.meta"
OUT="$BASE.out"
ERR="$BASE.err"
EXIT="$BASE.exitcode"
if [[ ! -f "$META" ]]; then
    echo "no_such_handle"
    exit 0
fi
PID=$(grep -oE '"pid":[0-9]+' "$META" | head -1 | cut -d: -f2)
STARTED=$(grep -oE '"started_at_sec":[0-9]+' "$META" | head -1 | cut -d: -f2)
NOW=$(date +%s)
RUNTIME=$((NOW - STARTED))
BYTES_OUT=$(wc -c < "$OUT" 2>/dev/null || echo 0)
BYTES_ERR=$(wc -c < "$ERR" 2>/dev/null || echo 0)
if kill -0 "$PID" 2>/dev/null; then
    echo "running pid=$PID runtime=$RUNTIME out=$BYTES_OUT err=$BYTES_ERR"
elif [[ -f "$EXIT" ]]; then
    EC=$(cat "$EXIT")
    echo "exited pid=$PID runtime=$RUNTIME exit=$EC out=$BYTES_OUT err=$BYTES_ERR"
else
    echo "gone pid=$PID runtime=$RUNTIME out=$BYTES_OUT err=$BYTES_ERR"
fi
"#,
        handle = handle
    );
    let out = match run_shell(
        runtime,
        "bash",
        vec!["-c".into(), script],
        None,
        Some(TOOL_DEFAULT_TIMEOUT_SECS),
    ) {
        Ok(o) => o,
        Err(e) => return json!({ "error": format!("shell_status shell failed: {e}") }),
    };
    let line = out
        .stdout_summary
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    if line == "no_such_handle" {
        return json!({ "error": format!("no such handle: {handle}") });
    }
    parse_bg_status_line(&line, handle)
}

fn parse_bg_status_line(line: &str, handle: &str) -> Value {
    let mut state = "unknown";
    let mut fields = std::collections::HashMap::new();
    for (i, tok) in line.split_whitespace().enumerate() {
        if i == 0 {
            state = tok;
            continue;
        }
        if let Some((k, v)) = tok.split_once('=') {
            fields.insert(k.to_string(), v.to_string());
        }
    }
    let pid: u32 = fields.get("pid").and_then(|s| s.parse().ok()).unwrap_or(0);
    let runtime: u64 = fields
        .get("runtime")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let bytes_out: u64 = fields.get("out").and_then(|s| s.parse().ok()).unwrap_or(0);
    let bytes_err: u64 = fields.get("err").and_then(|s| s.parse().ok()).unwrap_or(0);
    let exit_code: Option<i32> = fields.get("exit").and_then(|s| s.parse().ok());
    let running = state == "running";
    json!({
        "handle": handle,
        "running": running,
        "state": state,
        "pid": pid,
        "runtime_sec": runtime,
        "exit_code": exit_code,
        "bytes_stdout": bytes_out,
        "bytes_stderr": bytes_err,
    })
}

fn kimetsu_shell_output(runtime: &mut crate::tools::ToolRuntime, input: &Value) -> Value {
    let Some(handle) = input_str(input, "handle") else {
        return json!({ "error": "shell_output requires `handle`" });
    };
    if let Err(e) = validate_bg_handle(handle) {
        return json!({ "error": e });
    }
    let tail_bytes = input_u32(input, "tail_bytes", 8192).max(1);
    let script = format!(
        r#"
BASE=/tmp/kimetsu-bg-{handle}
OUT="$BASE.out"
ERR="$BASE.err"
if [[ ! -f "$BASE.meta" ]]; then
    echo "no_such_handle"
    exit 0
fi
echo "::STDOUT::"
tail -c {n} "$OUT" 2>/dev/null || true
echo "::STDERR::"
tail -c {n} "$ERR" 2>/dev/null || true
echo "::END::"
"#,
        handle = handle,
        n = tail_bytes
    );
    let out = match run_shell(
        runtime,
        "bash",
        vec!["-c".into(), script],
        None,
        Some(TOOL_DEFAULT_TIMEOUT_SECS),
    ) {
        Ok(o) => o,
        Err(e) => return json!({ "error": format!("shell_output shell failed: {e}") }),
    };
    let raw = out.stdout_summary;
    if raw.contains("no_such_handle") {
        return json!({ "error": format!("no such handle: {handle}") });
    }
    let stdout_tail = extract_between(&raw, "::STDOUT::", "::STDERR::");
    let stderr_tail = extract_between(&raw, "::STDERR::", "::END::");
    json!({
        "handle": handle,
        "stdout_tail": stdout_tail,
        "stderr_tail": stderr_tail,
        "tail_bytes": tail_bytes,
    })
}

fn extract_between(raw: &str, start: &str, end: &str) -> String {
    if let Some(s) = raw.find(start) {
        let after = &raw[s + start.len()..];
        if let Some(e) = after.find(end) {
            return after[..e].trim_matches('\n').to_string();
        }
        return after.trim_matches('\n').to_string();
    }
    String::new()
}

fn kimetsu_shell_stop(runtime: &mut crate::tools::ToolRuntime, input: &Value) -> Value {
    let Some(handle) = input_str(input, "handle") else {
        return json!({ "error": "shell_stop requires `handle`" });
    };
    if let Err(e) = validate_bg_handle(handle) {
        return json!({ "error": e });
    }
    let signal = input_u32(input, "signal", 15).clamp(1, 31);
    let script = format!(
        r#"
BASE=/tmp/kimetsu-bg-{handle}
META="$BASE.meta"
EXIT="$BASE.exitcode"
if [[ ! -f "$META" ]]; then
    echo "no_such_handle"
    exit 0
fi
PID=$(grep -oE '"pid":[0-9]+' "$META" | head -1 | cut -d: -f2)
STARTED=$(grep -oE '"started_at_sec":[0-9]+' "$META" | head -1 | cut -d: -f2)
NOW=$(date +%s)
RUNTIME=$((NOW - STARTED))
if kill -0 "$PID" 2>/dev/null; then
    # Target the whole process group (setsid'd at spawn time).
    PGID=$(ps -o pgid= -p "$PID" 2>/dev/null | tr -d ' ' || true)
    if [[ -n "$PGID" ]]; then
        kill -{signal} -"$PGID" 2>/dev/null || kill -{signal} "$PID" 2>/dev/null || true
    else
        kill -{signal} "$PID" 2>/dev/null || true
    fi
    # Wait briefly for it to die.
    for _ in 1 2 3 4 5 6 7 8 9 10; do
        kill -0 "$PID" 2>/dev/null || break
        sleep 0.2
    done
    if kill -0 "$PID" 2>/dev/null; then
        echo "still_running pid=$PID runtime=$RUNTIME signal={signal}"
    elif [[ -f "$EXIT" ]]; then
        echo "stopped pid=$PID runtime=$RUNTIME exit=$(cat "$EXIT")"
    else
        echo "stopped pid=$PID runtime=$RUNTIME exit=-{signal}"
    fi
elif [[ -f "$EXIT" ]]; then
    echo "already_exited pid=$PID runtime=$RUNTIME exit=$(cat "$EXIT")"
else
    echo "gone pid=$PID runtime=$RUNTIME"
fi
"#,
        handle = handle,
        signal = signal
    );
    let out = match run_shell(
        runtime,
        "bash",
        vec!["-c".into(), script],
        None,
        Some(TOOL_DEFAULT_TIMEOUT_SECS),
    ) {
        Ok(o) => o,
        Err(e) => return json!({ "error": format!("shell_stop shell failed: {e}") }),
    };
    let line = out.stdout_summary.lines().next().unwrap_or("").trim();
    if line == "no_such_handle" {
        return json!({ "error": format!("no such handle: {handle}") });
    }
    let status = parse_bg_status_line(line, handle);
    let stopped = matches!(
        status.get("state").and_then(Value::as_str),
        Some("stopped") | Some("already_exited") | Some("gone")
    );
    let mut obj = status.as_object().cloned().unwrap_or_default();
    obj.insert("stopped".to_string(), json!(stopped));
    obj.insert("signal".to_string(), json!(signal));
    Value::Object(obj)
}

// ---------------------------------------------------------------------------
// MP-16c: view_image - metadata + optional base64 for workspace images.
//
// Cheap version: shell out to `file`, `wc -c`, `sha256sum`, plus a sniff
// for image dimensions when ImageMagick's `identify` is present. We
// deliberately don't pipe the image to Claude's vision API here -
// surfacing image bytes to the model via the claude `-p` text channel
// would require provider-level work outside MP-16 scope. The metadata
// + optional base64 still help: the model can verify expected files,
// compare hashes, or hand the base64 to a Python script it writes.

const VIEW_IMAGE_DEFAULT_MAX_BYTES: u32 = 256 * 1024;

fn kimetsu_view_image(runtime: &mut crate::tools::ToolRuntime, input: &Value) -> Value {
    let Some(path) = input_str(input, "path") else {
        return json!({ "error": "view_image requires `path`" });
    };
    let include_b64 = input
        .get("include_base64")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let max_bytes = input_u32(input, "max_bytes", VIEW_IMAGE_DEFAULT_MAX_BYTES).max(64);

    let want_b64 = if include_b64 { "1" } else { "0" };
    let script = format!(
        r#"
set -u
P={path_q}
if [[ ! -f "$P" ]]; then
    echo "::ERR::not a regular file"
    exit 0
fi
SIZE=$(wc -c < "$P" 2>/dev/null || echo 0)
SHA=$(sha256sum "$P" 2>/dev/null | awk '{{print $1}}')
TYPE=$(file -b --mime-type "$P" 2>/dev/null || echo "application/octet-stream")
WH=$(identify -format '%w %h' "$P" 2>/dev/null | head -1 || echo "")
echo "::SIZE::$SIZE"
echo "::SHA::$SHA"
echo "::TYPE::$TYPE"
echo "::WH::$WH"
if [[ "{want_b64}" == "1" && "$SIZE" -le "{max_bytes}" ]]; then
    echo "::B64::"
    base64 -w0 "$P" 2>/dev/null
    echo
    echo "::B64END::"
fi
"#,
        path_q = shell_quote(path),
        want_b64 = want_b64,
        max_bytes = max_bytes,
    );
    let out = match run_shell(
        runtime,
        "bash",
        vec!["-c".into(), script],
        None,
        Some(TOOL_DEFAULT_TIMEOUT_SECS),
    ) {
        Ok(o) => o,
        Err(e) => return json!({ "error": format!("view_image shell failed: {e}") }),
    };
    let raw = &out.stdout_summary;
    if let Some(err) = raw.strip_prefix("::ERR::") {
        return json!({
            "error": format!("view_image: {}", err.lines().next().unwrap_or("").trim()),
            "path": path,
        });
    }
    let size = extract_marker(raw, "::SIZE::")
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    let sha = extract_marker(raw, "::SHA::").unwrap_or_default();
    let mime = extract_marker(raw, "::TYPE::").unwrap_or_default();
    let wh = extract_marker(raw, "::WH::").unwrap_or_default();
    let (width, height) = parse_wh(&wh);
    let base64 = if include_b64 {
        Some(extract_between(raw, "::B64::", "::B64END::").replace('\n', ""))
    } else {
        None
    };
    let format = mime
        .strip_prefix("image/")
        .map(str::to_string)
        .unwrap_or(mime.clone());
    json!({
        "path": path,
        "format": format,
        "mime": mime,
        "size_bytes": size,
        "sha256": sha,
        "width": width,
        "height": height,
        "base64": base64,
        "base64_truncated": include_b64 && size > max_bytes as u64,
    })
}

fn extract_marker(raw: &str, marker: &str) -> Option<String> {
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix(marker) {
            return Some(rest.trim().to_string());
        }
    }
    None
}

fn parse_wh(s: &str) -> (Option<u32>, Option<u32>) {
    let mut parts = s.split_whitespace();
    let w = parts.next().and_then(|p| p.parse().ok());
    let h = parts.next().and_then(|p| p.parse().ok());
    (w, h)
}

// --- small helpers ---------------------------------------------------------

/// MP-13a: orient the model with one upfront shell command. Returns
/// a multi-block string (pwd / top-level ls / nearby task-instruction
/// files / build system sniff). Best-effort - on failure we return
/// None and the user message just omits the section, which is no
/// worse than the pre-MP-13 behavior.
///
/// We deliberately do this in a SINGLE composite shell call so it
/// costs one shell/tool round-trip instead of four.
fn collect_workspace_orientation(runtime: &mut crate::tools::ToolRuntime) -> Option<String> {
    let script = concat!(
        "echo '## pwd';",
        " pwd;",
        " echo '## top-level (ls -la)';",
        " ls -la 2>/dev/null | head -40;",
        " if [ -d /app ] && [ \"$(pwd)\" != /app ]; then",
        "   echo '## /app (ls -la)';",
        "   ls -la /app 2>/dev/null | head -40;",
        " fi;",
        " echo '## task-instruction files (head -80 of first match)';",
        " for f in TASK.md task.md INSTRUCTIONS.md INSTRUCTIONS.txt README.md README.txt README PROBLEM.md problem.md PROMPT.md prompt.md;",
        " do",
        "   if [ -r \"$f\" ]; then echo \"--- $f ---\"; head -80 \"$f\"; break; fi;",
        "   if [ -r \"/app/$f\" ]; then echo \"--- /app/$f ---\"; head -80 \"/app/$f\"; break; fi;",
        " done;",
        " echo '## build-system sniff';",
        " ls -1 Makefile makefile CMakeLists.txt setup.py pyproject.toml package.json Cargo.toml go.mod build.gradle pom.xml /app/Makefile /app/CMakeLists.txt /app/setup.py /app/pyproject.toml /app/package.json /app/Cargo.toml 2>/dev/null | head -10;",
        " echo '## done'",
    );

    let spec = crate::tools::CommandSpec {
        program: "bash".to_string(),
        args: vec!["-c".to_string(), script.to_string()],
        cwd_relative: None,
        timeout_secs: Some(30),
        expected_exit: None,
    };
    match runtime.shell_command(spec) {
        Ok(out) if out.exit_code == 0 || !out.stdout_summary.trim().is_empty() => {
            // Cap at ~3500 bytes so a runaway ls -la doesn't dominate
            // the user-message budget. Most task-instruction files
            // fit in 2-3 KB; ~3500 bytes still leaves headroom for
            // the task description and the tool-format block.
            const CAP_BYTES: usize = 3500;
            let raw = out.stdout_summary;
            if raw.len() <= CAP_BYTES {
                Some(raw)
            } else {
                let mut clipped = raw[..CAP_BYTES].to_string();
                clipped.push_str("\n...[orientation truncated]\n");
                Some(clipped)
            }
        }
        _ => None,
    }
}

/// POSIX-style single-quote escape: `it's fine` -> `'it'\''s fine'`.
fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

fn truncate(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        s.to_string()
    } else {
        format!("{}...", &s[..cap])
    }
}

fn parse_grep_line(line: &str) -> Option<Value> {
    // grep -n format: "<file>:<line>:<text>"
    let mut parts = line.splitn(3, ':');
    let file = parts.next()?.to_string();
    let line_no: u32 = parts.next()?.parse().ok()?;
    let text = parts.next()?.to_string();
    Some(json!({ "file": file, "line": line_no, "text": text }))
}

/// Tiny base64 encoder (RFC 4648) - we don't need a crate just for one
/// helper. Used to ship file content through a shell heredoc safely.
fn base64_encode(input: &[u8]) -> String {
    const ALPHA: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    let mut chunks = input.chunks_exact(3);
    for chunk in &mut chunks {
        let n = (u32::from(chunk[0]) << 16) | (u32::from(chunk[1]) << 8) | u32::from(chunk[2]);
        out.push(ALPHA[((n >> 18) & 0x3F) as usize] as char);
        out.push(ALPHA[((n >> 12) & 0x3F) as usize] as char);
        out.push(ALPHA[((n >> 6) & 0x3F) as usize] as char);
        out.push(ALPHA[(n & 0x3F) as usize] as char);
    }
    let rem = chunks.remainder();
    match rem.len() {
        1 => {
            let n = u32::from(rem[0]) << 16;
            out.push(ALPHA[((n >> 18) & 0x3F) as usize] as char);
            out.push(ALPHA[((n >> 12) & 0x3F) as usize] as char);
            out.push_str("==");
        }
        2 => {
            let n = (u32::from(rem[0]) << 16) | (u32::from(rem[1]) << 8);
            out.push(ALPHA[((n >> 18) & 0x3F) as usize] as char);
            out.push(ALPHA[((n >> 12) & 0x3F) as usize] as char);
            out.push(ALPHA[((n >> 6) & 0x3F) as usize] as char);
            out.push('=');
        }
        _ => {}
    }
    out
}

/// MP-14a counterpart to base64_encode: decode standard RFC 4648
/// base64 (with optional `=` padding, whitespace skipped). Used by
/// edit_file to round-trip file content read back from the container.
fn base64_decode(input: &str) -> Result<Vec<u8>, String> {
    let mut buf: Vec<u8> = Vec::with_capacity(input.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for ch in input.chars() {
        if ch.is_whitespace() {
            continue;
        }
        if ch == '=' {
            break;
        }
        let v: u32 = match ch {
            'A'..='Z' => ch as u32 - 'A' as u32,
            'a'..='z' => ch as u32 - 'a' as u32 + 26,
            '0'..='9' => ch as u32 - '0' as u32 + 52,
            '+' => 62,
            '/' => 63,
            _ => return Err(format!("invalid base64 character: {ch:?}")),
        };
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            buf.push(((acc >> bits) & 0xFF) as u8);
        }
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----- MP-14e unit tests (pure-Rust paths, no shell needed) -----

    #[test]
    fn plan_tool_validates_and_normalizes_todos() {
        let input = json!({
            "todos": [
                {"content": "scan", "status": "completed", "activeForm": "Scanning"},
                {"content": "fix",  "status": "in_progress"},
                {"content": "ship", "status": "pending"},
            ]
        });
        let result = kimetsu_plan(&input);
        assert_eq!(result["total"], 3);
        assert_eq!(result["pending"], 1);
        assert_eq!(result["in_progress"], 1);
        assert_eq!(result["completed"], 1);
        // activeForm defaults to content when omitted.
        assert_eq!(result["todos"][1]["activeForm"], "fix");
        assert_eq!(result["todos"][0]["activeForm"], "Scanning");
    }

    #[test]
    fn plan_tool_rejects_invalid_status() {
        let input = json!({
            "todos": [
                {"content": "x", "status": "blocked"},
            ]
        });
        let result = kimetsu_plan(&input);
        let err = result["error"].as_str().unwrap_or("");
        assert!(
            err.contains("must be pending|in_progress|completed"),
            "got: {err}"
        );
    }

    #[test]
    fn plan_tool_rejects_missing_fields() {
        assert!(kimetsu_plan(&json!({})).get("error").is_some());
        assert!(
            kimetsu_plan(&json!({"todos": [{"status": "pending"}]}))
                .get("error")
                .is_some()
        );
        assert!(
            kimetsu_plan(&json!({"todos": [{"content": "x"}]}))
                .get("error")
                .is_some()
        );
    }

    #[test]
    fn think_tool_acknowledges_and_reports_length() {
        let thought = "consider whether to rebuild";
        let result = kimetsu_think(&json!({"thought": thought}));
        assert_eq!(result["ack"], true);
        assert_eq!(result["thought_len"], thought.len() as u64);
        assert!(kimetsu_think(&json!({})).get("error").is_some());
    }

    #[test]
    fn check_workspace_path_rejects_escape_attempts() {
        assert!(check_workspace_path("p", "").is_err());
        assert!(check_workspace_path("p", "/etc/passwd").is_err());
        assert!(check_workspace_path("p", "../sibling").is_err());
        assert!(check_workspace_path("p", "ok/path/../bad").is_err());
        assert!(check_workspace_path("p", "ok/path").is_ok());
        assert!(check_workspace_path("p", "deep/nested/file.rs").is_ok());
    }

    // ----- MP-16 unit tests (pure-Rust paths) -----

    #[test]
    fn codex_patch_translator_handles_update_file() {
        let codex = "\
*** Begin Patch
*** Update File: src/lib.rs
@@
-old line
+new line
*** End Patch
";
        let unified = translate_codex_patch(codex).expect("translate");
        assert!(unified.contains("--- a/src/lib.rs"));
        assert!(unified.contains("+++ b/src/lib.rs"));
        assert!(unified.contains("-old line"));
        assert!(unified.contains("+new line"));
    }

    #[test]
    fn codex_patch_translator_handles_add_file() {
        let codex = "\
*** Begin Patch
*** Add File: notes/new.md
+# Heading
+body line
*** End Patch
";
        let unified = translate_codex_patch(codex).expect("translate");
        assert!(unified.contains("--- /dev/null"));
        assert!(unified.contains("+++ b/notes/new.md"));
        // Hunk header reflects 2 added lines.
        assert!(unified.contains("@@ -0,0 +1,2 @@"));
        assert!(unified.contains("+# Heading"));
    }

    #[test]
    fn codex_patch_translator_handles_delete_file() {
        let codex = "\
*** Begin Patch
*** Delete File: legacy/old.rs
*** End Patch
";
        let unified = translate_codex_patch(codex).expect("translate");
        assert!(unified.contains("--- a/legacy/old.rs"));
        assert!(unified.contains("+++ /dev/null"));
    }

    #[test]
    fn codex_patch_translator_synthesizes_hunk_when_missing() {
        // Update File with no `@@` marker still gets a synthesized header.
        let codex = "\
*** Begin Patch
*** Update File: src/x.rs
-removed
+added
*** End Patch
";
        let unified = translate_codex_patch(codex).expect("translate");
        assert!(unified.contains("@@ -1,"));
    }

    #[test]
    fn codex_patch_translator_rejects_unmarked_input() {
        let plain = "diff --git a/x b/x\n--- a/x\n+++ b/x\n";
        let err = translate_codex_patch(plain).unwrap_err();
        assert!(err.contains("Begin Patch"));
    }

    #[test]
    fn validate_bg_handle_accepts_well_formed() {
        assert!(validate_bg_handle("bg-1234567890-abcd").is_ok());
        assert!(validate_bg_handle("bg-abc_def-0001").is_ok());
    }

    #[test]
    fn validate_bg_handle_rejects_garbage() {
        assert!(validate_bg_handle("").is_err());
        assert!(validate_bg_handle("noprefix").is_err());
        assert!(validate_bg_handle("bg-with;semicolon").is_err());
        assert!(validate_bg_handle("bg-`backtick`").is_err());
        // Pathological-length handle is refused.
        let oversized = format!("bg-{}", "a".repeat(200));
        assert!(validate_bg_handle(&oversized).is_err());
    }

    #[test]
    fn parse_bg_status_line_running_state() {
        let v = parse_bg_status_line("running pid=12345 runtime=42 out=1024 err=128", "bg-1-abcd");
        assert_eq!(v["state"], "running");
        assert_eq!(v["running"], true);
        assert_eq!(v["pid"], 12345);
        assert_eq!(v["runtime_sec"], 42);
        assert_eq!(v["bytes_stdout"], 1024);
        assert_eq!(v["bytes_stderr"], 128);
        assert!(v["exit_code"].is_null());
    }

    #[test]
    fn parse_bg_status_line_exited_state() {
        let v = parse_bg_status_line(
            "exited pid=999 runtime=120 exit=0 out=2048 err=0",
            "bg-2-beef",
        );
        assert_eq!(v["state"], "exited");
        assert_eq!(v["running"], false);
        assert_eq!(v["exit_code"], 0);
    }

    #[test]
    fn extract_between_pulls_section() {
        let raw = "::A::\nhello\nworld\n::B::\ntrailing\n";
        assert_eq!(extract_between(raw, "::A::", "::B::"), "hello\nworld");
    }

    #[test]
    fn extract_marker_finds_first_match() {
        let raw = "::SIZE::1234\n::SHA::abc\n";
        assert_eq!(extract_marker(raw, "::SIZE::"), Some("1234".to_string()));
        assert_eq!(extract_marker(raw, "::MISSING::"), None);
    }

    #[test]
    fn parse_wh_handles_well_formed_and_garbage() {
        assert_eq!(parse_wh("640 480"), (Some(640), Some(480)));
        assert_eq!(parse_wh(""), (None, None));
        assert_eq!(parse_wh("abc def"), (None, None));
    }

    // ----- MP-17 #10: unified-diff parser tests -----

    #[test]
    fn parse_unified_diff_single_file_single_hunk() {
        let diff = "\
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,3 +1,3 @@
 fn main() {
-    let x = 1;
+    let x = 2;
 }
";
        let files = parse_unified_diff(diff).expect("parse");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "src/lib.rs");
        assert!(!files[0].is_new);
        assert!(!files[0].is_delete);
        assert_eq!(files[0].hunks.len(), 1);
        let h = &files[0].hunks[0];
        assert!(h.old.contains("let x = 1;"));
        assert!(h.new.contains("let x = 2;"));
        assert!(h.old.contains("fn main()"));
        assert!(h.new.contains("fn main()"));
    }

    #[test]
    fn parse_unified_diff_detects_new_file() {
        let diff = "\
--- /dev/null
+++ b/notes/new.md
@@ -0,0 +1,2 @@
+# heading
+body
";
        let files = parse_unified_diff(diff).expect("parse");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "notes/new.md");
        assert!(files[0].is_new);
        assert!(!files[0].is_delete);
        assert_eq!(files[0].hunks.len(), 1);
        assert!(files[0].hunks[0].new.contains("# heading"));
    }

    #[test]
    fn parse_unified_diff_detects_delete_file() {
        let diff = "\
--- a/old.rs
+++ /dev/null
@@ -1,1 +0,0 @@
-line
";
        let files = parse_unified_diff(diff).expect("parse");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "old.rs");
        assert!(!files[0].is_new);
        assert!(files[0].is_delete);
    }

    #[test]
    fn parse_unified_diff_multi_file() {
        let diff = "\
--- a/x.rs
+++ b/x.rs
@@ -1,1 +1,1 @@
-a
+b
--- a/y.rs
+++ b/y.rs
@@ -1,1 +1,1 @@
-c
+d
";
        let files = parse_unified_diff(diff).expect("parse");
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "x.rs");
        assert_eq!(files[1].path, "y.rs");
    }

    #[test]
    fn parse_unified_diff_rejects_missing_plus_header() {
        let diff = "--- a/x\nbogus\n";
        assert!(parse_unified_diff(diff).is_err());
    }

    #[test]
    fn parse_unified_diff_rejects_empty() {
        assert!(parse_unified_diff("").is_err());
        assert!(parse_unified_diff("no headers here").is_err());
    }

    #[test]
    fn task_signals_verification_matches_specific_triggers() {
        // Concrete verifier identifiers / deliverables should fire.
        assert!(task_signals_verification(
            "Ensure pytest passes when you run it"
        ));
        assert!(task_signals_verification(
            "Your script should output 42 when run"
        ));
        assert!(task_signals_verification(
            "The verifier will run /app/test.sh"
        ));
        assert!(task_signals_verification(
            "make test must pass before declaring done"
        ));
        assert!(task_signals_verification(
            "cargo test should pass under release mode"
        ));
    }

    #[test]
    fn task_signals_verification_skips_generic_language() {
        // MP-17m.1: generic "ensure that"/"check that"/"i will check"
        // appear in tons of task instructions where verification IS the
        // task - firing the nudge there costs a turn or two with no
        // upside. These should NOT trigger.
        assert!(!task_signals_verification(
            "Ensure that the LaTeX document compiles successfully"
        ));
        assert!(!task_signals_verification(
            "I will check that you booted doom correctly"
        ));
        assert!(!task_signals_verification(
            "check that the first frame is correctly created"
        ));
        // Pure transformation / generation tasks still don't trigger.
        assert!(!task_signals_verification(
            "Replace words in input.tex with their synonyms in synonyms.txt"
        ));
        assert!(!task_signals_verification(
            "Summarize the log file at /app/access.log into a TOML report"
        ));
        assert!(!task_signals_verification(
            "Generate a self-signed TLS certificate at /tmp/cert.pem"
        ));
    }

    #[test]
    fn extract_diff_path_strips_ab_prefix() {
        assert_eq!(extract_diff_path("a/src/lib.rs"), "src/lib.rs");
        assert_eq!(extract_diff_path("b/notes/new.md"), "notes/new.md");
        assert_eq!(extract_diff_path("plain/path.rs"), "plain/path.rs");
        assert_eq!(extract_diff_path("  a/x.rs  "), "x.rs");
    }

    // ----- MP-18: record_deviation + render_verify_nudge tests -----

    #[test]
    fn record_deviation_accepts_complete_input() {
        let input = json!({
            "missing": "output.toml file was empty",
            "where_deviated": "step 3: forgot to write the analysis result",
            "lesson_for_next_time": "On tasks producing a file at a specific path, verify the file exists AND is non-empty before finish",
        });
        let result = kimetsu_record_deviation(&input);
        assert_eq!(result["recorded"], true);
        assert_eq!(result["missing"], "output.toml file was empty");
        assert!(
            result["lesson_for_next_time"]
                .as_str()
                .unwrap()
                .contains("non-empty")
        );
    }

    #[test]
    fn record_deviation_rejects_missing_fields() {
        assert!(kimetsu_record_deviation(&json!({})).get("error").is_some());
        assert!(
            kimetsu_record_deviation(&json!({"missing": "x"}))
                .get("error")
                .is_some()
        );
        assert!(
            kimetsu_record_deviation(&json!({"missing": "x", "where_deviated": "y"}))
                .get("error")
                .is_some()
        );
    }

    #[test]
    fn record_deviation_rejects_empty_strings() {
        let r = kimetsu_record_deviation(&json!({
            "missing": "",
            "where_deviated": "  ",
            "lesson_for_next_time": "",
        }));
        assert!(r["error"].as_str().unwrap().contains("non-empty"));
    }

    #[test]
    fn render_verify_nudge_includes_task_recap() {
        let nudge = render_verify_nudge(
            "Build CompCert from source and expose ccomp binary at /tmp/ccomp",
            None,
            1,
            false,
        );
        assert!(nudge.contains("Build CompCert"));
        assert!(nudge.contains("Goal recap"));
        assert!(nudge.contains("Inspect"));
        // First iteration uses gentle framing.
        assert!(nudge.contains("Before I accept finish"));
        // Lenient mode says "optionally call record_deviation".
        assert!(nudge.contains("optionally"));
    }

    #[test]
    fn render_verify_nudge_renders_plan_recap_when_present() {
        let plan = json!({
            "todos": [
                {"content": "configure", "status": "completed"},
                {"content": "build", "status": "completed"},
                {"content": "verify ccomp -v", "status": "in_progress"},
            ]
        });
        let nudge = render_verify_nudge("(task)", Some(&plan), 1, false);
        assert!(nudge.contains("1. [completed] configure"));
        assert!(nudge.contains("2. [completed] build"));
        assert!(nudge.contains("3. [in_progress] verify ccomp -v"));
    }

    #[test]
    fn render_verify_nudge_strict_mode_requires_record_deviation() {
        let strict = render_verify_nudge("(task)", None, 1, true);
        assert!(strict.contains("MUST call `record_deviation`"));
        let lenient = render_verify_nudge("(task)", None, 1, false);
        assert!(!lenient.contains("MUST call"));
        assert!(lenient.contains("optionally"));
    }

    #[test]
    fn render_verify_nudge_second_iteration_is_firmer() {
        let first = render_verify_nudge("(task)", None, 1, false);
        let second = render_verify_nudge("(task)", None, 2, false);
        assert!(first.contains("Before I accept finish"));
        assert!(second.contains("Second verify pass"));
        assert!(second.contains("Take this seriously"));
    }

    /// MP-17m.2: the stall heartbeat classifier - workspace-touching
    /// tools count as activity; pure deliberation tools don't.
    #[test]
    fn is_useful_tool_recognises_workspace_actions() {
        // Workspace-touching tools (reset stall counter).
        for tool in [
            "shell_command",
            "shell_background",
            "shell_status",
            "shell_output",
            "shell_stop",
            "read_file",
            "multi_read",
            "list_files",
            "glob",
            "search_files",
            "write_file",
            "edit_file",
            "apply_patch",
            "move_file",
            "delete_file",
            "git_status",
            "git_diff",
        ] {
            assert!(is_useful_tool(tool), "{tool} should be useful");
        }
    }

    #[test]
    fn is_useful_tool_excludes_deliberation_tools() {
        // Pure-deliberation tools shouldn't reset the stall counter.
        assert!(!is_useful_tool("think"));
        assert!(!is_useful_tool("plan"));
        // view_image is borderline (it's a metadata read) - current policy
        // is to NOT count it; we can revisit if the gauntlet shows it
        // helping.
        assert!(!is_useful_tool("view_image"));
        // Unknown / typo tool names also don't count.
        assert!(!is_useful_tool("unknown_tool"));
        assert!(!is_useful_tool(""));
    }
}
