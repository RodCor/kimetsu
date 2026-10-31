//! Durable remote state, independent of disposable request projections.
//! Constructors: SqliteStore::open(PathBuf, &str), async
//! PostgresStore::connect(&str, &str), RedisStore::connect(&str, &str), and
//! DynamoDbStore::connect(table: &str, namespace: &str, region: Option<&str>, endpoint: Option<&str>).

use async_trait::async_trait;
use std::fmt;

#[cfg(feature = "dynamodb")]
pub mod dynamodb;
#[cfg(feature = "postgres")]
pub mod postgres;
#[cfg(feature = "redis")]
pub mod redis;
pub mod sqlite;

pub const MAX_STATE_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_BATCH_WRITES: usize = 4;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredValue {
    pub revision: String,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct StoreWrite {
    pub key: String,
    pub expected: Option<String>,
    pub revision: String,
    pub data: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitOutcome {
    Committed,
    Conflict,
}

/// Public errors deliberately avoid embedding connection URLs or SDK errors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreError {
    Invalid(String),
    Unavailable(&'static str),
    Corrupt(&'static str),
}
impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) => write!(f, "invalid storage request: {message}"),
            Self::Unavailable(backend) => write!(f, "{backend} storage unavailable"),
            Self::Corrupt(backend) => write!(f, "{backend} storage record is corrupt"),
        }
    }
}
impl std::error::Error for StoreError {}
pub type StoreResult<T> = Result<T, StoreError>;

#[async_trait]
pub trait BrainStore: Send + Sync {
    async fn load(&self, key: &str) -> StoreResult<Option<StoredValue>>;
    /// All expected revisions are checked before any value is published.
    /// Conflict must leave every previous value intact, including absent keys.
    async fn commit(&self, writes: &[StoreWrite]) -> StoreResult<CommitOutcome>;
    /// Atomic fixed-minute limit across independent server instances. `window`
    /// is UTC unix_seconds / 60. Zero limit is disabled. Keys are token hashes.
    async fn allow_rate(&self, key: &str, window: u64, limit: u32) -> StoreResult<bool>;
}

pub fn validate_namespace(namespace: &str) -> StoreResult<()> {
    if namespace.is_empty()
        || namespace.len() > 64
        || !namespace
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(StoreError::Invalid(
            "namespace must be 1-64 ASCII letters, digits, '.', '-' or '_'".into(),
        ));
    }
    Ok(())
}

pub fn validate_key(key: &str) -> StoreResult<()> {
    if key.is_empty() || key.len() > 512 || key.chars().any(char::is_control) {
        return Err(StoreError::Invalid(
            "key must be 1-512 bytes without control characters".into(),
        ));
    }
    Ok(())
}

pub fn validate_writes(writes: &[StoreWrite]) -> StoreResult<()> {
    if writes.len() > MAX_BATCH_WRITES {
        return Err(StoreError::Invalid("too many writes in one commit".into()));
    }
    let mut keys = std::collections::HashSet::new();
    for write in writes {
        validate_key(&write.key)?;
        if !keys.insert(&write.key) {
            return Err(StoreError::Invalid("duplicate key in commit".into()));
        }
        if write.data.len() > MAX_STATE_BYTES {
            return Err(StoreError::Invalid("state exceeds 64 MiB".into()));
        }
        for revision in std::iter::once(&write.revision).chain(write.expected.iter()) {
            if revision.is_empty()
                || revision.len() > 128
                || !revision
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
            {
                return Err(StoreError::Invalid("invalid revision identifier".into()));
            }
        }
    }
    Ok(())
}
