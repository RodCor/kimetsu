//! Stub agent runners for harbor mode.
//!
//! v0.3.2 â€” moved from `kimetsu-agent::harness`. Used by
//! `kimetsu-harbor-agent --stub` so the Python adapter can be
//! smoke-tested on machines without API credentials.

use std::cell::RefCell;
use std::io::{BufRead, Write};
use std::rc::Rc;

use kimetsu_agent::tools::CommandSpec;
use kimetsu_core::KimetsuResult;
use serde_json::json;

use crate::protocol::default_timeout_secs;
use crate::protocol::{AgentDoneParams, HARBOR_PROTOCOL_VERSION, ToolExecParams, ToolExecResult};
use crate::session::HarborSession;

/// MP-7a stub agent loop. Exists so we can wire the CLI subcommand and
/// prove the protocol round-trips end to end. Runs one `echo` via
/// tool.exec, emits agent.done, returns.
pub fn run_stub_agent<R: BufRead, W: Write>(
    task: &str,
    session: &mut HarborSession<R, W>,
) -> KimetsuResult<ToolExecResult> {
    let probe = session.request_tool_exec(ToolExecParams {
        program: "echo".into(),
        args: vec![format!("kimetsu MP-7a harbor stub received task: {task}")],
        cwd: None,
        timeout_secs: default_timeout_secs(),
    })?;
    session.emit_done(AgentDoneParams {
        summary: format!(
            "stub agent for task `{task}` completed; protocol={HARBOR_PROTOCOL_VERSION}"
        ),
        context: Some(json!({
            "stub": true,
            "echo_exit_code": probe.exit_code,
        })),
    })?;
    Ok(probe)
}

/// MP-7c multi-step stub. Routes two shell commands through
/// `HarborShellExecutor` (`pwd` + `echo`) and emits agent.done.
pub fn run_multi_step_stub<R: BufRead + 'static, W: Write + 'static>(
    task: &str,
    session: Rc<RefCell<HarborSession<R, W>>>,
    runtime: &mut kimetsu_agent::tools::ToolRuntime,
) -> KimetsuResult<MultiStepStubReport> {
    let pwd_out = runtime.shell_command(CommandSpec {
        program: "pwd".into(),
        args: vec![],
        cwd_relative: None,
        timeout_secs: Some(15),
        expected_exit: Some(0),
    })?;
    let echo_out = runtime.shell_command(CommandSpec {
        program: "echo".into(),
        args: vec![format!("kimetsu MP-7c routed task: {task}")],
        cwd_relative: None,
        timeout_secs: Some(15),
        expected_exit: Some(0),
    })?;

    session.borrow_mut().emit_done(AgentDoneParams {
        summary: format!(
            "MP-7c multi-step stub for `{task}` completed; protocol={HARBOR_PROTOCOL_VERSION}"
        ),
        context: Some(json!({
            "stub": "multi-step",
            "steps": [
                { "program": "pwd",  "exit_code": pwd_out.exit_code },
                { "program": "echo", "exit_code": echo_out.exit_code },
            ],
        })),
    })?;

    Ok(MultiStepStubReport {
        pwd: pwd_out,
        echo: echo_out,
    })
}

#[derive(Debug)]
pub struct MultiStepStubReport {
    pub pwd: kimetsu_agent::tools::ShellCommandOutput,
    pub echo: kimetsu_agent::tools::ShellCommandOutput,
}
