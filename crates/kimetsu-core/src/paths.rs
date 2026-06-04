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
        let repo_root = discover_repo_root(start.as_ref())?;
        Ok(Self::at_root(repo_root))
    }

    /// Build the paths anchored at an explicit `repo_root`, WITHOUT
    /// climbing to an enclosing git repository. Use this when a command
    /// is told exactly which directory to operate on (e.g. the install
    /// wizard's `--workspace`), so it never writes into a parent repo.
    pub fn at_root(repo_root: impl Into<PathBuf>) -> Self {
        let repo_root = repo_root.into();
        let kimetsu_dir = repo_root.join(".kimetsu");
        Self {
            repo_root,
            project_toml: kimetsu_dir.join("project.toml"),
            brain_db: kimetsu_dir.join("brain.db"),
            project_log: kimetsu_dir.join("kimetsu.log"),
            runs_dir: kimetsu_dir.join("runs"),
            lock_file: kimetsu_dir.join("project.lock"),
            kimetsu_dir,
        }
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

/// v0.8: make `dir` a standalone git repository (best-effort) so
/// [`discover_repo_root`] resolves to `dir` itself instead of climbing
/// to an enclosing repo. Two callers:
///   * the benchmark harness, for throwaway fixture repos — without
///     this, a fixture created under the system temp dir on a machine
///     whose `$HOME` (or any ancestor) is a git repo would init its
///     brain at that ancestor and leak fixture memories into it;
///   * tests that create isolated project roots under the temp dir.
///
/// Creates `dir` if needed. Returns true when git reported success; a
/// failure (e.g. git not installed) just means the caller doesn't get
/// isolation, which is the prior behaviour.
pub fn git_init_boundary(dir: &Path) -> bool {
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(dir)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
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
    // Delegate to the config-aware variant with the default (true).
    user_brain_enabled_with(true)
}

/// W3.3: config-aware user-brain gate. Resolution precedence:
///   1. `KIMETSU_USER_BRAIN` env is explicitly set → its value wins.
///   2. Env is unset → `config_use_user_brain` governs.
///
/// Callers with a `ProjectConfig` should pass
/// `config.kimetsu.use_user_brain`; back-compat callers can use
/// `user_brain_enabled()`.
pub fn user_brain_enabled_with(config_use_user_brain: bool) -> bool {
    // Precedence: env override > config > default.
    match std::env::var("KIMETSU_USER_BRAIN") {
        Ok(value) => {
            let v = value.trim().to_ascii_lowercase();
            // Env is set — respect it (disable values → false, anything
            // else including empty → treat as "on").
            !matches!(v.as_str(), "0" | "false" | "off" | "no")
        }
        // Env unset → config governs.
        Err(_) => config_use_user_brain,
    }
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

/// W2: per-project cache root for transient, non-brain artifacts
/// (proactive hook state, chat REPL output, benchmark output).
///
/// Kept OUT of the project's `.kimetsu/` so a brain-only install's
/// `.kimetsu/` stays lean. Lives under the user kimetsu home
/// (`~/.kimetsu/cache/<project-id>/`), honouring
/// `KIMETSU_USER_BRAIN_DIR`. Falls back to the OS temp dir when no
/// home resolves, so it NEVER lands back inside `.kimetsu/`.
///
/// The `<project-id>` component is the same slug produced by
/// [`default_project_id`], which is filesystem-safe (ASCII
/// alphanumeric + hyphens only, non-empty).
pub fn user_cache_dir_for(repo_root: &Path) -> PathBuf {
    let hash = default_project_id(repo_root);
    match user_kimetsu_dir() {
        Some(home) => home.join("cache").join(&hash),
        None => std::env::temp_dir().join("kimetsu-cache").join(&hash),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::Mutex;

    /// Process-wide mutex for env-mutating path tests.  Any test that
    /// temporarily modifies `KIMETSU_USER_BRAIN_DIR`, `HOME`, or
    /// `USERPROFILE` must hold this guard for the duration.
    fn env_lock() -> &'static Mutex<()> {
        static LOCK: Mutex<()> = Mutex::new(());
        &LOCK
    }

    /// Run `f` with `KIMETSU_USER_BRAIN_DIR` set to `dir`, restoring the
    /// previous value under the shared env lock.
    fn with_brain_dir<R>(dir: &Path, f: impl FnOnce() -> R) -> R {
        let _guard = env_lock().lock().unwrap_or_else(|p| p.into_inner());
        let prev = std::env::var("KIMETSU_USER_BRAIN_DIR").ok();
        unsafe {
            std::env::set_var("KIMETSU_USER_BRAIN_DIR", dir);
        }
        let out = f();
        unsafe {
            match prev {
                Some(v) => std::env::set_var("KIMETSU_USER_BRAIN_DIR", v),
                None => std::env::remove_var("KIMETSU_USER_BRAIN_DIR"),
            }
        }
        out
    }

    /// Run `f` with both `KIMETSU_USER_BRAIN_DIR` and the platform home
    /// env var cleared, so `user_kimetsu_dir()` returns `None`.
    fn without_brain_dir<R>(f: impl FnOnce() -> R) -> R {
        let _guard = env_lock().lock().unwrap_or_else(|p| p.into_inner());
        let prev_override = std::env::var("KIMETSU_USER_BRAIN_DIR").ok();
        let home_key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let prev_home = std::env::var(home_key).ok();
        unsafe {
            std::env::remove_var("KIMETSU_USER_BRAIN_DIR");
            std::env::remove_var(home_key);
        }
        let out = f();
        unsafe {
            match prev_override {
                Some(v) => std::env::set_var("KIMETSU_USER_BRAIN_DIR", v),
                None => std::env::remove_var("KIMETSU_USER_BRAIN_DIR"),
            }
            match prev_home {
                Some(v) => std::env::set_var(home_key, v),
                None => std::env::remove_var(home_key),
            }
        }
        out
    }

    #[test]
    fn user_cache_dir_for_lands_under_user_home() {
        let tmp = std::env::temp_dir().join("kimetsu-test-cache-home");
        let repo = Path::new("/some/project/my-repo");
        let result = with_brain_dir(&tmp, || user_cache_dir_for(repo));
        // Must be under <tmp>/cache/<slug>/
        assert!(
            result.starts_with(tmp.join("cache")),
            "expected result under <tmp>/cache, got {result:?}"
        );
        // Must not be inside .kimetsu of the repo.
        assert!(
            !result.starts_with(repo.join(".kimetsu")),
            "must not be inside repo .kimetsu, got {result:?}"
        );
        // The leaf component is the slug of the repo name.
        let leaf = result.file_name().unwrap().to_str().unwrap();
        assert_eq!(leaf, "my-repo");
    }

    #[test]
    fn user_cache_dir_for_falls_back_to_temp_when_no_home() {
        let repo = Path::new("/some/project/fallback-repo");
        let result = without_brain_dir(|| user_cache_dir_for(repo));
        // Must be under the OS temp dir, not under ~/.kimetsu.
        let tmp = std::env::temp_dir();
        assert!(
            result.starts_with(&tmp),
            "expected result under OS temp dir, got {result:?}"
        );
        // Must contain "kimetsu-cache".
        assert!(
            result
                .components()
                .any(|c| c.as_os_str() == "kimetsu-cache"),
            "expected 'kimetsu-cache' in path, got {result:?}"
        );
    }

    #[test]
    fn slug_is_filesystem_safe() {
        // No env mutation — no lock needed.
        let id = default_project_id(Path::new("/tmp/my repo with spaces & stuff!"));
        assert!(
            id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
            "slug contains unsafe chars: {id:?}"
        );
        assert!(!id.is_empty());
    }
}
