//! Server-hosted Kimetsu brain over HTTP MCP. Library half (so integration
//! tests can drive the router); the `kimetsu-remote` binary is a thin wrapper.

pub mod app;
pub mod auth;
pub mod catalog;
pub mod config;
pub mod repo;
pub mod rpc;
pub mod state;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use crate::auth::AuthConfig;
use crate::state::AppState;

/// Parse → isolate → bind → serve. Blocks until shutdown.
pub fn run_serve(args: config::ServeArgs) -> Result<(), String> {
    init_tracing(args.log.as_deref());

    tracing::warn!(
        "Kimetsu Remote is BETA — under active testing; expect rough edges and possible \
         breaking changes before the stable release. Please report issues."
    );

    // Server isolation: every brain lives at an explicit root (never climb to an
    // enclosing repo), and the cross-project user brain is off (each repo brain
    // is standalone).
    kimetsu_core::paths::pin_discover_to_root();
    // SAFETY: set before the tokio runtime starts any worker threads.
    unsafe {
        std::env::set_var("KIMETSU_USER_BRAIN", "0");
    }

    let auth = config::build_auth(&args)?;
    let data_dir = prepare_data_dir(&args.data)?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .max_blocking_threads(args.max_blocking_threads.max(1))
        .build()
        .map_err(|e| format!("build runtime: {e}"))?;

    runtime.block_on(serve(args.addr, data_dir, auth))
}

fn prepare_data_dir(p: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(p).map_err(|e| format!("create data dir {}: {e}", p.display()))?;
    let canon = p
        .canonicalize()
        .map_err(|e| format!("canonicalize data dir {}: {e}", p.display()))?;
    if inside_git_repo(&canon) {
        tracing::warn!(
            path = %kimetsu_core::paths::display_path(&canon),
            "data dir is inside a git repository — prefer a dir outside any repo so brains aren't picked up by git tooling"
        );
    }
    Ok(canon)
}

fn inside_git_repo(start: &Path) -> bool {
    let mut cur = Some(start);
    while let Some(dir) = cur {
        if dir.join(".git").exists() {
            return true;
        }
        cur = dir.parent();
    }
    false
}

async fn serve(addr: SocketAddr, data_dir: PathBuf, auth: AuthConfig) -> Result<(), String> {
    let state = AppState::new(data_dir, auth);
    let router = app::build_router(state);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("bind {addr}: {e}"))?;
    tracing::info!(%addr, "kimetsu-remote listening");
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|e| format!("serve: {e}"))
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(_) => {
                ctrl_c.await;
                return;
            }
        };
        tokio::select! {
            _ = ctrl_c => {},
            _ = term.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        ctrl_c.await;
    }
}

fn init_tracing(filter: Option<&str>) {
    use tracing_subscriber::{EnvFilter, fmt};
    let env = filter
        .map(EnvFilter::new)
        .or_else(|| std::env::var("KIMETSU_LOG").ok().map(EnvFilter::new))
        .or_else(|| std::env::var("RUST_LOG").ok().map(EnvFilter::new))
        .unwrap_or_else(|| EnvFilter::new("info"));
    let _ = fmt().with_env_filter(env).try_init();
}
