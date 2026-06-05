//! Server-hosted Kimetsu brain over HTTP MCP. Library half (so integration
//! tests can drive the router); the `kimetsu-remote` binary is a thin wrapper.

pub mod app;
pub mod auth;
pub mod catalog;
pub mod config;
pub mod metrics;
pub mod ratelimit;
pub mod repo;
pub mod rpc;
pub mod state;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

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
    let state = AppState::with_rate_limit(data_dir, auth, args.rate_limit);

    let tls = match (args.tls_cert.clone(), args.tls_key.clone()) {
        (Some(cert), Some(key)) => Some((cert, key)),
        _ => None,
    };
    #[cfg(not(feature = "tls"))]
    if tls.is_some() {
        return Err(
            "this build has no TLS support — rebuild `kimetsu-remote --features tls`, or \
             terminate TLS at a reverse proxy (nginx/Caddy)"
                .to_string(),
        );
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .max_blocking_threads(args.max_blocking_threads.max(1))
        .build()
        .map_err(|e| format!("build runtime: {e}"))?;

    runtime.block_on(serve(args.addr, state, tls))
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

async fn serve(
    addr: SocketAddr,
    state: AppState,
    tls: Option<(PathBuf, PathBuf)>,
) -> Result<(), String> {
    let router = app::build_router(state);
    match tls {
        #[cfg(feature = "tls")]
        Some((cert, key)) => serve_tls(addr, router, cert, key).await,
        #[cfg(not(feature = "tls"))]
        Some(_) => Err("TLS requested but this build has no `tls` feature".to_string()),
        None => serve_plain(addr, router).await,
    }
}

async fn serve_plain(addr: SocketAddr, router: axum::Router) -> Result<(), String> {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("bind {addr}: {e}"))?;
    tracing::info!(%addr, "kimetsu-remote listening (http)");
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|e| format!("serve: {e}"))
}

#[cfg(feature = "tls")]
async fn serve_tls(
    addr: SocketAddr,
    router: axum::Router,
    cert: PathBuf,
    key: PathBuf,
) -> Result<(), String> {
    // Pin the ring crypto provider (we build rustls without aws-lc-rs).
    let _ = rustls::crypto::ring::default_provider().install_default();
    let config = axum_server::tls_rustls::RustlsConfig::from_pem_file(&cert, &key)
        .await
        .map_err(|e| format!("load TLS cert/key: {e}"))?;
    let handle = axum_server::Handle::new();
    let shutdown_handle = handle.clone();
    tokio::spawn(async move {
        shutdown_signal().await;
        shutdown_handle.graceful_shutdown(Some(std::time::Duration::from_secs(10)));
    });
    tracing::info!(%addr, "kimetsu-remote listening (https)");
    axum_server::bind_rustls(addr, config)
        .handle(handle)
        .serve(router.into_make_service())
        .await
        .map_err(|e| format!("serve tls: {e}"))
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
