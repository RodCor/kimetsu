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

/// Candidate pool the reranker judges before truncating to the caller's cap.
/// 6: measured on the eval fixture (full summaries, jina-tiny), pool 6
/// matches pool 12's quality exactly (recall@2/4 0.882/0.896, MRR 0.938,
/// noise 0) at half the rerank latency (~44ms vs ~95ms per query) — the
/// earlier pool-shrink regression was the snippet truncation, not the pool.
/// NOTE: summaries must stay FULL — truncating them cratered recall.
const RERANK_POOL: usize = 6;

/// Sigmoid-score floor — capsules the cross-encoder judges below this are noise.
const RERANK_FLOOR: f32 = 0.30;

/// Process-global state shared by all worker threads.
pub struct DaemonState {
    pub embedder: Box<dyn Embedder + Send + Sync>,
    pub reranker: Option<Box<dyn kimetsu_brain::embeddings::Reranker>>,
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
        // Clone query before it's moved into the request so we can pass it to
        // the reranker after retrieval.
        let query = args.query.clone();
        let cap = args.max_capsules;
        // When reranking, over-fetch a larger candidate pool so the
        // cross-encoder sees enough diversity before truncating to `cap`.
        let fetch_cap = if self.reranker.is_some() { cap.max(RERANK_POOL) } else { cap };
        // Bump the token budget so the pool isn't budget-starved before the
        // reranker sees it.
        let budget = if self.reranker.is_some() {
            (if args.budget_tokens == 0 { 2000 } else { args.budget_tokens }).max(6000)
        } else {
            if args.budget_tokens == 0 { 2000 } else { args.budget_tokens }
        };
        let request = ContextRequest {
            stage: if args.stage.is_empty() { "localization".into() } else { args.stage },
            query: args.query,
            budget_tokens: budget,
            min_score: args.min_score,
            max_capsules: fetch_cap,
            tags: args.tags,
            ..Default::default()
        };
        match session.retrieve_context_with_injected_embedder(request, self.embedder.as_ref()) {
            Ok(mut bundle) => {
                // Apply cross-encoder reranking when a reranker is present.
                if let Some(rr) = &self.reranker {
                    bundle.capsules = kimetsu_brain::context::rerank_capsules(
                        &query,
                        bundle.capsules,
                        rr.as_ref(),
                        RERANK_FLOOR,
                        cap,
                    );
                }
                proto::Response::Capsules {
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
                }
            }
            Err(e) => proto::Response::Error { message: format!("retrieve: {e}") },
        }
    }
}

/// Serve until a `Shutdown` request arrives. Test-only convenience — the
/// production entrypoint binds first (before model load) and calls
/// [`serve_with_listener`] directly.
#[cfg(test)]
pub fn serve(state: Arc<DaemonState>) -> std::io::Result<()> {
    let listener = ipc::listen(&state.model)?;
    serve_with_listener(listener, state)
}

/// Like [`serve`] but with a pre-bound listener. The daemon entrypoint binds
/// BEFORE loading any model so a redundant spawn (live daemon already owns
/// the socket) exits in milliseconds instead of after a multi-second model
/// load — that doomed child otherwise holds inherited stdio handles and
/// stalls the hook's caller for the whole load.
pub fn serve_with_listener(
    listener: interprocess::local_socket::Listener,
    state: Arc<DaemonState>,
) -> std::io::Result<()> {
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
                // Unblock our own accept() so the loop observes the flag and exits.
                let _ = ipc::connect(&state.model);
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
            Err(_) => {
                std::thread::sleep(std::time::Duration::from_millis(50));
                continue;
            }
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
            reranker: None,
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
        server.join().unwrap();
    }

    /// Direct-retrieve unit test for the rerank plumbing.
    ///
    /// Seeds a hermetic temp brain with two memories: one that shares many words
    /// with the query ("rust async tokio"), one that shares none ("python
    /// django"). The StubReranker scores by token overlap, so the rust memory
    /// must win. With `max_capsules: 1` exactly one capsule is returned and it
    /// must be the rust one. No socket is used — we call `DaemonState::retrieve`
    /// directly.
    #[test]
    fn retrieve_with_stub_reranker_reorders_and_caps() {
        use kimetsu_brain::project;
        use kimetsu_core::memory::{MemoryKind, MemoryScope};
        use kimetsu_core::paths::git_init_boundary;

        // ── 1. Set up a hermetic temp brain ──────────────────────────────────
        let root = std::env::temp_dir().join(format!(
            "kimetsu-daemon-rr-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&root).expect("create temp dir");
        git_init_boundary(&root);

        kimetsu_brain::user_brain::with_user_brain_disabled(|| {
            project::init_project(&root, true).expect("init brain");

            // Seed two memories with contrasting overlap against the query.
            project::add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "rust async tokio runtime is the go-to async executor for Rust",
            )
            .expect("add rust memory");
            project::add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "python django web framework for building applications",
            )
            .expect("add python memory");

            // ── 2. Build a DaemonState with NoopEmbedder + StubReranker ──────
            // Use NoopEmbedder so retrieval stays on FTS (no embeddings were
            // stored at ingest time). The reranker still gets applied to the
            // FTS-ranked capsules, which is the plumbing we want to test.
            let state = DaemonState {
                embedder: Box::new(kimetsu_brain::embeddings::NoopEmbedder),
                reranker: Some(Box::new(kimetsu_brain::embeddings::StubReranker)),
                model: unique_model(),
                started: Instant::now(),
                loaded_ms: 0,
                requests: AtomicU64::new(0),
            };

            // ── 3. Issue a Retrieve with max_capsules: 1 ─────────────────────
            let args = proto::RetrieveArgs {
                v: proto::PROTOCOL_VERSION,
                brain_root: root.to_string_lossy().into_owned(),
                query: "rust async tokio".to_string(),
                stage: "localization".to_string(),
                budget_tokens: 4000,
                max_capsules: 1,
                min_score: 0.0,
                tags: vec![],
            };
            let resp = state.retrieve(args);

            // ── 4. Assertions ─────────────────────────────────────────────────
            match resp {
                proto::Response::Capsules { capsules, .. } => {
                    assert_eq!(
                        capsules.len(),
                        1,
                        "max_capsules=1 must return exactly 1 capsule, got {}",
                        capsules.len()
                    );
                    assert!(
                        capsules[0].summary.to_lowercase().contains("rust")
                            || capsules[0].summary.to_lowercase().contains("tokio")
                            || capsules[0].summary.to_lowercase().contains("async"),
                        "the returned capsule must be the higher-overlap (rust/tokio) one, got: {:?}",
                        capsules[0].summary
                    );
                }
                other => panic!("expected Capsules response, got: {other:?}"),
            }
        });
    }
}
