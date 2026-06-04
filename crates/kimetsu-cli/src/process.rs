//! Process control for Kimetsu: list, stop, and restart running kimetsu processes.
//!
//! `kimetsu ps`      — list running kimetsu processes (PID, kind, workspace, exe)
//! `kimetsu stop`    — stop one or all kimetsu processes
//! `kimetsu restart` — stop all MCP servers (host will respawn them on next use)

use std::path::Path;
use std::process::Command as ProcessCommand;

use serde::{Deserialize, Serialize};

// ──────────────────────────────────────────────────────────────────────────────
// Public API
// ──────────────────────────────────────────────────────────────────────────────

/// Classification of what role a kimetsu process is playing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcKind {
    /// `kimetsu mcp serve` — the MCP sidecar that hosts like Claude Code spawn.
    McpServe,
    /// `kimetsu chat` — an interactive REPL chat session.
    Chat,
    /// A brain hook (pretool-hook, posttool-hook, context-hook, stop-hook, …).
    Hook,
    /// A top-level CLI invocation (update, doctor, ps, …).
    Cli,
    Unknown,
}

impl ProcKind {
    /// Short human-readable label used in the `ps` table.
    pub fn label(&self) -> &'static str {
        match self {
            ProcKind::McpServe => "mcp-serve",
            ProcKind::Chat => "chat",
            ProcKind::Hook => "hook",
            ProcKind::Cli => "cli",
            ProcKind::Unknown => "unknown",
        }
    }
}

/// A single running kimetsu process.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KimetsuProc {
    pub pid: u32,
    pub exe_path: Option<String>,
    pub command_line: Option<String>,
    pub kind: ProcKind,
    /// Extracted from `--workspace <path>` in the command line, when present.
    pub workspace: Option<String>,
}

/// List running kimetsu processes, excluding the current process.
///
/// Best-effort: returns an empty `Vec` on any query failure so that the CLI
/// never hard-errors on a diagnostics listing.
pub fn list_kimetsu_processes() -> Vec<KimetsuProc> {
    let current = std::process::id();

    #[cfg(windows)]
    {
        query_windows(current)
    }

    #[cfg(not(windows))]
    {
        query_unix(current)
    }
}

/// Stop a set of processes identified by PID.  Returns per-pid results.
///
/// Silently skips the current PID — never kills self.
pub fn stop_processes(pids: &[u32]) -> Vec<(u32, Result<(), String>)> {
    let current = std::process::id();
    pids.iter()
        .filter(|&&pid| pid != current)
        .map(|&pid| (pid, kill_pid(pid)))
        .collect()
}

// ──────────────────────────────────────────────────────────────────────────────
// Classification helpers (cfg-agnostic so they compile & test everywhere)
// ──────────────────────────────────────────────────────────────────────────────

/// Classify a process based on its command-line string.
pub fn classify_kind(command_line: &str) -> ProcKind {
    let lower = command_line.to_lowercase();
    // "mcp serve" wins before more-generic checks
    if lower.contains("mcp serve") || lower.contains("mcp-serve") {
        return ProcKind::McpServe;
    }
    // Hooks: "brain pretool-hook", "brain posttool-hook", "brain context-hook",
    //         "brain stop-hook", "brain session-end-hook", "-hook" suffix catch-all
    if lower.contains("-hook") || (lower.contains("brain") && lower.contains("hook")) {
        return ProcKind::Hook;
    }
    if lower.contains(" chat") || lower.ends_with("chat") {
        return ProcKind::Chat;
    }
    ProcKind::Cli
}

/// Extract the value of `--workspace <path>` from a command-line string.
/// Handles both `--workspace /path` and `--workspace "/path with spaces"`.
pub fn extract_workspace(command_line: &str) -> Option<String> {
    // Split naively on whitespace, respecting a single level of double-quoting.
    let tokens = tokenize_cmdline(command_line);
    let mut iter = tokens.iter().peekable();
    while let Some(tok) = iter.next() {
        if tok == "--workspace" || tok == "-workspace" {
            if let Some(next) = iter.next() {
                return Some(next.trim_matches('"').to_string());
            }
        }
    }
    None
}

/// Minimal tokeniser: splits on whitespace but keeps double-quoted spans
/// together (strips the outer quotes).
fn tokenize_cmdline(s: &str) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;

    for ch in s.chars() {
        match ch {
            '"' => {
                in_quotes = !in_quotes;
                // Don't push the quote character itself
            }
            ' ' | '\t' if !in_quotes => {
                if !current.is_empty() {
                    tokens.push(current.clone());
                    current.clear();
                }
            }
            other => current.push(other),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

// ──────────────────────────────────────────────────────────────────────────────
// Platform-specific listing
// ──────────────────────────────────────────────────────────────────────────────

/// Query running kimetsu processes via CIM on Windows.
#[cfg(windows)]
fn query_windows(current_pid: u32) -> Vec<KimetsuProc> {
    let output = ProcessCommand::new("powershell")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            "Get-CimInstance Win32_Process -Filter \"Name='kimetsu.exe'\" | \
             Select-Object ProcessId,ExecutablePath,CommandLine | \
             ConvertTo-Csv -NoTypeInformation",
        ])
        .output();

    let output = match output {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };

    let text = String::from_utf8_lossy(&output.stdout);
    parse_windows_proc_csv(&text, current_pid)
}

/// Query running kimetsu processes via `ps` on Unix.
#[cfg(not(windows))]
fn query_unix(current_pid: u32) -> Vec<KimetsuProc> {
    let output = ProcessCommand::new("ps")
        .args(["-eo", "pid=,args="])
        .output();

    let output = match output {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };

    let text = String::from_utf8_lossy(&output.stdout);
    parse_unix_ps(&text, current_pid)
}

// ──────────────────────────────────────────────────────────────────────────────
// Platform-specific killing
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(windows)]
fn kill_pid(pid: u32) -> Result<(), String> {
    let result = ProcessCommand::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .output();
    match result {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => Err(String::from_utf8_lossy(&o.stderr).trim().to_string()),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(not(windows))]
fn kill_pid(pid: u32) -> Result<(), String> {
    use std::os::unix::process::ExitStatusExt;

    // Try SIGTERM first.
    let sigterm = ProcessCommand::new("kill")
        .args(["-TERM", &pid.to_string()])
        .output();
    match sigterm {
        Ok(o) if o.status.success() => return Ok(()),
        _ => {}
    }
    // Fall back to SIGKILL.
    let sigkill = ProcessCommand::new("kill")
        .args(["-KILL", &pid.to_string()])
        .output();
    match sigkill {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => Err(format!(
            "kill -KILL {pid} exited {}",
            o.status.code().or_else(|| o.status.signal()).unwrap_or(-1)
        )),
        Err(e) => Err(e.to_string()),
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Pure parsers (cfg-agnostic — tested on any platform)
// ──────────────────────────────────────────────────────────────────────────────

/// Parse the CSV produced by:
///   `Get-CimInstance Win32_Process … | Select-Object ProcessId,ExecutablePath,CommandLine | ConvertTo-Csv -NoTypeInformation`
///
/// Expected header:  `"ProcessId","ExecutablePath","CommandLine"`
/// Expected rows:    `"1234","C:\...\kimetsu.exe","kimetsu mcp serve --workspace C:\proj"`
pub fn parse_windows_proc_csv(output: &str, current_pid: u32) -> Vec<KimetsuProc> {
    let mut procs = Vec::new();
    let mut lines = output.lines().peekable();

    // --- locate and validate the header row ---
    let header_line = loop {
        match lines.next() {
            None => return procs, // empty / no data
            Some(l) => {
                let l = l.trim();
                if l.is_empty() {
                    continue;
                }
                break l;
            }
        }
    };

    // Determine column order from header.
    let header_cols = parse_csv_row(header_line);
    let col_pid = header_cols
        .iter()
        .position(|c| c.eq_ignore_ascii_case("ProcessId"));
    let col_exe = header_cols
        .iter()
        .position(|c| c.eq_ignore_ascii_case("ExecutablePath"));
    let col_cmd = header_cols
        .iter()
        .position(|c| c.eq_ignore_ascii_case("CommandLine"));

    // If we can't find ProcessId, bail — we can't do anything useful.
    let col_pid = match col_pid {
        Some(c) => c,
        None => return procs,
    };

    // --- parse data rows ---
    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let cols = parse_csv_row(line);
        if cols.len() <= col_pid {
            continue; // malformed
        }

        let pid: u32 = match cols[col_pid].parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        if pid == current_pid {
            continue;
        }

        let exe_path = col_exe
            .and_then(|i| cols.get(i))
            .filter(|s| !s.is_empty())
            .cloned();

        let command_line = col_cmd
            .and_then(|i| cols.get(i))
            .filter(|s| !s.is_empty())
            .cloned();

        let kind = command_line
            .as_deref()
            .map(classify_kind)
            .unwrap_or(ProcKind::Unknown);
        let workspace = command_line.as_deref().and_then(extract_workspace);

        procs.push(KimetsuProc {
            pid,
            exe_path,
            command_line,
            kind,
            workspace,
        });
    }

    procs
}

/// Parse the output of `ps -eo pid=,args=` (one process per line).
///
/// Line format: `  <pid>  <full command line with args>`
/// We filter to rows whose command contains `kimetsu` (binary name).
// On Windows this function is only called from tests (the live path uses query_windows),
// so suppress the dead_code lint there while keeping it pub for cross-platform testing.
#[cfg_attr(windows, allow(dead_code))]
pub fn parse_unix_ps(output: &str, current_pid: u32) -> Vec<KimetsuProc> {
    let mut procs = Vec::new();

    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // First token is the PID.
        let (pid_str, rest) = match line.split_once(|c: char| c.is_whitespace()) {
            Some(pair) => pair,
            None => continue,
        };
        let pid: u32 = match pid_str.trim().parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        if pid == current_pid {
            continue;
        }

        let args = rest.trim();

        // Filter to rows whose command contains the kimetsu binary.
        // Check the first token of args (the executable path/name).
        let exe_tok = args.split_whitespace().next().unwrap_or("").to_lowercase();
        if !exe_tok.contains("kimetsu") {
            continue;
        }

        let kind = classify_kind(args);
        let workspace = extract_workspace(args);

        // Try to extract just the executable path (first token).
        let exe_path = args
            .split_whitespace()
            .next()
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty());

        procs.push(KimetsuProc {
            pid,
            exe_path,
            command_line: Some(args.to_string()),
            kind,
            workspace,
        });
    }

    procs
}

// ──────────────────────────────────────────────────────────────────────────────
// CSV helper
// ──────────────────────────────────────────────────────────────────────────────

/// Parse a single CSV row, handling double-quoted fields (including embedded
/// commas and escaped double-quotes `""`).
fn parse_csv_row(row: &str) -> Vec<String> {
    let mut fields: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut chars = row.chars().peekable();
    let mut in_quotes = false;

    while let Some(ch) = chars.next() {
        match ch {
            '"' if in_quotes => {
                // Peek: if next is also `"` it's an escaped quote inside the field.
                if chars.peek() == Some(&'"') {
                    chars.next(); // consume the second `"`
                    current.push('"');
                } else {
                    in_quotes = false;
                }
            }
            '"' => {
                in_quotes = true;
            }
            ',' if !in_quotes => {
                fields.push(current.clone());
                current.clear();
            }
            other => {
                current.push(other);
            }
        }
    }
    fields.push(current);
    fields
}

// ──────────────────────────────────────────────────────────────────────────────
// Update pre-flight helper
// ──────────────────────────────────────────────────────────────────────────────

/// Return every kimetsu process whose exe path matches `target` (canonicalized,
/// case-insensitive on Windows).  Excludes the current process (same as
/// `list_kimetsu_processes`).
///
/// Used by `kimetsu update` to detect processes that hold the target binary
/// locked before attempting the replace, so the user can be offered a chance
/// to stop them interactively.
pub fn processes_locking_target(target: &Path) -> Vec<KimetsuProc> {
    let target_canon = target
        .canonicalize()
        .unwrap_or_else(|_| target.to_path_buf());
    let target_str = target_canon.to_string_lossy().to_lowercase();

    list_kimetsu_processes()
        .into_iter()
        .filter(|p| {
            p.exe_path
                .as_deref()
                .map(|exe| {
                    let exe_path = Path::new(exe);
                    let exe_canon = exe_path
                        .canonicalize()
                        .unwrap_or_else(|_| exe_path.to_path_buf());
                    exe_canon.to_string_lossy().to_lowercase() == target_str
                })
                .unwrap_or(false)
        })
        .collect()
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Windows CSV parser ───────────────────────────────────────────────────

    // Row 1004 uses a properly-quoted path-with-space (--workspace "C:\other project").
    // In real CIM CSV the outer field quotes are `"…"` and the inner quotes are `""`.
    const WINDOWS_CSV: &str = "\"ProcessId\",\"ExecutablePath\",\"CommandLine\"\n\
\"1001\",\"C:\\Users\\user\\.cargo\\bin\\kimetsu.exe\",\"kimetsu mcp serve --workspace C:\\proj\"\n\
\"1002\",\"C:\\Users\\user\\.cargo\\bin\\kimetsu.exe\",\"kimetsu chat --workspace D:\\code\\myapp\"\n\
\"1003\",\"C:\\Users\\user\\.cargo\\bin\\kimetsu.exe\",\"kimetsu brain pretool-hook\"\n\
\"9999\",\"C:\\Users\\user\\.cargo\\bin\\kimetsu.exe\",\"kimetsu ps\"\n\
\"1004\",\"\",\"kimetsu mcp serve --workspace \"\"C:\\other project\"\"\"\n\
\"badrow\"\n";

    #[test]
    fn windows_csv_count_excludes_current_pid() {
        // PID 9999 is the "current" process — must be excluded.
        let procs = parse_windows_proc_csv(WINDOWS_CSV, 9999);
        // 1001, 1002, 1003, 1004 = 4 (the "badrow" is malformed and skipped)
        assert_eq!(procs.len(), 4, "expected 4 procs, got {procs:?}");
    }

    #[test]
    fn windows_csv_kinds() {
        let procs = parse_windows_proc_csv(WINDOWS_CSV, 9999);
        // pid→kind map
        let by_pid: std::collections::HashMap<u32, &ProcKind> =
            procs.iter().map(|p| (p.pid, &p.kind)).collect();

        assert_eq!(
            by_pid[&1001],
            &ProcKind::McpServe,
            "1001 should be McpServe"
        );
        assert_eq!(by_pid[&1002], &ProcKind::Chat, "1002 should be Chat");
        assert_eq!(by_pid[&1003], &ProcKind::Hook, "1003 should be Hook");
        // 1004 has empty exe but valid cmdline with "mcp serve"
        assert_eq!(
            by_pid[&1004],
            &ProcKind::McpServe,
            "1004 should be McpServe"
        );
    }

    #[test]
    fn windows_csv_workspace_extraction() {
        let procs = parse_windows_proc_csv(WINDOWS_CSV, 9999);
        let by_pid: std::collections::HashMap<u32, &KimetsuProc> =
            procs.iter().map(|p| (p.pid, p)).collect();

        assert_eq!(
            by_pid[&1001].workspace.as_deref(),
            Some("C:\\proj"),
            "workspace for pid 1001"
        );
        assert_eq!(
            by_pid[&1002].workspace.as_deref(),
            Some("D:\\code\\myapp"),
            "workspace for pid 1002"
        );
        // Hook row has no --workspace
        assert_eq!(
            by_pid[&1003].workspace, None,
            "hook row should have no workspace"
        );
        // Path with spaces (quoted in the tokeniser)
        assert_eq!(
            by_pid[&1004].workspace.as_deref(),
            Some("C:\\other project"),
            "workspace with space"
        );
    }

    #[test]
    fn windows_csv_malformed_rows_skipped() {
        // Only "badrow" should be skipped; everything else should parse.
        let procs = parse_windows_proc_csv(WINDOWS_CSV, 9999);
        // Verify no pid=0 or garbage got through.
        assert!(
            procs.iter().all(|p| p.pid > 0),
            "all pids should be positive"
        );
    }

    #[test]
    fn windows_csv_empty_input() {
        assert!(parse_windows_proc_csv("", 1).is_empty());
    }

    #[test]
    fn windows_csv_header_only() {
        let header = "\"ProcessId\",\"ExecutablePath\",\"CommandLine\"\n";
        assert!(parse_windows_proc_csv(header, 1).is_empty());
    }

    // ── Unix ps parser ───────────────────────────────────────────────────────

    const UNIX_PS: &str = "\
  1001 /usr/local/bin/kimetsu mcp serve --workspace /home/user/proj
  1002 /usr/local/bin/kimetsu chat --workspace /home/user/code/myapp
  1003 /usr/local/bin/kimetsu brain pretool-hook
  9999 /usr/local/bin/kimetsu ps
  5555 /usr/bin/python3 some_other_script.py
  1004 /usr/local/bin/kimetsu mcp serve --workspace /path/to/other
  garbage line no pid
";

    #[test]
    fn unix_ps_count_excludes_current_and_non_kimetsu() {
        let procs = parse_unix_ps(UNIX_PS, 9999);
        // 1001, 1002, 1003, 1004 = 4 (python and self excluded; garbage row skipped)
        assert_eq!(procs.len(), 4, "got {procs:?}");
    }

    #[test]
    fn unix_ps_kinds() {
        let procs = parse_unix_ps(UNIX_PS, 9999);
        let by_pid: std::collections::HashMap<u32, &ProcKind> =
            procs.iter().map(|p| (p.pid, &p.kind)).collect();

        assert_eq!(by_pid[&1001], &ProcKind::McpServe);
        assert_eq!(by_pid[&1002], &ProcKind::Chat);
        assert_eq!(by_pid[&1003], &ProcKind::Hook);
        assert_eq!(by_pid[&1004], &ProcKind::McpServe);
    }

    #[test]
    fn unix_ps_workspace_extraction() {
        let procs = parse_unix_ps(UNIX_PS, 9999);
        let by_pid: std::collections::HashMap<u32, &KimetsuProc> =
            procs.iter().map(|p| (p.pid, p)).collect();

        assert_eq!(by_pid[&1001].workspace.as_deref(), Some("/home/user/proj"));
        assert_eq!(
            by_pid[&1002].workspace.as_deref(),
            Some("/home/user/code/myapp")
        );
        assert_eq!(by_pid[&1003].workspace, None, "hook has no workspace");
    }

    #[test]
    fn unix_ps_empty_input() {
        assert!(parse_unix_ps("", 1).is_empty());
    }

    // ── classify_kind edge cases ─────────────────────────────────────────────

    #[test]
    fn classify_mcp_serve_various_forms() {
        assert_eq!(classify_kind("kimetsu mcp serve"), ProcKind::McpServe);
        assert_eq!(
            classify_kind("kimetsu mcp serve --workspace /x"),
            ProcKind::McpServe
        );
        assert_eq!(
            classify_kind("/usr/bin/kimetsu mcp serve --no-user-skills"),
            ProcKind::McpServe
        );
    }

    #[test]
    fn classify_chat() {
        assert_eq!(classify_kind("kimetsu chat"), ProcKind::Chat);
        assert_eq!(classify_kind("kimetsu chat --workspace /x"), ProcKind::Chat);
        // Should NOT classify as chat if it's an mcp serve
        assert_ne!(classify_kind("kimetsu mcp serve"), ProcKind::Chat);
    }

    #[test]
    fn classify_hook_variants() {
        assert_eq!(classify_kind("kimetsu brain pretool-hook"), ProcKind::Hook);
        assert_eq!(classify_kind("kimetsu brain posttool-hook"), ProcKind::Hook);
        assert_eq!(classify_kind("kimetsu brain context-hook"), ProcKind::Hook);
        assert_eq!(classify_kind("kimetsu brain stop-hook"), ProcKind::Hook);
        assert_eq!(
            classify_kind("kimetsu brain session-end-hook"),
            ProcKind::Hook
        );
    }

    #[test]
    fn classify_cli_fallback() {
        assert_eq!(classify_kind("kimetsu update"), ProcKind::Cli);
        assert_eq!(classify_kind("kimetsu doctor"), ProcKind::Cli);
        assert_eq!(classify_kind("kimetsu brain status"), ProcKind::Cli);
    }

    // ── workspace extraction edge cases ─────────────────────────────────────

    #[test]
    fn workspace_no_flag() {
        assert_eq!(extract_workspace("kimetsu mcp serve"), None);
    }

    #[test]
    fn workspace_simple() {
        assert_eq!(
            extract_workspace("kimetsu mcp serve --workspace /home/user/proj"),
            Some("/home/user/proj".into())
        );
    }

    #[test]
    fn workspace_quoted_with_spaces() {
        // The tokenizer strips outer double quotes.
        assert_eq!(
            extract_workspace(r#"kimetsu mcp serve --workspace "C:\My Projects\foo""#),
            Some("C:\\My Projects\\foo".into())
        );
    }

    #[test]
    fn workspace_flag_at_end_no_value() {
        // --workspace with no following token → None (don't panic)
        assert_eq!(extract_workspace("kimetsu mcp serve --workspace"), None);
    }

    // ── stop_processes safety: never kills current PID ───────────────────────

    #[test]
    fn stop_processes_never_kills_self() {
        let current = std::process::id();
        // If stop_processes tried to kill the current PID it would be a catastrophic
        // self-termination during the test. We verify the result set never contains
        // the current pid even when explicitly passed.
        let pids = vec![current];
        let results = stop_processes(&pids);
        // The current pid must be filtered out — no result entry for it.
        assert!(
            results.iter().all(|(pid, _)| *pid != current),
            "stop_processes must never include the current PID in results"
        );
        assert!(
            results.is_empty(),
            "when only the current pid is given, nothing should be stopped"
        );
    }

    // ── CSV helper ──────────────────────────────────────────────────────────

    #[test]
    fn csv_row_quoted_comma() {
        // A field that contains a comma must be quoted.
        let row = r#""123","C:\foo,bar","kimetsu mcp serve""#;
        let cols = super::parse_csv_row(row);
        assert_eq!(cols, vec!["123", "C:\\foo,bar", "kimetsu mcp serve"]);
    }

    #[test]
    fn csv_row_escaped_quote() {
        let row = r#""123","he said ""hello""","kimetsu""#;
        let cols = super::parse_csv_row(row);
        assert_eq!(cols, vec!["123", r#"he said "hello""#, "kimetsu"]);
    }

    // ── processes_locking_target path filtering ──────────────────────────────

    /// Build a `KimetsuProc` with a specific exe_path for testing.
    fn make_proc(pid: u32, exe: &str) -> KimetsuProc {
        KimetsuProc {
            pid,
            exe_path: Some(exe.to_string()),
            command_line: Some(format!("{exe} mcp serve")),
            kind: ProcKind::McpServe,
            workspace: None,
        }
    }

    /// Build a `KimetsuProc` with no exe_path.
    fn make_proc_no_exe(pid: u32) -> KimetsuProc {
        KimetsuProc {
            pid,
            exe_path: None,
            command_line: Some("kimetsu mcp serve".to_string()),
            kind: ProcKind::McpServe,
            workspace: None,
        }
    }

    /// Thin version of `processes_locking_target` that accepts a pre-built
    /// process list instead of calling the live OS query.  This is the pure
    /// path-filter logic we want to unit-test.
    fn filter_locking(procs: Vec<KimetsuProc>, target: &Path) -> Vec<KimetsuProc> {
        let target_canon = target
            .canonicalize()
            .unwrap_or_else(|_| target.to_path_buf());
        let target_str = target_canon.to_string_lossy().to_lowercase();

        procs
            .into_iter()
            .filter(|p| {
                p.exe_path
                    .as_deref()
                    .map(|exe| {
                        let exe_path = Path::new(exe);
                        let exe_canon = exe_path
                            .canonicalize()
                            .unwrap_or_else(|_| exe_path.to_path_buf());
                        exe_canon.to_string_lossy().to_lowercase() == target_str
                    })
                    .unwrap_or(false)
            })
            .collect()
    }

    #[test]
    fn locking_filter_returns_matching_procs() {
        // Use a path that doesn't exist so canonicalize falls back to as-is on
        // both sides — comparison is purely lexicographic in that case.
        let target = Path::new(r"C:\fake\nonexistent\kimetsu.exe");
        let procs = vec![
            make_proc(101, r"C:\fake\nonexistent\kimetsu.exe"),
            make_proc(102, r"C:\other\kimetsu.exe"),
            make_proc(103, r"C:\fake\nonexistent\kimetsu.exe"),
        ];
        let result = filter_locking(procs, target);
        let pids: Vec<u32> = result.iter().map(|p| p.pid).collect();
        assert_eq!(pids, vec![101, 103], "only procs whose path matches target");
    }

    #[test]
    fn locking_filter_case_insensitive() {
        // On Windows path comparisons must be case-insensitive.
        let target = Path::new(r"C:\FAKE\nonexistent\KIMETSU.EXE");
        let procs = vec![
            make_proc(201, r"C:\fake\nonexistent\kimetsu.exe"),
            make_proc(202, r"D:\other\kimetsu.exe"),
        ];
        let result = filter_locking(procs, target);
        // PID 201 matches (different case); PID 202 does not.
        let pids: Vec<u32> = result.iter().map(|p| p.pid).collect();
        assert_eq!(pids, vec![201], "case-insensitive path match");
    }

    #[test]
    fn locking_filter_excludes_procs_with_no_exe() {
        let target = Path::new(r"C:\fake\kimetsu.exe");
        let procs = vec![
            make_proc_no_exe(301),
            make_proc(302, r"C:\fake\kimetsu.exe"),
        ];
        let result = filter_locking(procs, target);
        let pids: Vec<u32> = result.iter().map(|p| p.pid).collect();
        assert_eq!(pids, vec![302], "proc with no exe_path must be excluded");
    }

    #[test]
    fn locking_filter_empty_list_returns_empty() {
        let target = Path::new(r"C:\fake\kimetsu.exe");
        let result = filter_locking(vec![], target);
        assert!(result.is_empty());
    }

    #[test]
    fn locking_filter_no_match_returns_empty() {
        let target = Path::new(r"C:\fake\kimetsu.exe");
        let procs = vec![
            make_proc(401, r"C:\other\kimetsu.exe"),
            make_proc(402, r"D:\somewhere\kimetsu.exe"),
        ];
        let result = filter_locking(procs, target);
        assert!(result.is_empty(), "no proc matches target path");
    }
}
