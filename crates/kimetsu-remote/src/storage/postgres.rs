//! Transactional PostgreSQL checkpoints with deterministic row lock ordering.
use super::*;
use postgres_native_tls::MakeTlsConnector;
use tokio::sync::Mutex;
use tokio_postgres::{Client, Row, error::SqlState};

pub struct PostgresStore {
    client: Mutex<Client>,
    namespace: String,
}

impl fmt::Debug for PostgresStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PostgresStore").finish_non_exhaustive()
    }
}

impl PostgresStore {
    pub async fn connect(url: &str, namespace: &str) -> StoreResult<Self> {
        validate_namespace(namespace)?;
        let tls = native_tls::TlsConnector::builder()
            .build()
            .map_err(|_| unavailable())?;
        let (client, connection) = tokio_postgres::connect(url, MakeTlsConnector::new(tls))
            .await
            .map_err(|_| unavailable())?;
        // Driver errors can include endpoint details. Callers receive a sanitized
        // error from their next operation, never the provider's diagnostic text.
        tokio::spawn(async move {
            let _ = connection.await;
        });
        client
            .batch_execute(
                "CREATE TABLE IF NOT EXISTS kimetsu_checkpoints (
                 namespace TEXT NOT NULL, key TEXT NOT NULL,
                 revision TEXT NOT NULL, data BYTEA NOT NULL,
                 PRIMARY KEY(namespace,key)
             );
             CREATE TABLE IF NOT EXISTS kimetsu_rate_limits (
                 namespace TEXT NOT NULL, key TEXT NOT NULL,
                 window_id BIGINT NOT NULL, count BIGINT NOT NULL,
                 PRIMARY KEY(namespace,key,window_id)
             );
             CREATE INDEX IF NOT EXISTS kimetsu_rate_expiry
                 ON kimetsu_rate_limits(namespace,window_id);",
            )
            .await
            .map_err(|_| unavailable())?;
        Ok(Self {
            client: Mutex::new(client),
            namespace: namespace.into(),
        })
    }
}

fn unavailable() -> StoreError {
    StoreError::Unavailable("postgres")
}
fn corrupt() -> StoreError {
    StoreError::Corrupt("postgres")
}

fn decode_value(row: Row) -> StoreResult<StoredValue> {
    let revision: String = row.try_get(0).map_err(|_| corrupt())?;
    let data: Vec<u8> = row.try_get(1).map_err(|_| corrupt())?;
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
}

#[async_trait]
impl BrainStore for PostgresStore {
    async fn load(&self, key: &str) -> StoreResult<Option<StoredValue>> {
        validate_key(key)?;
        self.client
            .lock()
            .await
            .query_opt(
                "SELECT revision,data FROM kimetsu_checkpoints WHERE namespace=$1 AND key=$2",
                &[&self.namespace, &key],
            )
            .await
            .map_err(|_| unavailable())?
            .map(decode_value)
            .transpose()
    }

    async fn commit(&self, writes: &[StoreWrite]) -> StoreResult<CommitOutcome> {
        validate_writes(writes)?;
        if writes.is_empty() {
            return Ok(CommitOutcome::Committed);
        }
        let mut ordered: Vec<_> = writes.iter().collect();
        ordered.sort_unstable_by(|a, b| a.key.cmp(&b.key));
        let mut client = self.client.lock().await;
        let transaction = client.transaction().await.map_err(|_| unavailable())?;
        for write in &ordered {
            let current = transaction.query_opt(
                "SELECT revision,data FROM kimetsu_checkpoints WHERE namespace=$1 AND key=$2 FOR UPDATE",
                &[&self.namespace, &write.key],
            ).await.map_err(|_| unavailable())?.map(decode_value).transpose()?;
            if current.as_ref().map(|value| value.revision.as_str()) != write.expected.as_deref() {
                return Ok(CommitOutcome::Conflict);
            }
        }
        for write in ordered {
            let result = if write.expected.is_none() {
                // Missing rows cannot be locked by SELECT. Concurrent insertion
                // loses on the unique constraint and rolls back the whole batch.
                transaction.execute(
                    "INSERT INTO kimetsu_checkpoints(namespace,key,revision,data) VALUES($1,$2,$3,$4)",
                    &[&self.namespace, &write.key, &write.revision, &write.data],
                ).await
            } else {
                transaction.execute(
                    "UPDATE kimetsu_checkpoints SET revision=$3,data=$4 WHERE namespace=$1 AND key=$2",
                    &[&self.namespace, &write.key, &write.revision, &write.data],
                ).await
            };
            match result {
                Ok(_) => {}
                Err(error) if error.code() == Some(&SqlState::UNIQUE_VIOLATION) => {
                    return Ok(CommitOutcome::Conflict);
                }
                Err(_) => return Err(unavailable()),
            }
        }
        transaction.commit().await.map_err(|_| unavailable())?;
        Ok(CommitOutcome::Committed)
    }

    async fn allow_rate(&self, key: &str, window: u64, limit: u32) -> StoreResult<bool> {
        validate_key(key)?;
        if limit == 0 {
            return Ok(true);
        }
        let window = i64::try_from(window)
            .map_err(|_| StoreError::Invalid("rate window out of range".into()))?;
        let mut client = self.client.lock().await;
        let transaction = client.transaction().await.map_err(|_| unavailable())?;
        // SKIP LOCKED prevents cleanup from waiting on another request's buckets.
        transaction
            .execute(
                "DELETE FROM kimetsu_rate_limits WHERE ctid IN (
                SELECT ctid FROM kimetsu_rate_limits
                WHERE namespace=$1 AND window_id<$2 LIMIT 128 FOR UPDATE SKIP LOCKED)",
                &[&self.namespace, &window.saturating_sub(2)],
            )
            .await
            .map_err(|_| unavailable())?;
        // The conditional upsert both increments and checks the shared limit in
        // one row operation, including concurrent first use of a token/window.
        let row = transaction
            .query_opt(
                "INSERT INTO kimetsu_rate_limits(namespace,key,window_id,count) VALUES($1,$2,$3,1)
             ON CONFLICT(namespace,key,window_id) DO UPDATE SET count=kimetsu_rate_limits.count+1
             WHERE kimetsu_rate_limits.count>=0 AND kimetsu_rate_limits.count<$4 RETURNING count",
                &[&self.namespace, &key, &window, &i64::from(limit)],
            )
            .await
            .map_err(|_| unavailable())?;
        let allowed = row.is_some();
        if !allowed {
            let count: i64 = transaction.query_one(
                "SELECT count FROM kimetsu_rate_limits WHERE namespace=$1 AND key=$2 AND window_id=$3",
                &[&self.namespace, &key, &window],
            ).await.map_err(|_| unavailable())?.try_get(0).map_err(|_| corrupt())?;
            if count < 0 {
                return Err(corrupt());
            }
        }
        transaction.commit().await.map_err(|_| unavailable())?;
        Ok(allowed)
    }
}
