use std::collections::HashSet;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use kimetsu_brain::trace::{RunPaths, TraceWriter};
use kimetsu_core::KimetsuResult;
use kimetsu_core::event::Event;
use kimetsu_core::ids::{EventId, RunId, new_id};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use similar::TextDiff;

const TRACE_STRING_CAP_BYTES: usize = 64 * 1024;
const SHELL_ARTIFACT_THRESHOLD_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone)]
pub struct ToolRuntimeConfig {
    pub default_timeout_secs: u64,
    pub max_timeout_secs: u64,
    pub model_output_cap_bytes: usize,
    pub disk_output_cap_bytes: usize,
    pub redact_secrets: bool,
    pub trace_fsync: bool,
    pub env_allowlist_extra: Vec<String>,
}

impl Default for ToolRuntimeConfig {
    fn default() -> Self {
        Self {
            default_timeout_secs: 60,
            max_timeout_secs: 600,
            model_output_cap_bytes: 64 * 1024,
            disk_output_cap_bytes: 256 * 1024 * 1024,
            redact_secrets: true,
            trace_fsync: true,
            env_allowlist_extra: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ToolPatchPlan {
    pub files_to_modify: HashSet<String>,
    pub files_to_create: HashSet<String>,
    pub files_to_delete: HashSet<String>,
}

impl ToolPatchPlan {
    pub fn allow_modify(paths: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            files_to_modify: paths.into_iter().map(Into::into).collect(),
            ..Self::default()
        }
    }
}

/// MP-7c: raw output of a shell command, before redaction / capping /
/// artifact writing. Returned by `ShellExecutor::execute`.
#[derive(Debug, Clone, Default)]
pub struct RawShellOutput {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
    pub duration_ms: u64,
}

/// MP-7c: pluggable shell backend. `LocalShellExecutor` (default) runs
/// the subprocess via `std::process::Command::new` on the host machine.
/// Benchmark adapters can supply their own executor; for example
/// `kimetsu-harbor-rs` proxies calls through the Kimetsu/Harbor JSON-RPC
/// protocol so commands run inside the Terminal-Bench container.
///
/// The trait is intentionally narrow - only `execute` - so adding a new
/// backend (Daytona, Vercel Sandbox, etc.) is one impl block.
pub trait ShellExecutor {
    fn execute(
        &mut self,
        repo_root: &Path,
        spec: &CommandSpec,
        config: &ToolRuntimeConfig,
    ) -> KimetsuResult<RawShellOutput>;
}

/// Default executor: runs the subprocess locally. Behavior is identical
/// to what `shell_command_inner` did before MP-7c - `env_clear` plus an
/// allowlist of host env vars, hard timeout with process-tree kill.
pub struct LocalShellExecutor;

impl ShellExecutor for LocalShellExecutor {
    fn execute(
        &mut self,
        repo_root: &Path,
        spec: &CommandSpec,
        config: &ToolRuntimeConfig,
    ) -> KimetsuResult<RawShellOutput> {
        let cwd = resolve_cwd_under(repo_root, spec.cwd_relative.as_deref().unwrap_or("."))?;
        let timeout_secs = spec
            .timeout_secs
            .unwrap_or(config.default_timeout_secs)
            .min(config.max_timeout_secs);
        let timeout = Duration::from_secs(timeout_secs);
        let started = Instant::now();

        let mut command = Command::new(&spec.program);
        command
            .args(&spec.args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_clear();
        for key in default_env_allowlist(&config.env_allowlist_extra) {
            if let Some(value) = std::env::var_os(&key) {
                command.env(key, value);
            }
        }

        let mut child = command.spawn()?;
        let timed_out = loop {
            if child.try_wait()?.is_some() {
                break false;
            }
            if started.elapsed() >= timeout {
                kill_process_tree(&mut child);
                break true;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let output = child.wait_with_output()?;
        Ok(RawShellOutput {
            exit_code: output.status.code().unwrap_or(-1),
            stdout: output.stdout,
            stderr: output.stderr,
            timed_out,
            duration_ms: started.elapsed().as_millis() as u64,
        })
    }
}

pub struct ToolRuntime {
    repo_root: PathBuf,
    run_id: RunId,
    stage: String,
    trace_writer: Option<TraceWriter>,
    run_paths: Option<RunPaths>,
    active_plan: Option<ToolPatchPlan>,
    config: ToolRuntimeConfig,
    /// MP-7c: backend that actually runs `shell_command` invocations.
    /// `LocalShellExecutor` is the default; benchmark adapters can swap in
    /// a transport-specific executor to route through their bridge.
    shell_executor: Box<dyn ShellExecutor>,
}

impl ToolRuntime {
    pub fn new(repo_root: impl AsRef<Path>, run_id: RunId) -> KimetsuResult<Self> {
        Ok(Self {
            repo_root: repo_root.as_ref().canonicalize()?,
            run_id,
            stage: "tool_test".to_string(),
            trace_writer: None,
            run_paths: None,
            active_plan: None,
            config: ToolRuntimeConfig::default(),
            shell_executor: Box::new(LocalShellExecutor),
        })
    }

    pub fn with_trace(mut self, writer: TraceWriter, run_paths: RunPaths) -> Self {
        self.trace_writer = Some(writer);
        self.run_paths = Some(run_paths);
        self
    }

    pub fn with_config(mut self, config: ToolRuntimeConfig) -> Self {
        self.config = config;
        self
    }

    pub fn with_stage(mut self, stage: impl Into<String>) -> Self {
        self.stage = stage.into();
        self
    }

    /// MP-7c: swap the shell backend. Benchmark adapters use this to route
    /// `shell_command` calls through their container; pass
    /// `LocalShellExecutor` (the default) for normal host-side runs.
    pub fn with_shell_executor(mut self, executor: Box<dyn ShellExecutor>) -> Self {
        self.shell_executor = executor;
        self
    }

    pub fn into_trace(self) -> Option<(TraceWriter, RunPaths)> {
        match (self.trace_writer, self.run_paths) {
            (Some(writer), Some(run_paths)) => Some((writer, run_paths)),
            _ => None,
        }
    }

    pub fn set_patch_plan(&mut self, plan: ToolPatchPlan) {
        self.active_plan = Some(plan);
    }

    pub fn run_id(&self) -> RunId {
        self.run_id
    }

    pub fn append_trace_event(&mut self, event: &Event, fsync: bool) -> KimetsuResult<()> {
        if let Some(writer) = &mut self.trace_writer {
            writer.append(event, fsync)?;
        }
        Ok(())
    }

    pub fn write_artifact(
        &self,
        file_name: impl AsRef<str>,
        bytes: &[u8],
    ) -> KimetsuResult<Option<String>> {
        let Some(run_paths) = &self.run_paths else {
            return Ok(None);
        };
        fs::create_dir_all(&run_paths.artifacts_dir)?;
        let file_name = file_name.as_ref();
        if file_name.contains('/') || file_name.contains('\\') || file_name.contains("..") {
            return Err("invalid artifact file name".into());
        }
        let path = run_paths.artifacts_dir.join(file_name);
        fs::write(path, bytes)?;
        Ok(Some(format!("artifacts/{file_name}")))
    }

    pub fn read_file(&mut self, input: ReadFileInput) -> KimetsuResult<ReadFileOutput> {
        let call = self.begin_tool("read_file", &input)?;
        let started = Instant::now();
        let result = self.read_file_inner(&input);
        self.finish_tool(call, started, result)
    }

    pub fn read_file_lines(
        &mut self,
        input: ReadFileLinesInput,
    ) -> KimetsuResult<ReadFileLinesOutput> {
        let call = self.begin_tool("read_file", &input)?;
        let started = Instant::now();
        let result = self.read_file_lines_inner(&input).map(|mut output| {
            output.duration_ms = started.elapsed().as_millis() as u64;
            output
        });
        self.finish_tool(call, started, result)
    }

    pub fn multi_read_lines(
        &mut self,
        input: MultiReadLinesInput,
    ) -> KimetsuResult<MultiReadLinesOutput> {
        let call = self.begin_tool("multi_read", &input)?;
        let started = Instant::now();
        let result = self.multi_read_lines_inner(&input);
        self.finish_tool(call, started, result)
    }

    pub fn list_files(&mut self, input: ListFilesInput) -> KimetsuResult<ListFilesOutput> {
        let call = self.begin_tool("list_files", &input)?;
        let started = Instant::now();
        let result = self.list_files_inner(&input);
        self.finish_tool(call, started, result)
    }

    pub fn search_files(&mut self, input: SearchFilesInput) -> KimetsuResult<SearchFilesOutput> {
        let call = self.begin_tool("search_files", &input)?;
        let started = Instant::now();
        let result = self.search_files_inner(&input);
        self.finish_tool(call, started, result)
    }

    pub fn git_status(&mut self) -> KimetsuResult<GitStatusOutput> {
        let input = serde_json::json!({});
        let call = self.begin_tool("git_status", &input)?;
        let started = Instant::now();
        let result = self.git_status_inner();
        self.finish_tool(call, started, result)
    }

    pub fn git_diff(&mut self, input: GitDiffInput) -> KimetsuResult<GitDiffOutput> {
        let call = self.begin_tool("git_diff", &input)?;
        let started = Instant::now();
        let result = self.git_diff_inner(&input);
        self.finish_tool(call, started, result)
    }

    pub fn shell_command(&mut self, input: CommandSpec) -> KimetsuResult<ShellCommandOutput> {
        let call = self.begin_tool("shell_command", &input)?;
        let started = Instant::now();
        let result = self.shell_command_inner(&input);
        self.finish_tool(call, started, result)
    }

    pub fn apply_patch(&mut self, input: ApplyPatchInput) -> KimetsuResult<ApplyPatchOutput> {
        let call = self.begin_tool("apply_patch", &input)?;
        let started = Instant::now();
        let result = self.apply_patch_inner(&input);
        self.finish_tool(call, started, result)
    }

    pub fn call_tool(&mut self, name: &str, input: Value) -> KimetsuResult<Value> {
        match name {
            "read_file" => Ok(serde_json::to_value(
                self.read_file(serde_json::from_value(input)?)?,
            )?),
            "list_files" => Ok(serde_json::to_value(
                self.list_files(serde_json::from_value(input)?)?,
            )?),
            "search_files" => Ok(serde_json::to_value(
                self.search_files(serde_json::from_value(input)?)?,
            )?),
            "git_status" => Ok(serde_json::to_value(self.git_status()?)?),
            "git_diff" => Ok(serde_json::to_value(
                self.git_diff(serde_json::from_value(input)?)?,
            )?),
            "shell_command" => Ok(serde_json::to_value(
                self.shell_command(serde_json::from_value(input)?)?,
            )?),
            "apply_patch" => Ok(serde_json::to_value(
                self.apply_patch(serde_json::from_value(input)?)?,
            )?),
            _ => Err(format!("unknown tool: {name}").into()),
        }
    }

    fn begin_tool(&mut self, tool: &str, input: &impl Serialize) -> KimetsuResult<ToolCallTrace> {
        let tool_call_id = new_id().to_string();
        let input_json = serde_json::to_value(input)?;
        let event = Event::new(
            self.run_id,
            "tool.called",
            serde_json::json!({
                "stage": &self.stage,
                "tool_call_id": tool_call_id,
                "tool": tool,
                "input_json": redact_value(input_json, self.config.redact_secrets),
            }),
        );
        if let Some(writer) = &mut self.trace_writer {
            writer.append(&event, self.config.trace_fsync)?;
        }
        Ok(ToolCallTrace {
            tool_call_id,
            tool: tool.to_string(),
        })
    }

    fn finish_tool<T: Serialize>(
        &mut self,
        call: ToolCallTrace,
        started: Instant,
        result: KimetsuResult<T>,
    ) -> KimetsuResult<T> {
        let duration_ms = started.elapsed().as_millis() as u64;
        match result {
            Ok(output) => {
                let output_json = serde_json::to_value(&output)?;
                let event = Event::new(
                    self.run_id,
                    "tool.completed",
                    serde_json::json!({
                        "tool_call_id": call.tool_call_id,
                        "output_json": redact_value(output_json, self.config.redact_secrets),
                        "duration_ms": duration_ms,
                    }),
                );
                if let Some(writer) = &mut self.trace_writer {
                    writer.append(&event, self.config.trace_fsync)?;
                }
                Ok(output)
            }
            Err(err) => {
                let event = Event::new(
                    self.run_id,
                    "tool.failed",
                    serde_json::json!({
                        "tool_call_id": call.tool_call_id,
                        "tool": call.tool,
                        "category": "Tool",
                        "message": err.to_string(),
                        "duration_ms": duration_ms,
                    }),
                );
                if let Some(writer) = &mut self.trace_writer {
                    writer.append(&event, self.config.trace_fsync)?;
                }
                Err(err)
            }
        }
    }

    fn read_file_inner(&self, input: &ReadFileInput) -> KimetsuResult<ReadFileOutput> {
        let path = normalize_repo_path(&input.path)?;
        reject_protected_repo_path(&path)?;
        let full_path = self.resolve_existing(&path)?;
        let bytes = fs::read(&full_path)?;
        if looks_binary(&bytes) {
            return Err("binary".into());
        }

        let max_bytes = input.max_bytes.unwrap_or(64 * 1024) as usize;
        let truncated = bytes.len() > max_bytes;
        let visible = &bytes[..bytes.len().min(max_bytes)];
        let content = String::from_utf8_lossy(visible).to_string();

        Ok(ReadFileOutput {
            path,
            content,
            truncated,
            hash: blake3::hash(&bytes).to_hex().to_string(),
            size: bytes.len() as u64,
        })
    }

    fn read_file_lines_inner(
        &self,
        input: &ReadFileLinesInput,
    ) -> KimetsuResult<ReadFileLinesOutput> {
        let path = normalize_repo_path(&input.path)?;
        reject_protected_repo_path(&path)?;
        let full_path = self.resolve_existing(&path)?;
        if full_path.is_dir() {
            return Err("path_is_directory".into());
        }
        if looks_binary_prefix(&full_path)? {
            return Err("binary".into());
        }

        let offset = input.offset.max(1);
        let limit = input.limit;
        let end_line = offset.saturating_add(limit).saturating_sub(1);
        let file = fs::File::open(&full_path)?;
        let mut reader = BufReader::new(file);
        let mut buf = String::new();
        let mut content = String::new();
        let mut line_no = 0u32;
        let mut returned = 0u32;

        loop {
            buf.clear();
            let read = reader.read_line(&mut buf)?;
            if read == 0 {
                break;
            }
            line_no = line_no.saturating_add(1);
            if line_no >= offset && line_no <= end_line {
                content.push_str(&buf);
                returned = returned.saturating_add(1);
            }
        }

        let truncated = line_no > end_line;
        Ok(ReadFileLinesOutput {
            path,
            content,
            offset,
            limit,
            lines_returned: returned,
            lines_total: Some(line_no),
            truncated,
            duration_ms: 0,
        })
    }

    fn multi_read_lines_inner(
        &self,
        input: &MultiReadLinesInput,
    ) -> KimetsuResult<MultiReadLinesOutput> {
        let started = Instant::now();
        let mut files = Vec::with_capacity(input.files.len());
        for file in &input.files {
            let file_started = Instant::now();
            match self.read_file_lines_inner(file) {
                Ok(mut output) => {
                    output.duration_ms = file_started.elapsed().as_millis() as u64;
                    files.push(serde_json::to_value(output)?);
                }
                Err(err) => {
                    files.push(serde_json::json!({
                        "error": err.to_string(),
                        "path": file.path.clone(),
                    }));
                }
            }
        }
        Ok(MultiReadLinesOutput {
            files_returned: files.len() as u32,
            duration_ms: started.elapsed().as_millis() as u64,
            files,
        })
    }

    fn list_files_inner(&self, input: &ListFilesInput) -> KimetsuResult<ListFilesOutput> {
        let dir = normalize_repo_path(input.dir.as_deref().unwrap_or("."))?;
        reject_protected_repo_path(&dir)?;
        let root = if dir == "." {
            self.repo_root.clone()
        } else {
            self.resolve_existing(&dir)?
        };
        let max_depth = input.depth.unwrap_or(2);
        let mut entries = Vec::new();
        self.walk_list(&root, 0, max_depth, &mut entries)?;
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(ListFilesOutput { entries })
    }

    fn search_files_inner(&self, input: &SearchFilesInput) -> KimetsuResult<SearchFilesOutput> {
        let regex = Regex::new(&input.pattern).map_err(|err| format!("invalid_regex: {err}"))?;
        let max_results = input.max_results.unwrap_or(100) as usize;
        let mut matches = Vec::new();
        self.walk_search(
            &self.repo_root,
            &regex,
            input.glob.as_deref(),
            max_results,
            &mut matches,
        )?;
        Ok(SearchFilesOutput { matches })
    }

    fn git_status_inner(&self) -> KimetsuResult<GitStatusOutput> {
        let output = run_direct(
            &self.repo_root,
            "git",
            &["status", "--porcelain=v2", "--branch"],
            Duration::from_secs(30),
            &default_env_allowlist(&self.config.env_allowlist_extra),
        )?;
        let porcelain_v2 = output.stdout;
        let (branch, ahead, behind) = parse_git_branch(&porcelain_v2);
        Ok(GitStatusOutput {
            porcelain_v2,
            branch,
            ahead,
            behind,
        })
    }

    fn git_diff_inner(&self, input: &GitDiffInput) -> KimetsuResult<GitDiffOutput> {
        let mut args = vec!["diff".to_string(), "--no-ext-diff".to_string()];
        if input.staged.unwrap_or(false) {
            args.push("--staged".to_string());
        }
        args.push("--".to_string());
        if let Some(paths) = &input.paths {
            for path in paths {
                args.push(normalize_repo_path(path)?);
            }
        }
        let args_ref = args.iter().map(String::as_str).collect::<Vec<_>>();
        let output = run_direct(
            &self.repo_root,
            "git",
            &args_ref,
            Duration::from_secs(30),
            &default_env_allowlist(&self.config.env_allowlist_extra),
        )?;
        let cap = self.config.model_output_cap_bytes;
        let truncated = output.stdout.len() > cap;
        let unified_diff = output.stdout.chars().take(cap).collect::<String>();
        Ok(GitDiffOutput {
            unified_diff,
            truncated,
            full_artifact: None,
        })
    }

    fn shell_command_inner(&mut self, input: &CommandSpec) -> KimetsuResult<ShellCommandOutput> {
        validate_command_policy(input)?;

        // MP-7c: subprocess work delegated to the shell executor. The
        // executor decides whether to run locally or proxy through a
        // transport bridge; everything around it (redaction,
        // artifact write-out, capped summaries) is pipeline-side and
        // identical regardless of where the command ran.
        let raw = self
            .shell_executor
            .execute(&self.repo_root, input, &self.config)?;

        let capped_stdout = summarize_shell_stream(
            &raw.stdout,
            self.config.model_output_cap_bytes,
            self.config.redact_secrets,
        );
        let capped_stderr = summarize_shell_stream(
            &raw.stderr,
            self.config.model_output_cap_bytes,
            self.config.redact_secrets,
        );
        let unexpected_exit = raw.exit_code != input.expected_exit.unwrap_or(0);
        let stdout_artifact_needed =
            should_write_shell_artifact(&raw.stdout, unexpected_exit, raw.timed_out);
        let stderr_artifact_needed =
            should_write_shell_artifact(&raw.stderr, unexpected_exit, raw.timed_out);
        let (stdout_artifact, stderr_artifact) = self.write_stream_artifacts(
            &raw.stdout,
            stdout_artifact_needed,
            &raw.stderr,
            stderr_artifact_needed,
        )?;

        Ok(ShellCommandOutput {
            exit_code: raw.exit_code,
            stdout_summary: capped_stdout,
            stderr_summary: capped_stderr,
            stdout_artifact,
            stderr_artifact,
            duration_ms: raw.duration_ms,
            timed_out: raw.timed_out,
            killed_for_policy: false,
        })
    }

    fn apply_patch_inner(&self, input: &ApplyPatchInput) -> KimetsuResult<ApplyPatchOutput> {
        let Some(plan) = &self.active_plan else {
            return Err("op_not_in_patch_plan: no active patch plan".into());
        };

        let mut applied_files = Vec::new();
        let mut unified_diff = String::new();

        for change in &input.changes {
            let path = normalize_repo_path(&change.path)?;
            reject_protected_repo_path(&path)?;
            validate_plan_allows(plan, &path, change.op)?;
            let full_path = self.resolve_for_write(&path)?;

            match change.op {
                PatchOp::Create => {
                    if full_path.exists() {
                        return Err(format!("file_already_exists: {path}").into());
                    }
                    let content = change.content.as_deref().unwrap_or("");
                    if let Some(parent) = full_path.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    fs::write(&full_path, content)?;
                    unified_diff.push_str(&render_diff(&path, "", content));
                }
                PatchOp::Modify => {
                    let expected_hash = change
                        .expected_hash
                        .as_deref()
                        .ok_or("expected_hash_required")?;
                    let old_bytes =
                        fs::read(&full_path).map_err(|_| format!("file_missing: {path}"))?;
                    let actual_hash = blake3::hash(&old_bytes).to_hex().to_string();
                    if actual_hash != expected_hash {
                        return Err(format!("file_changed_since_read: {path}").into());
                    }
                    if looks_binary(&old_bytes) {
                        return Err(format!("binary: {path}").into());
                    }
                    let old = String::from_utf8_lossy(&old_bytes).to_string();
                    let new = change.content.as_deref().unwrap_or("");
                    fs::write(&full_path, new)?;
                    unified_diff.push_str(&render_diff(&path, &old, new));
                }
                PatchOp::Delete => {
                    let expected_hash = change
                        .expected_hash
                        .as_deref()
                        .ok_or("expected_hash_required")?;
                    let old_bytes =
                        fs::read(&full_path).map_err(|_| format!("file_missing: {path}"))?;
                    let actual_hash = blake3::hash(&old_bytes).to_hex().to_string();
                    if actual_hash != expected_hash {
                        return Err(format!("file_changed_since_read: {path}").into());
                    }
                    if looks_binary(&old_bytes) {
                        return Err(format!("binary: {path}").into());
                    }
                    let old = String::from_utf8_lossy(&old_bytes).to_string();
                    fs::remove_file(&full_path)?;
                    unified_diff.push_str(&render_diff(&path, &old, ""));
                }
            }

            applied_files.push(path);
        }

        Ok(ApplyPatchOutput {
            applied_files,
            unified_diff,
        })
    }

    fn resolve_existing(&self, repo_path: &str) -> KimetsuResult<PathBuf> {
        let full = self.repo_root.join(repo_path);
        let canonical = full.canonicalize()?;
        ensure_inside(&self.repo_root, &canonical)?;
        Ok(canonical)
    }

    fn resolve_for_write(&self, repo_path: &str) -> KimetsuResult<PathBuf> {
        let full = self.repo_root.join(repo_path);
        let parent = full.parent().ok_or("path has no parent")?;
        let canonical_parent = if parent.exists() {
            parent.canonicalize()?
        } else {
            nearest_existing_parent(parent)?.canonicalize()?
        };
        ensure_inside(&self.repo_root, &canonical_parent)?;
        if let Ok(metadata) = fs::symlink_metadata(&full)
            && metadata.file_type().is_symlink()
        {
            return Err(format!("refusing_to_write_symlink: {repo_path}").into());
        }
        Ok(full)
    }

    fn walk_list(
        &self,
        dir: &Path,
        depth: u32,
        max_depth: u32,
        entries: &mut Vec<FileEntry>,
    ) -> KimetsuResult<()> {
        if depth > max_depth {
            return Ok(());
        }
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if should_skip_path(&path) {
                continue;
            }
            let metadata = entry.metadata()?;
            let rel = path_to_repo_rel(&self.repo_root, &path)?;
            if is_protected_repo_path(&rel) {
                continue;
            }
            let kind = if metadata.is_dir() { "dir" } else { "file" };
            entries.push(FileEntry {
                path: rel,
                kind: kind.to_string(),
                size: metadata.len(),
                mtime: metadata
                    .modified()
                    .ok()
                    .map(time::OffsetDateTime::from)
                    .and_then(|value| {
                        value
                            .format(&time::format_description::well_known::Rfc3339)
                            .ok()
                    })
                    .unwrap_or_default(),
            });
            if metadata.is_dir() {
                self.walk_list(&path, depth + 1, max_depth, entries)?;
            }
        }
        Ok(())
    }

    fn walk_search(
        &self,
        dir: &Path,
        regex: &Regex,
        glob: Option<&str>,
        max_results: usize,
        matches: &mut Vec<SearchMatch>,
    ) -> KimetsuResult<()> {
        if matches.len() >= max_results {
            return Ok(());
        }
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if should_skip_path(&path) {
                continue;
            }
            let metadata = entry.metadata()?;
            if metadata.is_dir() {
                self.walk_search(&path, regex, glob, max_results, matches)?;
                continue;
            }
            let rel = path_to_repo_rel(&self.repo_root, &path)?;
            if is_protected_repo_path(&rel) {
                continue;
            }
            if let Some(glob) = glob
                && !simple_glob_match(&rel, glob)
            {
                continue;
            }
            if metadata.len() > 512 * 1024 {
                continue;
            }
            let bytes = fs::read(&path)?;
            if looks_binary(&bytes) {
                continue;
            }
            let content = String::from_utf8_lossy(&bytes);
            for (line_idx, line) in content.lines().enumerate() {
                for mat in regex.find_iter(line) {
                    matches.push(SearchMatch {
                        path: rel.clone(),
                        line: (line_idx + 1) as u32,
                        column: (mat.start() + 1) as u32,
                        excerpt: line.chars().take(240).collect(),
                    });
                    if matches.len() >= max_results {
                        return Ok(());
                    }
                }
            }
        }
        Ok(())
    }

    fn write_stream_artifacts(
        &self,
        stdout: &[u8],
        write_stdout: bool,
        stderr: &[u8],
        write_stderr: bool,
    ) -> KimetsuResult<(Option<String>, Option<String>)> {
        let Some(run_paths) = &self.run_paths else {
            return Ok((None, None));
        };
        if !write_stdout && !write_stderr {
            return Ok((None, None));
        }
        fs::create_dir_all(&run_paths.artifacts_dir)?;
        let id = EventId(new_id()).to_string();
        let stdout_artifact = if write_stdout {
            let stdout_path = run_paths.artifacts_dir.join(format!("{id}.stdout"));
            fs::write(
                &stdout_path,
                shell_artifact_bytes(
                    stdout,
                    self.config.disk_output_cap_bytes,
                    self.config.redact_secrets,
                ),
            )?;
            Some(format!("artifacts/{id}.stdout"))
        } else {
            None
        };
        let stderr_artifact = if write_stderr {
            let stderr_path = run_paths.artifacts_dir.join(format!("{id}.stderr"));
            fs::write(
                &stderr_path,
                shell_artifact_bytes(
                    stderr,
                    self.config.disk_output_cap_bytes,
                    self.config.redact_secrets,
                ),
            )?;
            Some(format!("artifacts/{id}.stderr"))
        } else {
            None
        };
        Ok((stdout_artifact, stderr_artifact))
    }
}

#[derive(Debug, Clone)]
struct ToolCallTrace {
    tool_call_id: String,
    tool: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadFileInput {
    pub path: String,
    pub max_bytes: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadFileOutput {
    pub path: String,
    pub content: String,
    pub truncated: bool,
    pub hash: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadFileLinesInput {
    pub path: String,
    pub offset: u32,
    pub limit: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadFileLinesOutput {
    pub path: String,
    pub content: String,
    pub offset: u32,
    pub limit: u32,
    pub lines_returned: u32,
    pub lines_total: Option<u32>,
    pub truncated: bool,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultiReadLinesInput {
    pub files: Vec<ReadFileLinesInput>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultiReadLinesOutput {
    pub files: Vec<Value>,
    pub files_returned: u32,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchFilesInput {
    pub pattern: String,
    pub glob: Option<String>,
    pub max_results: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchFilesOutput {
    pub matches: Vec<SearchMatch>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchMatch {
    pub path: String,
    pub line: u32,
    pub column: u32,
    pub excerpt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListFilesInput {
    pub dir: Option<String>,
    pub depth: Option<u32>,
    pub glob: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListFilesOutput {
    pub entries: Vec<FileEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: String,
    pub kind: String,
    pub size: u64,
    pub mtime: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd_relative: Option<String>,
    pub timeout_secs: Option<u64>,
    pub expected_exit: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellCommandOutput {
    pub exit_code: i32,
    pub stdout_summary: String,
    pub stderr_summary: String,
    pub stdout_artifact: Option<String>,
    pub stderr_artifact: Option<String>,
    pub duration_ms: u64,
    pub timed_out: bool,
    pub killed_for_policy: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitStatusOutput {
    pub porcelain_v2: String,
    pub branch: String,
    pub ahead: u32,
    pub behind: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitDiffInput {
    pub staged: Option<bool>,
    pub paths: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitDiffOutput {
    pub unified_diff: String,
    pub truncated: bool,
    pub full_artifact: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyPatchInput {
    pub changes: Vec<PatchChange>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchChange {
    pub path: String,
    pub op: PatchOp,
    pub content: Option<String>,
    pub expected_hash: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PatchOp {
    Create,
    Modify,
    Delete,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyPatchOutput {
    pub applied_files: Vec<String>,
    pub unified_diff: String,
}

#[derive(Debug)]
struct DirectOutput {
    stdout: String,
}

/// MP-7c: free-function variant of `ToolRuntime::resolve_cwd` so the
/// `LocalShellExecutor` (which doesn't hold a `ToolRuntime` reference)
/// can resolve a repo-relative cwd against any repo root.
fn resolve_cwd_under(repo_root: &Path, repo_path: &str) -> KimetsuResult<PathBuf> {
    let path = normalize_repo_path(repo_path)?;
    let full = if path == "." {
        repo_root.to_path_buf()
    } else {
        let full = repo_root.join(&path);
        let canonical = full.canonicalize()?;
        ensure_inside(repo_root, &canonical)?;
        canonical
    };
    if !full.is_dir() {
        return Err("cwd_outside_repo".into());
    }
    ensure_inside(repo_root, &full)?;
    Ok(full)
}

fn normalize_repo_path(path: &str) -> KimetsuResult<String> {
    if path.trim().is_empty() || path == "." {
        return Ok(".".to_string());
    }
    let path = path.replace('\\', "/");
    let parsed = Path::new(&path);
    if parsed.is_absolute() || path.contains(':') {
        return Err("path_outside_repo".into());
    }
    let mut parts = Vec::new();
    for component in parsed.components() {
        match component {
            Component::Normal(part) => {
                let Some(part) = part.to_str() else {
                    return Err("path_outside_repo".into());
                };
                if part.is_empty() || part == "." || part == ".." {
                    return Err("path_outside_repo".into());
                }
                parts.push(part.to_string());
            }
            Component::CurDir => {}
            _ => return Err("path_outside_repo".into()),
        }
    }
    if parts.is_empty() {
        Ok(".".to_string())
    } else {
        Ok(parts.join("/"))
    }
}

fn ensure_inside(repo_root: &Path, path: &Path) -> KimetsuResult<()> {
    if !path.starts_with(repo_root) {
        return Err("path_outside_repo".into());
    }
    Ok(())
}

fn reject_protected_repo_path(path: &str) -> KimetsuResult<()> {
    if is_protected_repo_path(path) {
        return Err(format!("protected_path: {path}").into());
    }
    Ok(())
}

fn is_protected_repo_path(path: &str) -> bool {
    let normalized = path.replace('\\', "/");
    let mut parts = normalized.split('/');
    let first = parts.next().unwrap_or("");
    if matches!(first, ".git" | ".kimetsu") {
        return true;
    }

    let file_name = normalized
        .rsplit('/')
        .next()
        .unwrap_or(normalized.as_str())
        .to_ascii_lowercase();
    file_name == ".env"
        || file_name.starts_with(".env.")
        || file_name.ends_with(".pem")
        || file_name.ends_with(".key")
        || file_name.starts_with("id_rsa")
}

fn nearest_existing_parent(path: &Path) -> KimetsuResult<PathBuf> {
    let mut current = path;
    loop {
        if current.exists() {
            return Ok(current.to_path_buf());
        }
        current = current.parent().ok_or("path_outside_repo")?;
    }
}

fn path_to_repo_rel(repo_root: &Path, path: &Path) -> KimetsuResult<String> {
    let rel = path.strip_prefix(repo_root)?;
    let mut parts = Vec::new();
    for component in rel.components() {
        match component {
            Component::Normal(part) => {
                let Some(part) = part.to_str() else {
                    return Err("path_outside_repo".into());
                };
                parts.push(part.to_string());
            }
            _ => return Err("path_outside_repo".into()),
        }
    }
    Ok(parts.join("/"))
}

fn should_skip_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|value| value.to_str())
        .map(|name| {
            matches!(
                name,
                ".git" | ".kimetsu" | "target" | "node_modules" | ".venv" | "__pycache__"
            )
        })
        .unwrap_or(false)
}

fn looks_binary(bytes: &[u8]) -> bool {
    let scan_len = bytes.len().min(8192);
    bytes[..scan_len].contains(&0)
}

fn looks_binary_prefix(path: &Path) -> KimetsuResult<bool> {
    let mut file = fs::File::open(path)?;
    let mut buf = [0u8; 8192];
    let read = std::io::Read::read(&mut file, &mut buf)?;
    Ok(buf[..read].contains(&0))
}

fn validate_command_policy(input: &CommandSpec) -> KimetsuResult<()> {
    let program = Path::new(&input.program)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(&input.program)
        .to_ascii_lowercase();
    let program = program.trim_end_matches(".exe");

    if matches!(
        program,
        "curl" | "wget" | "nc" | "netcat" | "ssh" | "scp" | "sftp" | "ftp" | "rsync"
    ) {
        return Err(format!("policy_violation: network command blocked: {program}").into());
    }

    if matches!(
        program,
        "sh" | "bash"
            | "zsh"
            | "fish"
            | "cmd"
            | "powershell"
            | "pwsh"
            | "python"
            | "python3"
            | "py"
            | "node"
            | "deno"
            | "ruby"
            | "perl"
            | "php"
    ) {
        return Err(
            format!("policy_violation: shell/interpreter wrapper blocked: {program}").into(),
        );
    }

    if program == "git"
        && input
            .args
            .iter()
            .any(|arg| matches!(arg.as_str(), "push" | "commit" | "tag" | "reset"))
    {
        return Err("policy_violation: git write command blocked".into());
    }

    if matches!(program, "rm" | "rmdir" | "del" | "erase") {
        return Err("policy_violation: delete command blocked".into());
    }

    Ok(())
}

fn default_env_allowlist(extra: &[String]) -> Vec<String> {
    let mut values = [
        "PATH",
        "Path",
        "PATHEXT",
        "SystemRoot",
        "WINDIR",
        "COMSPEC",
        "HOME",
        "USERPROFILE",
        "TEMP",
        "TMP",
    ]
    .into_iter()
    .map(str::to_string)
    .collect::<Vec<_>>();
    values.extend(extra.iter().cloned());
    values.sort();
    values.dedup();
    values
}

fn run_direct(
    cwd: &Path,
    program: &str,
    args: &[&str],
    timeout: Duration,
    env_allowlist: &[String],
) -> KimetsuResult<DirectOutput> {
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_clear();
    for key in env_allowlist {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }

    let started = Instant::now();
    let mut child = command.spawn()?;
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if started.elapsed() >= timeout {
            kill_process_tree(&mut child);
            return Err(format!("command timed out: {program}").into());
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let output = child.wait_with_output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("command failed: {program}: {stderr}").into());
    }

    Ok(DirectOutput {
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
    })
}

fn kill_process_tree(child: &mut std::process::Child) {
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &child.id().to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
}

fn parse_git_branch(porcelain: &str) -> (String, u32, u32) {
    let mut branch = String::new();
    let mut ahead = 0;
    let mut behind = 0;

    for line in porcelain.lines() {
        if let Some(value) = line.strip_prefix("# branch.head ") {
            branch = value.to_string();
        }
        if let Some(value) = line.strip_prefix("# branch.ab ") {
            for part in value.split_whitespace() {
                if let Some(value) = part.strip_prefix('+') {
                    ahead = value.parse().unwrap_or(0);
                } else if let Some(value) = part.strip_prefix('-') {
                    behind = value.parse().unwrap_or(0);
                }
            }
        }
    }

    (branch, ahead, behind)
}

fn validate_plan_allows(plan: &ToolPatchPlan, path: &str, op: PatchOp) -> KimetsuResult<()> {
    let allowed = match op {
        PatchOp::Create => plan.files_to_create.contains(path),
        PatchOp::Modify => plan.files_to_modify.contains(path),
        PatchOp::Delete => plan.files_to_delete.contains(path),
    };
    if !allowed {
        return Err(format!("op_not_in_patch_plan: {path}").into());
    }
    Ok(())
}

fn render_diff(path: &str, old: &str, new: &str) -> String {
    TextDiff::from_lines(old, new)
        .unified_diff()
        .header(&format!("a/{path}"), &format!("b/{path}"))
        .to_string()
}

fn cap_string(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut capped = value.chars().take(max_bytes).collect::<String>();
    capped.push_str("\n<truncated>");
    capped
}

fn cap_bytes(value: &[u8], max_bytes: usize) -> &[u8] {
    &value[..value.len().min(max_bytes)]
}

fn summarize_shell_stream(bytes: &[u8], max_bytes: usize, redact: bool) -> String {
    let mut summary = String::from_utf8_lossy(cap_bytes(bytes, max_bytes)).to_string();
    if redact {
        summary = redact_text(&summary);
    }
    if bytes.len() > max_bytes {
        summary.push_str("\n<truncated>");
    }
    summary
}

fn should_write_shell_artifact(bytes: &[u8], unexpected_exit: bool, timed_out: bool) -> bool {
    !bytes.is_empty()
        && (unexpected_exit || timed_out || bytes.len() > SHELL_ARTIFACT_THRESHOLD_BYTES)
}

fn shell_artifact_bytes(bytes: &[u8], max_bytes: usize, redact: bool) -> Vec<u8> {
    let capped = cap_bytes(bytes, max_bytes);
    if redact {
        redact_text(&String::from_utf8_lossy(capped)).into_bytes()
    } else {
        capped.to_vec()
    }
}

fn redact_value(value: Value, enabled: bool) -> Value {
    if !enabled {
        return cap_value_strings(value);
    }
    match value {
        Value::String(value) => {
            Value::String(cap_string(&redact_text(&value), TRACE_STRING_CAP_BYTES))
        }
        Value::Array(values) => Value::Array(
            values
                .into_iter()
                .map(|value| redact_value(value, enabled))
                .collect(),
        ),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, value)| (key, redact_value(value, enabled)))
                .collect(),
        ),
        value => value,
    }
}

fn cap_value_strings(value: Value) -> Value {
    match value {
        Value::String(value) => Value::String(cap_string(&value, TRACE_STRING_CAP_BYTES)),
        Value::Array(values) => Value::Array(values.into_iter().map(cap_value_strings).collect()),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, value)| (key, cap_value_strings(value)))
                .collect(),
        ),
        value => value,
    }
}

fn redact_text(value: &str) -> String {
    let regex = Regex::new(r"(?i)(api[_-]?key|secret|password|token|bearer)[\s:=]+\S+")
        .expect("redaction regex is valid");
    regex.replace_all(value, "$1=<redacted>").to_string()
}

fn simple_glob_match(path: &str, glob: &str) -> bool {
    if glob == "*" {
        return true;
    }
    if let Some(suffix) = glob.strip_prefix("*.") {
        return path.ends_with(&format!(".{suffix}"));
    }
    if let Some(prefix) = glob.strip_suffix('*') {
        return path.starts_with(prefix);
    }
    path == glob
}

#[cfg(test)]
mod tests {
    use std::fs;

    use kimetsu_brain::project::init_project;
    use kimetsu_brain::trace::{TraceWriter, read_trace};
    use kimetsu_core::ids::{RunId, new_id};
    use kimetsu_core::paths::ProjectPaths;

    use super::*;

    #[test]
    fn file_tools_and_apply_patch_emit_trace_events() {
        let root = temp_project();
        fs::write(root.join("src.txt"), "hello TOKEN=secret\nworld\n").expect("write file");
        fs::write(root.join(".env"), "TOKEN=secret\n").expect("write env");
        init_project(&root, false).expect("init project");
        let paths = ProjectPaths::discover(&root).expect("paths");
        let run_id = RunId::new();
        let (writer, run_paths) = TraceWriter::create(&paths, run_id).expect("trace writer");
        let mut runtime = ToolRuntime::new(&root, run_id)
            .expect("runtime")
            .with_trace(writer, run_paths.clone());

        let read = runtime
            .read_file(ReadFileInput {
                path: "src.txt".to_string(),
                max_bytes: None,
            })
            .expect("read file");
        assert!(read.content.contains("hello"));
        let sliced = runtime
            .read_file_lines(ReadFileLinesInput {
                path: "src.txt".to_string(),
                offset: 2,
                limit: 1,
            })
            .expect("read file lines");
        assert_eq!(sliced.content, "world\n");
        assert_eq!(sliced.lines_total, Some(2));
        let multi = runtime
            .multi_read_lines(MultiReadLinesInput {
                files: vec![
                    ReadFileLinesInput {
                        path: "src.txt".to_string(),
                        offset: 1,
                        limit: 1,
                    },
                    ReadFileLinesInput {
                        path: "missing.txt".to_string(),
                        offset: 1,
                        limit: 1,
                    },
                ],
            })
            .expect("multi read lines");
        assert_eq!(multi.files_returned, 2);
        assert!(
            multi.files[0]["content"]
                .as_str()
                .unwrap()
                .contains("hello")
        );
        assert!(multi.files[1]["error"].as_str().is_some());
        assert!(
            runtime
                .read_file(ReadFileInput {
                    path: ".env".to_string(),
                    max_bytes: None,
                })
                .is_err(),
            "tool runtime must not expose .env contents"
        );
        let listed = runtime
            .list_files(ListFilesInput {
                dir: None,
                depth: Some(1),
                glob: None,
            })
            .expect("list files");
        assert!(
            listed.entries.iter().all(|entry| entry.path != ".env"),
            "list_files must hide protected secret paths"
        );

        let search = runtime
            .search_files(SearchFilesInput {
                pattern: "world".to_string(),
                glob: Some("*.txt".to_string()),
                max_results: Some(10),
            })
            .expect("search files");
        assert_eq!(search.matches.len(), 1);

        runtime.set_patch_plan(ToolPatchPlan::allow_modify(["src.txt"]));
        let patched = runtime
            .apply_patch(ApplyPatchInput {
                changes: vec![PatchChange {
                    path: "src.txt".to_string(),
                    op: PatchOp::Modify,
                    content: Some("hello\nkimetsu\n".to_string()),
                    expected_hash: Some(read.hash.clone()),
                }],
            })
            .expect("apply patch");
        assert_eq!(patched.applied_files, vec!["src.txt"]);
        assert!(patched.unified_diff.contains("kimetsu"));

        let stale = runtime.apply_patch(ApplyPatchInput {
            changes: vec![PatchChange {
                path: "src.txt".to_string(),
                op: PatchOp::Modify,
                content: Some("stale\n".to_string()),
                expected_hash: Some(read.hash),
            }],
        });
        assert!(stale.is_err());

        let events = read_trace(&run_paths.trace_jsonl).expect("read trace");
        assert!(events.iter().any(|event| event.kind == "tool.called"));
        assert!(events.iter().any(|event| event.kind == "tool.completed"));
        assert!(events.iter().any(|event| event.kind == "tool.failed"));

        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn shell_policy_blocks_network_and_allows_direct_programs() {
        let root = temp_project();
        init_project(&root, false).expect("init project");
        let run_id = RunId::new();
        let mut runtime = ToolRuntime::new(&root, run_id).expect("runtime");

        let blocked = runtime.shell_command(CommandSpec {
            program: "curl".to_string(),
            args: vec!["https://example.com".to_string()],
            cwd_relative: None,
            timeout_secs: Some(5),
            expected_exit: Some(0),
        });
        assert!(blocked.is_err());

        let wrapper = validate_command_policy(&CommandSpec {
            program: "powershell".to_string(),
            args: vec![
                "-Command".to_string(),
                "curl https://example.com".to_string(),
            ],
            cwd_relative: None,
            timeout_secs: Some(5),
            expected_exit: Some(0),
        });
        assert!(wrapper.is_err(), "shell wrappers must not bypass policy");

        let output = runtime
            .shell_command(CommandSpec {
                program: "rustc".to_string(),
                args: vec!["--version".to_string()],
                cwd_relative: None,
                timeout_secs: Some(10),
                expected_exit: Some(0),
            })
            .expect("rustc command");
        assert_eq!(output.exit_code, 0);
        assert!(output.stdout_summary.contains("rustc"));

        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn apply_patch_rejects_symlink_targets() {
        let root = temp_project();
        init_project(&root, false).expect("init project");
        let outside = std::env::temp_dir().join(format!("kimetsu-tools-outside-{}", new_id()));
        fs::write(&outside, "outside\n").expect("outside");
        let link = root.join("link.txt");
        if create_file_symlink(&outside, &link).is_err() {
            fs::remove_dir_all(root).ok();
            fs::remove_file(outside).ok();
            return;
        }

        let run_id = RunId::new();
        let mut runtime = ToolRuntime::new(&root, run_id).expect("runtime");
        runtime.set_patch_plan(ToolPatchPlan::allow_modify(["link.txt"]));
        let outside_hash = blake3::hash(&fs::read(&outside).expect("outside read"))
            .to_hex()
            .to_string();
        let err = runtime
            .apply_patch(ApplyPatchInput {
                changes: vec![PatchChange {
                    path: "link.txt".to_string(),
                    op: PatchOp::Modify,
                    content: Some("changed\n".to_string()),
                    expected_hash: Some(outside_hash),
                }],
            })
            .expect_err("symlink writes must be rejected");
        assert!(err.to_string().contains("refusing_to_write_symlink"));
        assert_eq!(
            fs::read_to_string(&outside).expect("outside read"),
            "outside\n"
        );
        fs::remove_dir_all(root).ok();
        fs::remove_file(outside).ok();
    }

    #[cfg(unix)]
    fn create_file_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }

    #[cfg(windows)]
    fn create_file_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::windows::fs::symlink_file(target, link)
    }

    fn temp_project() -> PathBuf {
        let root = std::env::temp_dir().join(format!("kimetsu-tools-test-{}", new_id()));
        fs::create_dir_all(&root).expect("create temp root");
        // Isolate from any enclosing git repo (see git_init_boundary).
        kimetsu_core::paths::git_init_boundary(&root);
        root
    }
}
