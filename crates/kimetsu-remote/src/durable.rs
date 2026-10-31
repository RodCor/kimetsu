//! A durable transaction around an otherwise disposable brain invocation.
use crate::{
    projection, repo,
    storage::{BrainStore, CommitOutcome, StoreError, StoreWrite, StoredValue},
};
use kimetsu_core::{
    config::ProjectConfig,
    paths::{ProjectPaths, UserBrainScope},
};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Clone)]
pub struct DurableState {
    pub store: Arc<dyn BrainStore>,
    pub shared_org: bool,
    pub project_config: ProjectConfig,
}

#[derive(Debug)]
pub enum DurableError {
    Storage(StoreError),
    Projection(String),
    Operation(String),
    Conflict,
    Worker,
}
impl std::fmt::Display for DurableError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Storage(error) => write!(f, "{error}"),
            Self::Projection(message) | Self::Operation(message) => write!(f, "{message}"),
            Self::Conflict => write!(f, "concurrent update; retry the request"),
            Self::Worker => write!(f, "projection worker failed"),
        }
    }
}

struct Workspace {
    directory: tempfile::TempDir,
    repo: PathBuf,
    org: Option<PathBuf>,
}
impl Drop for Workspace {
    fn drop(&mut self) {
        projection::release(&self.repo);
        if let Some(root) = &self.org {
            projection::release(root);
        }
    }
}

impl DurableState {
    pub fn new(store: Arc<dyn BrainStore>, shared_org: bool) -> Self {
        // Tool handlers use ProjectPaths::discover. Remote roots are explicit
        // tenant projections and must never climb into an enclosing Git repo,
        // including when the library is embedded without run_serve.
        kimetsu_core::paths::pin_discover_to_root();
        Self {
            store,
            shared_org,
            project_config: ProjectConfig::default_for_project("remote"),
        }
    }

    pub async fn execute<F>(
        &self,
        repo_id: &str,
        origin: String,
        operation: F,
    ) -> Result<Value, DurableError>
    where
        F: Fn(&Path) -> Result<Value, String> + Send + Sync + 'static,
    {
        let repo_id = repo::sanitize_repo_id(repo_id).map_err(DurableError::Operation)?;
        let repo_key = format!("repo/{repo_id}");
        let operation = Arc::new(operation);
        for attempt in 0..8 {
            let previous_repo = self
                .store
                .load(&repo_key)
                .await
                .map_err(DurableError::Storage)?;
            let previous_org = if self.shared_org {
                self.store
                    .load("org")
                    .await
                    .map_err(DurableError::Storage)?
            } else {
                None
            };
            let repo_bytes = previous_repo.as_ref().map(|value| value.data.clone());
            let org_bytes = previous_org.as_ref().map(|value| value.data.clone());
            let shared = self.shared_org;
            let operation = operation.clone();
            let config = self.project_config.clone();
            let identity = repo_id.clone();
            let origin = origin.clone();
            let (value, repo_data, org_data) = tokio::task::spawn_blocking(move || {
                let directory = tempfile::Builder::new()
                    .prefix("kimetsu-remote-request-")
                    .tempdir()
                    .map_err(|_| {
                        DurableError::Projection("cannot create private request workspace".into())
                    })?;
                let root = directory.path().join("repo");
                let org = shared.then(|| directory.path().join("org"));
                let workspace = Workspace {
                    directory,
                    repo: root,
                    org,
                };
                let _user = UserBrainScope::with_cache(
                    workspace.org.as_ref().map(|root| root.join(".kimetsu")),
                    workspace.directory.path().join("cache"),
                );
                let _origin = kimetsu_core::event::OriginScope::new(origin);
                initialize(&workspace.repo, repo_bytes.as_deref(), &config, &identity)?;
                if let Some(org) = &workspace.org {
                    initialize(org, org_bytes.as_deref(), &config, "org")?;
                }
                let value = operation(&workspace.repo).map_err(DurableError::Operation)?;
                let repo_data =
                    projection::capture(&workspace.repo).map_err(DurableError::Projection)?;
                let org_data = workspace
                    .org
                    .as_ref()
                    .map(|root| projection::capture(root))
                    .transpose()
                    .map_err(DurableError::Projection)?;
                Ok::<_, DurableError>((value, repo_data, org_data))
            })
            .await
            .map_err(|_| DurableError::Worker)??;

            let mut writes = Vec::new();
            changed(&mut writes, &repo_key, previous_repo, repo_data);
            if let Some(data) = org_data {
                changed(&mut writes, "org", previous_org, data);
            }
            if writes.is_empty() {
                return Ok(value);
            }
            match self
                .store
                .commit(&writes)
                .await
                .map_err(DurableError::Storage)?
            {
                CommitOutcome::Committed => return Ok(value),
                CommitOutcome::Conflict => {
                    tokio::time::sleep(std::time::Duration::from_millis(3 + attempt * 3)).await
                }
            }
        }
        Err(DurableError::Conflict)
    }
}

fn changed(writes: &mut Vec<StoreWrite>, key: &str, previous: Option<StoredValue>, data: Vec<u8>) {
    if previous.as_ref().is_some_and(|old| old.data == data) {
        return;
    }
    writes.push(StoreWrite {
        key: key.to_string(),
        expected: previous.map(|old| old.revision),
        revision: ulid::Ulid::new().to_string(),
        data,
    });
}

fn initialize(
    root: &Path,
    snapshot: Option<&[u8]>,
    template: &ProjectConfig,
    identity: &str,
) -> Result<(), DurableError> {
    if let Some(data) = snapshot {
        return projection::restore(root, data).map_err(DurableError::Projection);
    }
    repo::ensure_initialized(root).map_err(DurableError::Projection)?;
    let mut config = template.clone();
    config.kimetsu.project_id = identity.to_string();
    std::fs::write(
        ProjectPaths::at_root(root).project_toml,
        config
            .to_toml()
            .map_err(|_| DurableError::Projection("invalid project template".into()))?,
    )
    .map_err(|_| DurableError::Projection("cannot initialize project configuration".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{StoreResult, sqlite::SqliteStore};
    use async_trait::async_trait;
    use kimetsu_core::memory::{MemoryKind, MemoryScope};

    fn durable(store: Arc<dyn BrainStore>, org: bool) -> DurableState {
        let mut state = DurableState::new(store, org);
        state.project_config.embedder.enabled = false;
        state
    }

    #[tokio::test]
    async fn independent_instances_keep_concurrent_writes_and_org_scope() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("authority.db");
        let mut tasks = Vec::new();
        for idx in 0..4 {
            let store = Arc::new(SqliteStore::open(path.clone(), "test").unwrap());
            tasks.push(tokio::spawn(async move {
                durable(store, true)
                    .execute("one", "test/user:one".into(), move |root| {
                        kimetsu_brain::project::add_memory(
                            root,
                            MemoryScope::Project,
                            MemoryKind::Convention,
                            &format!("Durable project marker number {idx}"),
                        )
                        .map(|id| serde_json::json!({"id":id}))
                        .map_err(|e| e.to_string())
                    })
                    .await
                    .unwrap()
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        let fresh = durable(Arc::new(SqliteStore::open(path, "test").unwrap()), true);
        let rows = fresh
            .execute("one", "test".into(), |root| {
                let (_, _, conn) = kimetsu_brain::project::load_project_at_root(root)
                    .map_err(|e| e.to_string())?;
                let count: i64 = conn
                    .query_row("SELECT COUNT(*) FROM memories", [], |row| row.get(0))
                    .map_err(|e| e.to_string())?;
                Ok(serde_json::json!(count))
            })
            .await
            .unwrap();
        assert_eq!(rows, 4);
        fresh
            .execute("one", "test".into(), |root| {
                kimetsu_brain::project::add_memory(
                    root,
                    MemoryScope::GlobalUser,
                    MemoryKind::Convention,
                    "Shared Orchid port is 7319",
                )
                .map(|_| Value::Null)
                .map_err(|e| e.to_string())
            })
            .await
            .unwrap();
        let shared = fresh
            .execute("two", "test".into(), |_root| {
                let conn = kimetsu_brain::user_brain::open_user_brain_readonly()
                    .map_err(|e| e.to_string())?
                    .unwrap();
                let count: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM memories WHERE text LIKE '%Orchid%'",
                        [],
                        |row| row.get(0),
                    )
                    .map_err(|e| e.to_string())?;
                Ok(serde_json::json!(count))
            })
            .await
            .unwrap();
        assert_eq!(shared, 1);
    }

    struct RejectCommit(Arc<dyn BrainStore>);
    #[async_trait]
    impl BrainStore for RejectCommit {
        async fn load(&self, key: &str) -> StoreResult<Option<StoredValue>> {
            self.0.load(key).await
        }
        async fn commit(&self, _: &[StoreWrite]) -> StoreResult<CommitOutcome> {
            Err(StoreError::Unavailable("test"))
        }
        async fn allow_rate(&self, _: &str, _: u64, _: u32) -> StoreResult<bool> {
            Ok(true)
        }
    }

    #[tokio::test]
    async fn failed_publication_never_acknowledges_or_updates_either_brain() {
        let home = tempfile::tempdir().unwrap();
        let store = Arc::new(SqliteStore::open(home.path().join("authority.db"), "test").unwrap());
        let result = durable(Arc::new(RejectCommit(store.clone())), true)
            .execute("one", "test".into(), |root| {
                for scope in [MemoryScope::Project, MemoryScope::GlobalUser] {
                    kimetsu_brain::project::add_memory(
                        root,
                        scope,
                        MemoryKind::Convention,
                        "Must never be acknowledged",
                    )
                    .map_err(|e| e.to_string())?;
                }
                Ok(Value::Bool(true))
            })
            .await;
        assert!(matches!(result, Err(DurableError::Storage(_))));
        assert!(store.load("repo/one").await.unwrap().is_none());
        assert!(store.load("org").await.unwrap().is_none());
    }
}
