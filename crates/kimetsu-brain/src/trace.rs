use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use kimetsu_core::KimetsuResult;
use kimetsu_core::event::Event;
use kimetsu_core::ids::RunId;
use kimetsu_core::paths::ProjectPaths;

#[derive(Debug, Clone)]
pub struct RunPaths {
    pub run_dir: PathBuf,
    pub trace_jsonl: PathBuf,
    pub artifacts_dir: PathBuf,
    pub patch_plans_dir: PathBuf,
    pub final_report: PathBuf,
    pub run_log: PathBuf,
}

impl RunPaths {
    pub fn new(paths: &ProjectPaths, run_id: RunId) -> Self {
        let run_dir = paths.runs_dir.join(run_id.to_string());
        Self {
            trace_jsonl: run_dir.join("trace.jsonl"),
            artifacts_dir: run_dir.join("artifacts"),
            patch_plans_dir: run_dir.join("patch_plans"),
            final_report: run_dir.join("final_report.md"),
            run_log: run_dir.join("kimetsu.log"),
            run_dir,
        }
    }

    pub fn create_dirs(&self) -> KimetsuResult<()> {
        fs::create_dir_all(&self.artifacts_dir)?;
        fs::create_dir_all(&self.patch_plans_dir)?;
        Ok(())
    }
}

pub struct TraceWriter {
    file: File,
}

impl TraceWriter {
    pub fn create(paths: &ProjectPaths, run_id: RunId) -> KimetsuResult<(Self, RunPaths)> {
        let run_paths = RunPaths::new(paths, run_id);
        run_paths.create_dirs()?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&run_paths.trace_jsonl)?;
        Ok((Self { file }, run_paths))
    }

    pub fn append(&mut self, event: &Event, fsync: bool) -> KimetsuResult<()> {
        serde_json::to_writer(&mut self.file, event)?;
        self.file.write_all(b"\n")?;
        self.file.flush()?;
        if fsync {
            self.file.sync_data()?;
        }
        Ok(())
    }
}

pub fn read_trace(trace_jsonl: &Path) -> KimetsuResult<Vec<Event>> {
    let file = File::open(trace_jsonl)?;
    let mut reader = BufReader::new(file);
    let mut events = Vec::new();
    let mut line = String::new();
    let mut line_number = 0usize;

    loop {
        line.clear();
        let bytes_read = reader.read_line(&mut line)?;
        if bytes_read == 0 {
            break;
        }
        line_number += 1;

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        match serde_json::from_str::<Event>(trimmed) {
            Ok(event) => events.push(event),
            Err(err) => {
                if !line.ends_with('\n') {
                    eprintln!(
                        "warning: ignoring invalid trailing JSONL line in {}: {err}",
                        trace_jsonl.display()
                    );
                    break;
                }

                return Err(format!(
                    "invalid JSONL at {}:{}: {err}",
                    trace_jsonl.display(),
                    line_number
                )
                .into());
            }
        }
    }

    Ok(events)
}

pub fn discover_traces(paths: &ProjectPaths) -> KimetsuResult<Vec<PathBuf>> {
    if !paths.runs_dir.exists() {
        return Ok(Vec::new());
    }

    let mut traces = Vec::new();
    for entry in fs::read_dir(&paths.runs_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }

        let trace = entry.path().join("trace.jsonl");
        if trace.exists() {
            traces.push(trace);
        }
    }

    traces.sort();
    Ok(traces)
}

pub fn read_all_traces(paths: &ProjectPaths) -> KimetsuResult<Vec<Event>> {
    let mut events = Vec::new();
    for trace in discover_traces(paths)? {
        events.extend(read_trace(&trace)?);
    }

    events.sort_by(|left, right| {
        left.event_id
            .0
            .cmp(&right.event_id.0)
            .then_with(|| left.ts.cmp(&right.ts))
    });
    events.dedup_by_key(|event| event.event_id);
    Ok(events)
}
