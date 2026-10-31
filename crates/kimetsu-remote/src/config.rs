//! CLI args + auth assembly for `kimetsu-remote serve`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Args, ValueEnum};
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
    pub data: Option<PathBuf>,

    /// Durable checkpoint backend. Omit to retain --data local-file mode.
    #[arg(long)]
    pub storage: Option<StorageBackend>,
    /// Backend connection URL/path; prefer KIMETSU_REMOTE_DATABASE_URL for credentials.
    #[arg(long)]
    pub database_url: Option<SecretString>,
    #[arg(long)]
    pub storage_namespace: Option<String>,
    #[arg(long)]
    pub dynamodb_table: Option<String>,
    #[arg(long)]
    pub aws_region: Option<String>,
    /// Operator-owned DynamoDB endpoint override (e.g. local emulator).
    #[arg(long)]
    pub dynamodb_endpoint: Option<SecretString>,
    /// Shared org checkpoint in the durable store; replaces --org-brain PATH.
    #[arg(long)]
    pub shared_org_brain: bool,

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

#[derive(Clone)]
pub struct SecretString(String);
impl std::fmt::Debug for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}
impl std::str::FromStr for SecretString {
    type Err = std::convert::Infallible;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(Self(value.to_string()))
    }
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum StorageBackend {
    Sqlite,
    #[value(alias = "postgresql")]
    Postgres,
    Redis,
    Dynamodb,
}

#[derive(Debug, Clone)]
pub struct StorageSettings {
    pub backend: StorageBackend,
    pub namespace: String,
    url: Option<SecretString>,
    #[cfg(feature = "dynamodb")]
    table: Option<String>,
    #[cfg(feature = "dynamodb")]
    region: Option<String>,
    #[cfg(feature = "dynamodb")]
    endpoint: Option<SecretString>,
    pub shared_org: bool,
}
impl StorageSettings {
    pub fn from_args(args: &ServeArgs) -> Result<Option<Self>, String> {
        Self::resolve(args, |key| std::env::var(key).ok())
    }
    fn resolve(
        args: &ServeArgs,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Option<Self>, String> {
        let backend = match args.storage {
            Some(backend) => Some(backend),
            None => env("KIMETSU_REMOTE_STORAGE")
                .map(|value| {
                    StorageBackend::from_str(&value, true)
                        .map_err(|_| "unknown remote storage backend".to_string())
                })
                .transpose()?,
        };
        let url = args
            .database_url
            .clone()
            .or_else(|| env("KIMETSU_REMOTE_DATABASE_URL").map(SecretString));
        let Some(backend) = backend else {
            if url.is_some() || args.shared_org_brain {
                return Err("select --storage before configuring durable storage".into());
            }
            if args.data.is_none() {
                return Err(
                    "local mode requires --data; or select --storage for stateless mode".into(),
                );
            }
            return Ok(None);
        };
        if args.org_brain.is_some() {
            return Err("durable mode uses --shared-org-brain, not --org-brain PATH".into());
        }
        let namespace = args
            .storage_namespace
            .clone()
            .or_else(|| env("KIMETSU_REMOTE_STORAGE_NAMESPACE"))
            .unwrap_or_else(|| "default".into());
        crate::storage::validate_namespace(&namespace).map_err(|e| e.to_string())?;
        let table = args
            .dynamodb_table
            .clone()
            .or_else(|| env("KIMETSU_REMOTE_DYNAMODB_TABLE"));
        if backend == StorageBackend::Dynamodb {
            if table.as_ref().is_none_or(|value| value.trim().is_empty()) {
                return Err(
                    "DynamoDB storage requires --dynamodb-table or KIMETSU_REMOTE_DYNAMODB_TABLE"
                        .into(),
                );
            }
        } else if url.as_ref().is_none_or(|value| value.0.trim().is_empty()) {
            return Err(
                "selected storage requires --database-url or KIMETSU_REMOTE_DATABASE_URL".into(),
            );
        }
        let shared_org = if args.shared_org_brain {
            true
        } else {
            match env("KIMETSU_REMOTE_SHARED_ORG")
                .as_deref()
                .map(str::to_ascii_lowercase)
                .as_deref()
            {
                None | Some("0" | "false" | "off") => false,
                Some("1" | "true" | "on") => true,
                Some(_) => return Err("KIMETSU_REMOTE_SHARED_ORG must be true or false".into()),
            }
        };
        Ok(Some(Self {
            backend,
            namespace,
            url,
            #[cfg(feature = "dynamodb")]
            table,
            #[cfg(feature = "dynamodb")]
            region: args
                .aws_region
                .clone()
                .or_else(|| env("AWS_REGION"))
                .or_else(|| env("AWS_DEFAULT_REGION")),
            #[cfg(feature = "dynamodb")]
            endpoint: args
                .dynamodb_endpoint
                .clone()
                .or_else(|| env("KIMETSU_REMOTE_DYNAMODB_ENDPOINT").map(SecretString)),
            shared_org,
        }))
    }

    pub async fn open(&self) -> Result<std::sync::Arc<dyn crate::storage::BrainStore>, String> {
        use std::sync::Arc;
        let operation = async {
            match self.backend {
                StorageBackend::Sqlite => {
                    let raw = &self.url.as_ref().ok_or("missing database URL")?.0;
                    let path = raw.strip_prefix("sqlite://").unwrap_or(raw);
                    let store = crate::storage::sqlite::SqliteStore::open(
                        PathBuf::from(path),
                        &self.namespace,
                    )
                    .map_err(|e| e.to_string())?;
                    Ok(Arc::new(store) as Arc<dyn crate::storage::BrainStore>)
                }
                StorageBackend::Postgres => {
                    #[cfg(feature = "postgres")]
                    {
                        Ok(Arc::new(
                            crate::storage::postgres::PostgresStore::connect(
                                &self.url.as_ref().ok_or("missing database URL")?.0,
                                &self.namespace,
                            )
                            .await
                            .map_err(|e| e.to_string())?,
                        )
                            as Arc<dyn crate::storage::BrainStore>)
                    }
                    #[cfg(not(feature = "postgres"))]
                    {
                        Err(
                            "this build needs --features postgres for PostgreSQL storage"
                                .to_string(),
                        )
                    }
                }
                StorageBackend::Redis => {
                    #[cfg(feature = "redis")]
                    {
                        Ok(Arc::new(
                            crate::storage::redis::RedisStore::connect(
                                &self.url.as_ref().ok_or("missing database URL")?.0,
                                &self.namespace,
                            )
                            .await
                            .map_err(|e| e.to_string())?,
                        )
                            as Arc<dyn crate::storage::BrainStore>)
                    }
                    #[cfg(not(feature = "redis"))]
                    {
                        Err("this build needs --features redis for Redis storage".to_string())
                    }
                }
                StorageBackend::Dynamodb => {
                    #[cfg(feature = "dynamodb")]
                    {
                        Ok(Arc::new(
                            crate::storage::dynamodb::DynamoDbStore::connect(
                                self.table.as_deref().ok_or("missing table")?,
                                &self.namespace,
                                self.region.as_deref(),
                                self.endpoint.as_ref().map(|v| v.0.as_str()),
                            )
                            .await
                            .map_err(|e| e.to_string())?,
                        )
                            as Arc<dyn crate::storage::BrainStore>)
                    }
                    #[cfg(not(feature = "dynamodb"))]
                    {
                        Err("this build needs --features dynamodb for DynamoDB storage".to_string())
                    }
                }
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(20), operation)
            .await
            .map_err(|_| "storage initialization timed out".to_string())?
    }
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
            data: Some(std::env::temp_dir().join("kimetsu-remote-config-test")),
            storage: None,
            database_url: None,
            storage_namespace: None,
            dynamodb_table: None,
            aws_region: None,
            dynamodb_endpoint: None,
            shared_org_brain: false,
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
    fn durable_settings_are_explicit_and_redact_connection_credentials() {
        let mut args = serve_args(PathBuf::new());
        args.data = None;
        assert!(StorageSettings::resolve(&args, |_| None).is_err());
        args.storage = Some(StorageBackend::Postgres);
        args.database_url = Some(SecretString(
            "postgres://user:private-password@localhost/db".into(),
        ));
        let settings = StorageSettings::resolve(&args, |_| None).unwrap().unwrap();
        assert!(!format!("{settings:?}").contains("private-password"));
        args.storage = None;
        assert!(
            StorageSettings::resolve(&args, |_| None).is_err(),
            "a URL alone cannot fall back to local mode"
        );
        args.database_url = None;
        let settings = StorageSettings::resolve(&args, |key| match key {
            "KIMETSU_REMOTE_STORAGE" => Some("redis".into()),
            "KIMETSU_REMOTE_DATABASE_URL" => Some("redis://127.0.0.1".into()),
            "KIMETSU_REMOTE_SHARED_ORG" => Some("true".into()),
            _ => None,
        })
        .unwrap()
        .unwrap();
        assert_eq!(settings.backend, StorageBackend::Redis);
        assert!(settings.shared_org);
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
