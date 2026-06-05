//! CLI args + auth assembly for `kimetsu-remote serve`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Args;
use serde::Deserialize;

use crate::auth::AuthConfig;

#[derive(Debug, Args)]
pub struct ServeArgs {
    /// Address to bind, e.g. 0.0.0.0:8787.
    #[arg(long, default_value = "0.0.0.0:8787")]
    pub addr: SocketAddr,

    /// Directory holding one brain per repo (`<data>/<repo-id>/.kimetsu/`).
    /// Should live OUTSIDE any git repository.
    #[arg(long)]
    pub data: PathBuf,

    /// A bearer token valid for every repo. Repeatable. Also accepts a
    /// comma-separated list via `KIMETSU_REMOTE_TOKEN`.
    #[arg(long = "token")]
    pub tokens: Vec<String>,

    /// TOML file of tokens: `global = [..]` plus a `[per_repo]` map.
    #[arg(long)]
    pub tokens_file: Option<PathBuf>,

    /// Upper bound on blocking threads (each request runs the brain on one).
    #[arg(long, default_value_t = 64)]
    pub max_blocking_threads: usize,

    /// Per-token request rate limit (requests/minute). `0` disables it.
    #[arg(long, default_value_t = 0)]
    pub rate_limit: u32,

    /// Enable a shared org brain at <dir>: `global_user`-scoped memories are
    /// stored here and merged into EVERY repo's retrieval (cross-project team
    /// memory). Off by default (each repo brain is standalone). Must be a path
    /// OUTSIDE --data.
    #[arg(long)]
    pub org_brain: Option<PathBuf>,

    /// Enable server-side ingest from a TOML file registering repo-id → git URL
    /// (`[repos]` table). The server clones/refreshes each registered repo and
    /// `kimetsu_brain_ingest_repo` indexes its files into that repo's brain.
    /// Requires --checkout-dir.
    #[arg(long, requires = "checkout_dir")]
    pub repos_file: Option<PathBuf>,

    /// Where server-side ingest keeps its managed git checkouts. Must be OUTSIDE
    /// --data.
    #[arg(long, requires = "repos_file")]
    pub checkout_dir: Option<PathBuf>,

    /// TLS certificate chain (PEM). Serve HTTPS directly when set with --tls-key
    /// (otherwise plain HTTP — terminate TLS at a reverse proxy).
    #[arg(long, requires = "tls_key")]
    pub tls_cert: Option<PathBuf>,

    /// TLS private key (PEM). Pair with --tls-cert.
    #[arg(long, requires = "tls_cert")]
    pub tls_key: Option<PathBuf>,

    /// Tracing filter (else `RUST_LOG` / `KIMETSU_LOG`).
    #[arg(long)]
    pub log: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct TokensFile {
    #[serde(default)]
    global: Vec<String>,
    #[serde(default)]
    per_repo: HashMap<String, Vec<String>>,
}

/// Assemble the auth config from `--token`, `KIMETSU_REMOTE_TOKEN`, and an
/// optional `--tokens-file`. Errors if the result is empty (we refuse to run a
/// server that accepts any request).
pub fn build_auth(args: &ServeArgs) -> Result<AuthConfig, String> {
    let mut auth = AuthConfig::default();

    auth.global.extend(args.tokens.iter().cloned());

    if let Ok(env) = std::env::var("KIMETSU_REMOTE_TOKEN") {
        auth.global.extend(
            env.split(',')
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_string),
        );
    }

    if let Some(path) = &args.tokens_file {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("read tokens file {}: {e}", path.display()))?;
        let parsed: TokensFile =
            toml::from_str(&text).map_err(|e| format!("parse tokens file: {e}"))?;
        auth.global.extend(parsed.global);
        for (repo, toks) in parsed.per_repo {
            auth.per_repo.entry(repo).or_default().extend(toks);
        }
    }

    auth.global.retain(|t| !t.trim().is_empty());
    for toks in auth.per_repo.values_mut() {
        toks.retain(|t| !t.trim().is_empty());
    }

    if auth.is_empty() {
        return Err(
            "no tokens configured — pass --token, KIMETSU_REMOTE_TOKEN, or --tokens-file \
             (refusing to start an unauthenticated server)"
                .to_string(),
        );
    }
    Ok(auth)
}
