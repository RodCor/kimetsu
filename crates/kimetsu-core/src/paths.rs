use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::KimetsuResult;

#[derive(Debug, Clone)]
pub struct ProjectPaths {
    pub repo_root: PathBuf,
    pub kimetsu_dir: PathBuf,
    pub project_toml: PathBuf,
    pub brain_db: PathBuf,
    pub project_log: PathBuf,
    pub runs_dir: PathBuf,
    pub lock_file: PathBuf,
}

impl ProjectPaths {
    pub fn discover(start: impl AsRef<Path>) -> KimetsuResult<Self> {
        let start = start.as_ref();
        let repo_root = discover_repo_root(start)?;
        let kimetsu_dir = repo_root.join(".kimetsu");

        Ok(Self {
            repo_root,
            project_toml: kimetsu_dir.join("project.toml"),
            brain_db: kimetsu_dir.join("brain.db"),
            project_log: kimetsu_dir.join("kimetsu.log"),
            runs_dir: kimetsu_dir.join("runs"),
            lock_file: kimetsu_dir.join("project.lock"),
            kimetsu_dir,
        })
    }
}

pub fn discover_repo_root(start: &Path) -> KimetsuResult<PathBuf> {
    if let Some(root) = git_root(start) {
        return Ok(root);
    }

    let start = start.canonicalize()?;
    if start.is_file() {
        Ok(start
            .parent()
            .ok_or("file path has no parent")?
            .to_path_buf())
    } else {
        Ok(start)
    }
}

fn git_root(start: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(start)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8(output.stdout).ok()?;
    let root = stdout.trim();
    if root.is_empty() {
        return None;
    }

    PathBuf::from(root).canonicalize().ok()
}

pub fn default_project_id(repo_root: &Path) -> String {
    repo_root
        .file_name()
        .and_then(OsStr::to_str)
        .map(slug)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "kimetsu-project".to_string())
}

fn slug(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}
