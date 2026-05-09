use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use kimetsu_core::KimetsuResult;
use kimetsu_core::ids::RunId;
use kimetsu_core::paths::ProjectPaths;
use serde::Serialize;
use time::OffsetDateTime;

pub struct ProjectLock {
    path: PathBuf,
    active: bool,
}

#[derive(Debug, Serialize)]
struct LockPayload {
    pid: u32,
    command: String,
    run_id: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    started_at: OffsetDateTime,
}

impl ProjectLock {
    pub fn acquire(
        paths: &ProjectPaths,
        command: impl Into<String>,
        run_id: Option<RunId>,
    ) -> KimetsuResult<Self> {
        fs::create_dir_all(&paths.kimetsu_dir)?;
        let payload = LockPayload {
            pid: std::process::id(),
            command: command.into(),
            run_id: run_id.map(|id| id.to_string()),
            started_at: OffsetDateTime::now_utc(),
        };
        let payload = serde_json::to_string_pretty(&payload)?;

        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&paths.lock_file)
        {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                let existing =
                    fs::read_to_string(&paths.lock_file).unwrap_or_else(|_| "<unreadable>".into());
                return Err(format!(
                    "project writer lock is already held at {}\n{}",
                    paths.lock_file.display(),
                    existing
                )
                .into());
            }
            Err(err) => return Err(err.into()),
        };

        file.write_all(payload.as_bytes())?;
        file.sync_all()?;

        Ok(Self {
            path: paths.lock_file.clone(),
            active: true,
        })
    }

    pub fn release(mut self) -> KimetsuResult<()> {
        self.active = false;
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.into()),
        }
    }
}

impl Drop for ProjectLock {
    fn drop(&mut self) {
        if self.active {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub fn clear_force(paths: &ProjectPaths) -> KimetsuResult<bool> {
    match fs::remove_file(&paths.lock_file) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err.into()),
    }
}
