//! Per-test scratch project. Wraps `kimetsu_brain::project::init_project`
//! with deterministic-but-unique paths under the system temp dir, and
//! cleans up on `Drop` so a failing test doesn't leave detritus behind.

use std::fs;
use std::path::{Path, PathBuf};

use kimetsu_core::ids::RunId;

/// A fully-initialized kimetsu project rooted at a per-test temp
/// directory. Use this for any test that needs a real `brain.db` +
/// `project.toml` + `repo_root`.
///
/// Drop removes the directory; if you need to inspect a failed run,
/// either call `.leak()` or extract the path before the panic.
#[derive(Debug)]
pub struct TempProject {
    root: PathBuf,
    cleanup: bool,
}

impl TempProject {
    /// Create a fresh project rooted at `<system_temp>/kimetsu-e2e-<label>-<ulid>`.
    /// The label keeps debug output meaningful when a test fails; the
    /// ULID guarantees per-run uniqueness even when two tests run with
    /// the same label.
    ///
    /// Calls `kimetsu_brain::project::init_project(root, false)`
    /// internally — every TempProject is a real on-disk project with
    /// a real `brain.db`.
    pub fn init(label: &str) -> Self {
        let root = std::env::temp_dir().join(format!("kimetsu-e2e-{label}-{}", RunId::new()));
        fs::create_dir_all(&root).expect("create temp project root");
        kimetsu_brain::project::init_project(&root, false).expect("init_project");
        Self {
            root,
            cleanup: true,
        }
    }

    /// Absolute path to the project root (the dir containing
    /// `project.toml` + `.kimetsu/brain.db`).
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Path to `<root>/.kimetsu/brain.db`. Useful when a test wants to
    /// open the brain directly with `rusqlite::Connection::open` to
    /// seed rows or run custom queries.
    pub fn brain_db(&self) -> PathBuf {
        self.root.join(".kimetsu").join("brain.db")
    }

    /// Open a fresh `rusqlite::Connection` against this project's
    /// brain.db. Each call gets its own connection so tests can run
    /// queries without sharing transaction state with the loop.
    pub fn open_brain(&self) -> rusqlite::Connection {
        let conn = rusqlite::Connection::open(self.brain_db()).expect("open brain.db");
        kimetsu_brain::schema::initialize(&conn).expect("schema init");
        conn
    }

    /// Skip the cleanup-on-drop. Useful when debugging — the caller
    /// keeps the on-disk artifacts so they can `ls` / `sqlite3` the
    /// brain.db after the test fails.
    pub fn leak(mut self) -> PathBuf {
        self.cleanup = false;
        self.root.clone()
    }
}

impl Drop for TempProject {
    fn drop(&mut self) {
        if self.cleanup {
            // Best-effort cleanup — a failing test that panics
            // mid-write may leave a lock the cleanup can't acquire.
            // We swallow the error so Drop doesn't double-panic.
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}
