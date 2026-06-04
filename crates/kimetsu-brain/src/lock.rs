use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use kimetsu_core::KimetsuResult;
use kimetsu_core::ids::RunId;
use kimetsu_core::paths::ProjectPaths;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// How long `acquire` blocks waiting for a held lock before giving up.
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(15);

/// How long to sleep between poll attempts.
const POLL_INTERVAL: Duration = Duration::from_millis(150);

/// A short write op whose lock is older than this is certainly from a dead
/// holder (quick writes complete in milliseconds).
const STALE_SHORT_OP_AGE: Duration = Duration::from_secs(120);

#[derive(Debug)]
pub struct ProjectLock {
    path: PathBuf,
    active: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct LockPayload {
    pid: u32,
    command: String,
    run_id: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    started_at: OffsetDateTime,
}

impl ProjectLock {
    /// Acquire the writer lock, blocking until it is free or the default
    /// timeout elapses.  Stale locks (dead PID, corrupt payload) are
    /// reclaimed automatically.
    pub fn acquire(
        paths: &ProjectPaths,
        command: impl Into<String>,
        run_id: Option<RunId>,
    ) -> KimetsuResult<Self> {
        acquire_with_timeout(paths, command, run_id, ACQUIRE_TIMEOUT)
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

/// Inner implementation that accepts an explicit timeout so tests can pass a
/// short one without waiting 15 s.
pub(crate) fn acquire_with_timeout(
    paths: &ProjectPaths,
    command: impl Into<String>,
    run_id: Option<RunId>,
    timeout: Duration,
) -> KimetsuResult<ProjectLock> {
    fs::create_dir_all(&paths.kimetsu_dir)?;
    let command: String = command.into();
    let payload = LockPayload {
        pid: std::process::id(),
        command: command.clone(),
        run_id: run_id.map(|id| id.to_string()),
        started_at: OffsetDateTime::now_utc(),
    };
    let serialized = serde_json::to_string_pretty(&payload)?;
    let deadline = Instant::now() + timeout;

    loop {
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&paths.lock_file)
        {
            Ok(mut file) => {
                file.write_all(serialized.as_bytes())?;
                file.sync_all()?;
                return Ok(ProjectLock {
                    path: paths.lock_file.clone(),
                    active: true,
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // Check if the existing lock is stale (dead holder or corrupt).
                if lock_is_stale(&paths.lock_file) {
                    // Best-effort removal; if another racer beats us here the
                    // next loop iteration will retry create_new.
                    let _ = fs::remove_file(&paths.lock_file);
                    continue;
                }

                if Instant::now() >= deadline {
                    let existing = fs::read_to_string(&paths.lock_file).unwrap_or_default();
                    return Err(format!(
                        "project writer lock held (timed out after {}s); \
                         if the holder crashed, run `kimetsu lock clear`.\n{existing}",
                        timeout.as_secs()
                    )
                    .into());
                }

                std::thread::sleep(POLL_INTERVAL);
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// Returns `true` when the lock at `lock_file` should be reclaimed.
///
/// Staleness criteria (conservative — when in doubt, return `false`):
/// * Payload cannot be parsed (corrupt / empty) → stale.
/// * Holder PID is no longer alive → stale.
/// * PID liveness is indeterminate AND the lock is implausibly old for a
///   short write command → stale (age safety-net).
fn lock_is_stale(lock_file: &Path) -> bool {
    let content = match fs::read_to_string(lock_file) {
        Ok(s) => s,
        Err(_) => return true, // unreadable → treat as stale
    };

    let payload: LockPayload = match serde_json::from_str(&content) {
        Ok(p) => p,
        Err(_) => return true, // corrupt → treat as stale
    };

    match process_alive(payload.pid) {
        ProcessLiveness::Dead => true,
        ProcessLiveness::Alive => false,
        ProcessLiveness::Indeterminate => {
            // Fall back to the age safety-net for short-lived commands.
            is_short_op_too_old(&payload)
        }
    }
}

/// Returns `true` when `payload` looks like a quick write operation whose
/// `started_at` timestamp is implausibly far in the past.
fn is_short_op_too_old(payload: &LockPayload) -> bool {
    let age_secs = (OffsetDateTime::now_utc() - payload.started_at).whole_seconds();
    if age_secs < 0 {
        return false; // clock skew — be conservative
    }
    let age = Duration::from_secs(age_secs as u64);
    if age < STALE_SHORT_OP_AGE {
        return false;
    }
    // Heuristic: agent-run commands (long-lived) contain "run" or "record" or
    // "ingest".  Everything else (memory add/propose/edit/undo/invalidate,
    // brain config, etc.) is a quick write.
    let cmd = payload.command.to_ascii_lowercase();
    let is_long_op = cmd.contains("run") || cmd.contains("record") || cmd.contains("ingest");
    !is_long_op
}

#[derive(Debug, PartialEq, Eq)]
enum ProcessLiveness {
    Alive,
    Dead,
    Indeterminate,
}

/// Check whether a process with `pid` is still alive.
///
/// Conservative: when we cannot determine liveness, return `Indeterminate`
/// rather than `Dead` so we don't accidentally reclaim a live lock.
fn process_alive(pid: u32) -> ProcessLiveness {
    #[cfg(unix)]
    {
        process_alive_unix(pid)
    }
    #[cfg(windows)]
    {
        process_alive_windows(pid)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        ProcessLiveness::Indeterminate
    }
}

#[cfg(unix)]
fn process_alive_unix(pid: u32) -> ProcessLiveness {
    // SAFETY: kill(pid, 0) is a standard POSIX probe: it performs permission
    // checks without sending a signal.  A return value of 0 means the process
    // exists and we have permission; ESRCH means no such process (dead).
    // Any other errno (e.g. EPERM) means the process exists but we lack
    // permission — treat as Alive.
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    unsafe {
        let rc = kill(pid as i32, 0);
        if rc == 0 {
            return ProcessLiveness::Alive;
        }
        // Check errno.
        let errno = *libc_errno();
        if errno == 3 {
            // ESRCH = 3 on Linux/macOS
            ProcessLiveness::Dead
        } else {
            ProcessLiveness::Alive // EPERM or other → process exists
        }
    }
}

/// Portable errno accessor for unix (avoids the `libc` crate).
#[cfg(unix)]
unsafe fn libc_errno() -> *mut i32 {
    // On Linux glibc the TLS errno is accessed via __errno_location().
    // On macOS it's __error().  Both are in the C standard library.
    #[cfg(target_os = "macos")]
    unsafe extern "C" {
        fn __error() -> *mut i32;
    }
    #[cfg(target_os = "macos")]
    return unsafe { __error() };

    #[cfg(not(target_os = "macos"))]
    unsafe extern "C" {
        fn __errno_location() -> *mut i32;
    }
    #[cfg(not(target_os = "macos"))]
    return unsafe { __errno_location() };
}

#[cfg(windows)]
fn process_alive_windows(pid: u32) -> ProcessLiveness {
    // SAFETY: We use Win32 to probe PID liveness.
    //
    // Strategy:
    //   1. OpenProcess(SYNCHRONIZE, ...) — if NULL: check last-error.
    //      ERROR_INVALID_PARAMETER (87) → PID doesn't exist → Dead.
    //      ERROR_ACCESS_DENIED (5)      → process exists but no access → Alive.
    //      Anything else                → Indeterminate (conservative).
    //   2. Got a handle: call WaitForSingleObject(handle, 0).
    //      WAIT_OBJECT_0 (0) → process is signaled/exited → Dead.
    //      WAIT_TIMEOUT (258) or other → process is running → Alive.
    //   This correctly handles zombie processes (handle obtained but process
    //   already exited — WFSO immediately returns WAIT_OBJECT_0).
    unsafe extern "system" {
        fn OpenProcess(desired_access: u32, inherit_handle: i32, pid: u32) -> isize;
        fn CloseHandle(handle: isize) -> i32;
        fn GetLastError() -> u32;
        fn WaitForSingleObject(handle: isize, milliseconds: u32) -> u32;
    }

    const SYNCHRONIZE: u32 = 0x0010_0000;
    const ERROR_INVALID_PARAMETER: u32 = 87;
    const ERROR_ACCESS_DENIED: u32 = 5;
    const WAIT_OBJECT_0: u32 = 0;
    const WAIT_TIMEOUT: u32 = 258;

    unsafe {
        let handle = OpenProcess(SYNCHRONIZE, 0, pid);
        if handle == 0 {
            let err = GetLastError();
            return match err {
                ERROR_INVALID_PARAMETER => ProcessLiveness::Dead,
                ERROR_ACCESS_DENIED => ProcessLiveness::Alive,
                _ => ProcessLiveness::Indeterminate,
            };
        }
        // We have a handle — use WaitForSingleObject with 0 timeout to
        // distinguish a zombie (exited but handle not yet closed) from a live
        // process.  A signaled process object means it has exited.
        let wait_result = WaitForSingleObject(handle, 0);
        CloseHandle(handle);
        match wait_result {
            WAIT_OBJECT_0 => ProcessLiveness::Dead,
            WAIT_TIMEOUT => ProcessLiveness::Alive,
            _ => ProcessLiveness::Indeterminate,
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use kimetsu_core::paths::ProjectPaths;
    use std::sync::{Arc, Barrier};
    use std::time::Instant;

    /// RAII temp directory that removes itself on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static CTR: AtomicU64 = AtomicU64::new(0);
            let n = CTR.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let dir = std::env::temp_dir().join(format!("kimetsu-lock-test-{pid}-{n}"));
            fs::create_dir_all(&dir).expect("create temp dir");
            TempDir(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Build a fresh `ProjectPaths` rooted at `dir`.
    fn make_paths(dir: &TempDir) -> ProjectPaths {
        ProjectPaths::at_root(dir.path())
    }

    // -----------------------------------------------------------------------
    // T1 — Concurrent acquire serializes (no failure)
    // -----------------------------------------------------------------------
    #[test]
    fn concurrent_acquire_serializes() {
        let dir = TempDir::new();
        let paths = make_paths(&dir);
        fs::create_dir_all(&paths.kimetsu_dir).unwrap();

        let paths = Arc::new(paths);
        // Barrier so both threads enter the acquire window at roughly the same time.
        let barrier = Arc::new(Barrier::new(2));
        let errors = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));

        let mut handles = Vec::new();
        for i in 0..2 {
            let p = Arc::clone(&paths);
            let b = Arc::clone(&barrier);
            let errs = Arc::clone(&errors);
            let h = std::thread::spawn(move || {
                b.wait(); // race to the acquire
                match acquire_with_timeout(
                    &p,
                    format!("test-thread-{i}"),
                    None,
                    Duration::from_secs(10),
                ) {
                    Ok(lock) => {
                        std::thread::sleep(Duration::from_millis(30));
                        lock.release().unwrap();
                    }
                    Err(e) => {
                        errs.lock().unwrap().push(e.to_string());
                    }
                }
            });
            handles.push(h);
        }

        for h in handles {
            h.join().unwrap();
        }

        let errs = errors.lock().unwrap();
        assert!(
            errs.is_empty(),
            "expected both threads to succeed; errors: {errs:?}"
        );
    }

    // -----------------------------------------------------------------------
    // T2 — Stale lock with dead PID is reclaimed automatically
    // -----------------------------------------------------------------------
    #[test]
    fn stale_lock_dead_pid_is_reclaimed() {
        let dir = TempDir::new();
        let paths = make_paths(&dir);
        fs::create_dir_all(&paths.kimetsu_dir).unwrap();

        // Spawn a trivial child process, wait for it to fully exit, capture its PID.
        let mut child = std::process::Command::new(if cfg!(windows) { "cmd" } else { "true" })
            .args(if cfg!(windows) {
                &["/c", "exit", "0"][..]
            } else {
                &[][..]
            })
            .spawn()
            .expect("spawn child");
        let dead_pid = child.id();
        child.wait().expect("wait for child to exit");
        // Give the OS a moment to fully reap the process object.
        std::thread::sleep(Duration::from_millis(200));

        // Write a stale lock file with the dead PID.
        let stale_payload = serde_json::json!({
            "pid": dead_pid,
            "command": "memory add",
            "run_id": null,
            "started_at": "2000-01-01T00:00:00Z"
        });
        fs::write(&paths.lock_file, stale_payload.to_string()).unwrap();

        // acquire should reclaim the stale lock and succeed.
        let lock = acquire_with_timeout(&paths, "test", None, Duration::from_secs(5))
            .expect("should reclaim stale lock and succeed");
        lock.release().unwrap();
    }

    // -----------------------------------------------------------------------
    // T3 — Corrupt lock file is treated as stale and reclaimed
    // -----------------------------------------------------------------------
    #[test]
    fn corrupt_lock_is_reclaimed() {
        let dir = TempDir::new();
        let paths = make_paths(&dir);
        fs::create_dir_all(&paths.kimetsu_dir).unwrap();

        // Write garbage into the lock file.
        fs::write(&paths.lock_file, b"not json at all!!!\x00\x01\x02").unwrap();

        let lock = acquire_with_timeout(&paths, "test", None, Duration::from_secs(5))
            .expect("should reclaim corrupt lock and succeed");
        lock.release().unwrap();
    }

    // -----------------------------------------------------------------------
    // T4 — Live-held lock times out and returns Err (bounded wait)
    // -----------------------------------------------------------------------
    #[test]
    fn live_held_lock_times_out() {
        let dir = TempDir::new();
        let paths = make_paths(&dir);
        fs::create_dir_all(&paths.kimetsu_dir).unwrap();

        let paths = Arc::new(paths);

        // Hold the lock from a background thread and never release it during the
        // timeout window.
        let barrier = Arc::new(Barrier::new(2));
        let paths2 = Arc::clone(&paths);
        let b2 = Arc::clone(&barrier);
        let holder = std::thread::spawn(move || {
            let lock = acquire_with_timeout(&paths2, "holder", None, Duration::from_secs(5))
                .expect("holder should acquire");
            b2.wait(); // signal: lock is held
            // Hold for long enough that the waiter definitely times out.
            std::thread::sleep(Duration::from_secs(3));
            lock.release().unwrap();
        });

        barrier.wait(); // wait until the holder has the lock

        let short_timeout = Duration::from_millis(350);
        let t0 = Instant::now();
        let result = acquire_with_timeout(&paths, "waiter", None, short_timeout);
        let elapsed = t0.elapsed();

        // Must have returned an error (timed out).
        assert!(result.is_err(), "expected Err, got Ok");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("timed out"),
            "error message should mention 'timed out', got: {msg}"
        );

        // Must NOT have failed instantly — the waiter should have polled for
        // close to the timeout duration.
        assert!(
            elapsed >= short_timeout.saturating_sub(Duration::from_millis(50)),
            "waiter returned too quickly (elapsed {elapsed:?}, expected ~{short_timeout:?})"
        );

        holder.join().unwrap();
    }

    // -----------------------------------------------------------------------
    // T5 — process_alive: current pid is Alive; dead child pid is Dead
    // -----------------------------------------------------------------------
    #[test]
    fn process_alive_current_is_alive() {
        let my_pid = std::process::id();
        assert_eq!(
            process_alive(my_pid),
            ProcessLiveness::Alive,
            "current process should be Alive"
        );
    }

    #[test]
    fn process_alive_dead_pid_is_dead() {
        // Spawn a child that exits immediately, capture its PID, wait for it.
        let mut child = std::process::Command::new(if cfg!(windows) { "cmd" } else { "true" })
            .args(if cfg!(windows) {
                &["/c", "exit", "0"][..]
            } else {
                &[][..]
            })
            .spawn()
            .expect("spawn child");
        let pid = child.id();
        child.wait().expect("wait for child");

        // Give the OS a moment to fully reap.
        std::thread::sleep(Duration::from_millis(100));

        let liveness = process_alive(pid);
        // On Windows PIDs can be recycled quickly, so we allow Indeterminate
        // as a safe fallback; Dead is the expected answer.
        assert!(
            matches!(
                liveness,
                ProcessLiveness::Dead | ProcessLiveness::Indeterminate
            ),
            "dead child PID should be Dead or Indeterminate, got {liveness:?}"
        );
    }
}
