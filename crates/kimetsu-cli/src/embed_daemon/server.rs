//! The daemon event loop: eager model load, a bounded worker pool, and
//! per-request read-only retrieval with the one shared embedder.

use crate::embed_daemon::{ipc, proto};
use interprocess::local_socket::prelude::*;
use kimetsu_brain::context::ContextRequest;
use kimetsu_brain::embeddings::Embedder;
use kimetsu_brain::project::BrainSession;
use std::io::BufReader;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

/// Process-global state shared by all worker threads.
pub struct DaemonState {
    pub embedder: Box<dyn Embedder + Send + Sync>,
    pub model: String,
    pub started: Instant,
    pub loaded_ms: u64,
    pub requests: AtomicU64,
}

impl DaemonState {
    fn handle(&self, req: proto::Request) -> proto::Response {
        match req {
            proto::Request::Ping | proto::Request::Warm => proto::Response::Info {
                version: env!("CARGO_PKG_VERSION").to_string(),
                model: self.model.clone(),
                uptime_s: self.started.elapsed().as_secs(),
                requests: self.requests.load(Ordering::Relaxed),
                loaded_ms: self.loaded_ms,
            },
            proto::Request::Shutdown => proto::Response::Ok,
            proto::Request::Retrieve(args) => {
                self.requests.fetch_add(1, Ordering::Relaxed);
                self.retrieve(args)
            }
        }
    }

    fn retrieve(&self, args: proto::RetrieveArgs) -> proto::Response {
        let session = match BrainSession::open_readonly(std::path::Path::new(&args.brain_root)) {
            Ok(s) => s,
            Err(e) => return proto::Response::Error { message: format!("open: {e}") },
        };
        let request = ContextRequest {
            stage: if args.stage.is_empty() { "localization".into() } else { args.stage },
            query: args.query,
            budget_tokens: if args.budget_tokens == 0 { 2000 } else { args.budget_tokens },
            min_score: args.min_score,
            max_capsules: args.max_capsules,
            tags: args.tags,
            ..Default::default()
        };
        match session.retrieve_context_with_injected_embedder(request, self.embedder.as_ref()) {
            Ok(bundle) => proto::Response::Capsules {
                capsules: bundle
                    .capsules
                    .iter()
                    .map(|c| proto::Capsule {
                        summary: c.summary.clone(),
                        kind: c.kind.clone(),
                        score: c.score,
                    })
                    .collect(),
                skipped: bundle.skipped,
                top_score: bundle.top_score,
            },
            Err(e) => proto::Response::Error { message: format!("retrieve: {e}") },
        }
    }
}

/// Serve until a `Shutdown` request arrives. `state.embedder` is whatever the
/// caller injected (the real model in production, a stub in tests).
pub fn serve(state: Arc<DaemonState>) -> std::io::Result<()> {
    let listener = ipc::listen(&state.model)?;
    let workers = std::thread::available_parallelism()
        .map(|n| (usize::from(n) * 2).min(8))
        .unwrap_or(4);
    let (tx, rx) = std::sync::mpsc::sync_channel::<interprocess::local_socket::Stream>(workers * 2);
    let rx = Arc::new(std::sync::Mutex::new(rx));
    let shutdown = Arc::new(AtomicBool::new(false));

    let mut handles = Vec::new();
    for _ in 0..workers {
        let rx = rx.clone();
        let state = state.clone();
        let shutdown = shutdown.clone();
        handles.push(std::thread::spawn(move || loop {
            let conn = {
                let guard = rx.lock().unwrap_or_else(|p| p.into_inner());
                guard.recv()
            };
            let Ok(conn) = conn else { break };
            if handle_connection(&state, conn) {
                shutdown.store(true, Ordering::Relaxed);
                break;
            }
        }));
    }

    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        match listener.accept() {
            Ok(conn) => {
                if tx.send(conn).is_err() {
                    break;
                }
            }
            Err(_) => continue,
        }
    }
    Ok(())
}

/// Handle one connection: read a request, write the response. Returns true if
/// the request was `Shutdown`.
fn handle_connection(state: &DaemonState, conn: interprocess::local_socket::Stream) -> bool {
    let mut reader = BufReader::new(&conn);
    let req: proto::Request = match proto::read_line(&mut reader) {
        Ok(r) => r,
        Err(_) => return false,
    };
    let is_shutdown = matches!(req, proto::Request::Shutdown);
    let resp = state.handle(req);
    let mut w = &conn;
    let _ = proto::write_line(&mut w, &resp);
    is_shutdown
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed_daemon::ipc;

    fn unique_model() -> String {
        format!(
            "stub-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        )
    }

    #[test]
    fn serve_answers_ping_then_shuts_down() {
        let model = unique_model();
        let state = Arc::new(DaemonState {
            embedder: Box::new(kimetsu_brain::embeddings::StubEmbedder::default()),
            model: model.clone(),
            started: Instant::now(),
            loaded_ms: 0,
            requests: AtomicU64::new(0),
        });
        let server = {
            let state = state.clone();
            std::thread::spawn(move || serve(state).unwrap())
        };
        std::thread::sleep(std::time::Duration::from_millis(100));

        {
            let conn = ipc::connect(&model).expect("connect");
            let mut w = &conn;
            proto::write_line(&mut w, &proto::Request::Ping).unwrap();
            let mut r = BufReader::new(&conn);
            let resp: proto::Response = proto::read_line(&mut r).unwrap();
            assert!(matches!(resp, proto::Response::Info { .. }));
        }
        {
            let conn = ipc::connect(&model).expect("connect");
            let mut w = &conn;
            proto::write_line(&mut w, &proto::Request::Shutdown).unwrap();
            let mut r = BufReader::new(&conn);
            let _resp: proto::Response = proto::read_line(&mut r).unwrap();
        }
        // Nudge the accept loop so it observes the shutdown flag and returns.
        let _ = ipc::connect(&model);
        server.join().unwrap();
    }
}
