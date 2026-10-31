//! File-backed checkpoints. Blocking SQLite work never runs on an async worker.
use super::*;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Clone)]
pub struct SqliteStore {
    connection: Arc<Mutex<Connection>>,
    namespace: String,
}

impl fmt::Debug for SqliteStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SqliteStore").finish_non_exhaustive()
    }
}

impl SqliteStore {
    pub fn open(path: PathBuf, namespace: &str) -> StoreResult<Self> {
        validate_namespace(namespace)?;
        if path.as_os_str().is_empty()
            || path.as_os_str() == ":memory:"
            || path.to_str().is_some_and(|path| path.starts_with("file:"))
        {
            return Err(StoreError::Invalid(
                "checkpoint SQLite requires a file path".into(),
            ));
        }
        // URI opening is deliberately disabled as well: URI query parameters
        // must never turn the durable checkpoint file into an in-memory DB.
        let connection = Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|_| unavailable())?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(|_| unavailable())?;
        connection
            .execute_batch(
                "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=FULL;
             CREATE TABLE IF NOT EXISTS kimetsu_checkpoints (
                 namespace TEXT NOT NULL, key TEXT NOT NULL,
                 revision TEXT NOT NULL, data BLOB NOT NULL,
                 PRIMARY KEY(namespace, key)
             );
             CREATE TABLE IF NOT EXISTS kimetsu_rate_limits (
                 namespace TEXT NOT NULL, key TEXT NOT NULL,
                 window_id INTEGER NOT NULL, count INTEGER NOT NULL,
                 PRIMARY KEY(namespace, key, window_id)
             );
             CREATE INDEX IF NOT EXISTS kimetsu_rate_expiry
                 ON kimetsu_rate_limits(namespace, window_id);",
            )
            .map_err(|_| unavailable())?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            namespace: namespace.into(),
        })
    }

    async fn with_connection<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut Connection, &str) -> StoreResult<T> + Send + 'static,
    ) -> StoreResult<T> {
        let connection = self.connection.clone();
        let namespace = self.namespace.clone();
        tokio::task::spawn_blocking(move || {
            let mut connection = connection.lock().map_err(|_| unavailable())?;
            operation(&mut connection, &namespace)
        })
        .await
        .map_err(|_| unavailable())?
    }
}

fn unavailable() -> StoreError {
    StoreError::Unavailable("sqlite")
}
fn corrupt() -> StoreError {
    StoreError::Corrupt("sqlite")
}

fn load_value(
    connection: &Connection,
    namespace: &str,
    key: &str,
) -> StoreResult<Option<StoredValue>> {
    let value = connection
        .query_row(
            "SELECT revision, data FROM kimetsu_checkpoints WHERE namespace=?1 AND key=?2",
            params![namespace, key],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )
        .optional()
        .map_err(|error| match error {
            rusqlite::Error::InvalidColumnType(..)
            | rusqlite::Error::FromSqlConversionFailure(..) => corrupt(),
            _ => unavailable(),
        })?;
    value
        .map(|(revision, data)| {
            if revision.is_empty()
                || revision.len() > 128
                || !revision
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
                || data.len() > MAX_STATE_BYTES
            {
                return Err(corrupt());
            }
            Ok(StoredValue { revision, data })
        })
        .transpose()
}

#[async_trait]
impl BrainStore for SqliteStore {
    async fn load(&self, key: &str) -> StoreResult<Option<StoredValue>> {
        validate_key(key)?;
        let key = key.to_owned();
        self.with_connection(move |connection, namespace| load_value(connection, namespace, &key))
            .await
    }

    async fn commit(&self, writes: &[StoreWrite]) -> StoreResult<CommitOutcome> {
        validate_writes(writes)?;
        if writes.is_empty() {
            return Ok(CommitOutcome::Committed);
        }
        let writes = writes.to_vec();
        self.with_connection(move |connection, namespace| {
            let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|_| unavailable())?;
            for write in &writes {
                let current = load_value(&transaction, namespace, &write.key)?;
                if current.as_ref().map(|value| value.revision.as_str()) != write.expected.as_deref() {
                    return Ok(CommitOutcome::Conflict);
                }
            }
            for write in &writes {
                transaction.execute(
                    "INSERT INTO kimetsu_checkpoints(namespace,key,revision,data) VALUES(?1,?2,?3,?4)
                     ON CONFLICT(namespace,key) DO UPDATE SET revision=excluded.revision, data=excluded.data",
                    params![namespace, write.key, write.revision, write.data],
                ).map_err(|_| unavailable())?;
            }
            transaction.commit().map_err(|_| unavailable())?;
            Ok(CommitOutcome::Committed)
        }).await
    }

    async fn allow_rate(&self, key: &str, window: u64, limit: u32) -> StoreResult<bool> {
        validate_key(key)?;
        if limit == 0 {
            return Ok(true);
        }
        let window = i64::try_from(window)
            .map_err(|_| StoreError::Invalid("rate window out of range".into()))?;
        let key = key.to_owned();
        self.with_connection(move |connection, namespace| {
            let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|_| unavailable())?;
            // A bounded sweep avoids both an unbounded request and permanent old buckets.
            transaction.execute(
                "DELETE FROM kimetsu_rate_limits WHERE rowid IN (
                   SELECT rowid FROM kimetsu_rate_limits WHERE namespace=?1 AND window_id<?2 LIMIT 128)",
                params![namespace, window.saturating_sub(2)],
            ).map_err(|_| unavailable())?;
            let count: Option<i64> = transaction.query_row(
                "SELECT count FROM kimetsu_rate_limits WHERE namespace=?1 AND key=?2 AND window_id=?3",
                params![namespace, key, window], |row| row.get(0),
            ).optional().map_err(|_| corrupt())?;
            let count = count.unwrap_or(0);
            if count < 0 { return Err(corrupt()); }
            let allowed = count < i64::from(limit);
            if allowed {
                transaction.execute(
                    "INSERT INTO kimetsu_rate_limits(namespace,key,window_id,count) VALUES(?1,?2,?3,1)
                     ON CONFLICT(namespace,key,window_id) DO UPDATE SET count=count+1",
                    params![namespace, key, window],
                ).map_err(|_| unavailable())?;
            }
            transaction.commit().map_err(|_| unavailable())?;
            Ok(allowed)
        }).await
    }
}
