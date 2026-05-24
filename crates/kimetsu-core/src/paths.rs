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

/// v0.4.1: return the user-scope kimetsu directory (`~/.kimetsu/`).
///
/// Resolution order:
///   1. `$KIMETSU_USER_BRAIN_DIR` if set and non-empty. Used by tests
///      to point the user brain at a temp dir without touching the
///      real `$HOME`, and by power users who want the brain to live
///      somewhere other than home (encrypted volume, network share,
///      etc.).
///   2. `$HOME` on Unix / `$USERPROFILE` on Windows, joined with
///      `.kimetsu`.
///
/// Returns `None` only when neither env var is set — in practice we
/// always have a home dir, so this almost never returns None outside
/// of stripped CI environments.
pub fn user_kimetsu_dir() -> Option<PathBuf> {
    if let Ok(override_dir) = std::env::var("KIMETSU_USER_BRAIN_DIR") {
        let trimmed = override_dir.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    let home = if cfg!(windows) {
        std::env::var("USERPROFILE").ok()
    } else {
        std::env::var("HOME").ok()
    };
    home.filter(|h| !h.trim().is_empty())
        .map(|h| PathBuf::from(h).join(".kimetsu"))
}

/// v0.4.1: full path to the user-scope brain.db.
///
/// Convenience wrapper over [`user_kimetsu_dir`] that appends
/// `brain.db`. Returns None when no home directory is resolvable.
pub fn user_brain_db_path() -> Option<PathBuf> {
    user_kimetsu_dir().map(|dir| dir.join("brain.db"))
}

/// v0.4.1: returns true when the user brain is enabled.
///
/// `KIMETSU_USER_BRAIN=0` / `false` / `off` / `no` disables it
/// (case-insensitive). Anything else, including unset, leaves it
/// enabled by default — the "brain follows you between projects"
/// pitch only works if the user opts OUT, not opts IN.
pub fn user_brain_enabled() -> bool {
    let value = match std::env::var("KIMETSU_USER_BRAIN") {
        Ok(v) => v,
        Err(_) => return true,
    };
    let v = value.trim().to_ascii_lowercase();
    !matches!(v.as_str(), "0" | "false" | "off" | "no")
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
