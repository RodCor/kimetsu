//! Hook-side client: a single timeout-bounded request, plus detached spawn of
//! the daemon when it isn't running. Never blocks the prompt — the caller
//! falls back to floored-FTS on any `Err`/`None`.

use crate::embed_daemon::{ipc, proto};
use std::io::BufReader;
use std::sync::mpsc;
use std::time::Duration;

/// Total wall-clock budget for connect+request before we give up and let the
/// caller fall back to FTS. 300ms total: embed ~10ms + ANN + rerank-of-12
/// ~30–80ms comfortably fits; anything slower falls back to FTS by design.
const REQUEST_BUDGET: Duration = Duration::from_millis(300);

/// Send one request to the daemon for `model`, returning the response or
/// `None` on any failure/timeout. Runs the blocking socket I/O on a worker
/// thread bounded by `REQUEST_BUDGET` (interprocess has no portable connect
/// timeout, so we time-box the whole exchange).
pub fn request(model: &str, req: proto::Request) -> Option<proto::Response> {
    let (tx, rx) = mpsc::channel();
    let model = model.to_string();
    std::thread::spawn(move || {
        let _ = tx.send(exchange(&model, req));
    });
    match rx.recv_timeout(REQUEST_BUDGET) {
        Ok(Ok(resp)) => Some(resp),
        _ => None,
    }
}

fn exchange(model: &str, req: proto::Request) -> std::io::Result<proto::Response> {
    let conn = ipc::connect(model)?;
    let mut w = &conn;
    proto::write_line(&mut w, &req)?;
    let mut reader = BufReader::new(&conn);
    proto::read_line(&mut reader)
}

/// Windows: mark this process's std handles non-inheritable before spawning.
/// Rust's `Command` passes `bInheritHandles=TRUE`, so every inheritable
/// handle in the hook — including the stdout pipe the harness reads — gets
/// duplicated into the daemon child. When that child WINS the bind and lives
/// on as the daemon, it holds the hook's stdout open and the harness waits
/// on pipe EOF until its hook timeout: the first prompt of a session would
/// stall. Clearing HANDLE_FLAG_INHERIT on our own std handles is safe (it
/// doesn't affect our own use of them) and breaks the leak at the source.
#[cfg(windows)]
fn unshare_std_handles() {
    use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: GetStdHandle/SetHandleInformation on our own process's
        // standard handles; null/invalid handles are skipped.
        unsafe {
            let handle = GetStdHandle(which);
            if !handle.is_null() && handle as isize != -1 {
                let _ = SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
}

/// Spawn `kimetsu brain embed-daemon --model <model> --reranker <reranker>`
/// detached. Fire-and-forget: returns immediately; the model load happens
/// inside the child.
pub fn spawn_daemon(model: &str, reranker: &str) -> std::io::Result<()> {
    #[cfg(windows)]
    unshare_std_handles();
    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["brain", "embed-daemon", "--model", model, "--reranker", reranker])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP
        cmd.creation_flags(0x0000_0008 | 0x0000_0200);
    }
    cmd.spawn().map(|_child| ())
}

/// Ensure a daemon is reachable for `model`: a quick ping; if unreachable,
/// spawn one (detached) and return — the CURRENT call still falls back to FTS,
/// but the next prompt will find it warm.
pub fn ensure_daemon(model: &str, reranker: &str) {
    if request(model, proto::Request::Ping).is_some() {
        return;
    }
    let _ = spawn_daemon(model, reranker);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_returns_none_when_no_daemon() {
        let got = request("no-daemon-here-zzz", proto::Request::Ping);
        assert!(got.is_none(), "must be None (-> FTS fallback) when daemon absent");
    }
}
