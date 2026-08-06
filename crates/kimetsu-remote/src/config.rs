//! CLI args + auth assembly for `kimetsu-remote serve`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Args;
use serde::Deserialize;

use crate::{auth::AuthConfig, repo};

#[derive(Debug, Args)]
pub struct ServeArgs {
    /// Address to bind, e.g. 127.0.0.1:8787.
    #[arg(long, default_value = "127.0.0.1:8787")]
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
    #[arg(long, default_value_t = 60)]
    pub rate_limit: u32,

    /// Allow plaintext HTTP on a non-loopback bind address. Prefer TLS or a
    /// local reverse proxy; this flag exists for explicitly trusted networks.
    #[arg(long)]
    pub allow_public_http: bool,

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

    /// Operator-level cross-encoder rerank stage for `kimetsu_brain_context`.
    /// `"off"` disables reranking. Any curated or HuggingFace model id is accepted.
    /// Default chosen by the 100-memory benchmark: jina-tiny MRR 0.931 vs 0.914
    /// for tinybert; remote has no hook-latency budget so the fastest reranker wins.
    #[arg(long, default_value = "jina-reranker-v1-tiny-en")]
    pub reranker: String,

    /// v2.6 #3 Slice C: this server's node id — the HLC node and the machine part
    /// of each write's `origin` (`<node>/user:<name>`). Defaults to the hostname,
    /// else "remote". Keep it stable + unique per server so a synced team brain can
    /// attribute and order server-written events.
    #[arg(long)]
    pub node: Option<String>,

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
    /// v2.6 #3 Slice C: token → display name for per-user write attribution.
    /// Optional `[names]` table; absent in pre-Slice-C tokens files.
    #[serde(default)]
    names: HashMap<String, String>,
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
            let canonical = repo::sanitize_repo_id(&repo)
                .map_err(|e| format!("invalid per_repo token key {repo:?}: {e}"))?;
            if auth.per_repo.contains_key(&canonical) {
                return Err(format!(
                    "duplicate per_repo token key after canonicalization: {repo:?} -> {canonical:?}"
                ));
            }
            auth.per_repo.insert(canonical, toks);
        }
        // Slice C: token → display name for write attribution.
        auth.token_names.extend(parsed.names);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn serve_args(tokens_file: PathBuf) -> ServeArgs {
        ServeArgs {
            addr: "127.0.0.1:8787".parse().expect("addr"),
            data: std::env::temp_dir().join("kimetsu-remote-config-test"),
            tokens: Vec::new(),
            tokens_file: Some(tokens_file),
            max_blocking_threads: 64,
            rate_limit: 60,
            allow_public_http: false,
            org_brain: None,
            repos_file: None,
            checkout_dir: None,
            tls_cert: None,
            tls_key: None,
            reranker: "jina-reranker-v1-tiny-en".to_string(),
            node: None,
            log: None,
        }
    }

    #[test]
    fn per_repo_tokens_are_canonicalized() {
        let path =
            std::env::temp_dir().join(format!("kimetsu-remote-tokens-{}.toml", std::process::id()));
        std::fs::write(
            &path,
            r#"
[per_repo]
Acme-API = ["tok"]
"#,
        )
        .expect("write tokens file");

        let auth = build_auth(&serve_args(path.clone())).expect("build auth");
        assert!(auth.per_repo.contains_key("acme-api"));
        assert!(!auth.per_repo.contains_key("Acme-API"));

        std::fs::remove_file(path).ok();
    }

    #[test]
    fn duplicate_canonical_per_repo_tokens_fail() {
        let path = std::env::temp_dir().join(format!(
            "kimetsu-remote-tokens-dup-{}.toml",
            std::process::id()
        ));
        std::fs::write(
            &path,
            r#"
[per_repo]
Acme-API = ["tok1"]
acme-api = ["tok2"]
"#,
        )
        .expect("write tokens file");

        let err = build_auth(&serve_args(path.clone())).expect_err("duplicate keys must fail");
        assert!(err.contains("duplicate per_repo token key after canonicalization"));

        std::fs::remove_file(path).ok();
    }
}
