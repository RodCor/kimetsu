//! Redis checkpoints live in a single durable namespace hash. Lua preflights
//! every revision before publishing the entire batch with exactly one HSET.
use super::*;
use ::redis::aio::ConnectionManager;

#[derive(Clone)]
pub struct RedisStore {
    connection: ConnectionManager,
    root: String,
    rate_prefix: String,
}

impl fmt::Debug for RedisStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RedisStore").finish_non_exhaustive()
    }
}

impl RedisStore {
    pub async fn connect(url: &str, namespace: &str) -> StoreResult<Self> {
        validate_namespace(namespace)?;
        let client = ::redis::Client::open(url).map_err(|_| unavailable())?;
        let mut connection = ConnectionManager::new(client)
            .await
            .map_err(|_| unavailable())?;
        let _: String = ::redis::cmd("PING")
            .query_async(&mut connection)
            .await
            .map_err(|_| unavailable())?;
        Ok(Self {
            connection,
            root: format!("kimetsu:{{{namespace}}}:checkpoints"),
            rate_prefix: format!("kimetsu:{{{namespace}}}:rate:"),
        })
    }
}

fn unavailable() -> StoreError {
    StoreError::Unavailable("redis")
}
fn corrupt() -> StoreError {
    StoreError::Corrupt("redis")
}

// Distinct fixed prefixes make revision and data fields collision-free for all
// valid keys, even keys that themselves start with "r:" or contain colons.
fn revision_field(key: &str) -> String {
    format!("r:{key}")
}
fn data_field(key: &str) -> String {
    format!("d:{key}")
}

const LOAD: &str = r#"
local kind = redis.call('TYPE', KEYS[1]).ok
if kind == 'none' then return {0, false, false} end
if kind ~= 'hash' then return {2, false, false} end
local pair = redis.call('HMGET', KEYS[1], ARGV[1], ARGV[2])
if not pair[1] and not pair[2] then return {0, false, false} end
if not pair[1] or not pair[2] then return {2, false, false} end
return {1, pair[1], pair[2]}
"#;

const COMMIT: &str = r#"
local kind = redis.call('TYPE', KEYS[1]).ok
if kind ~= 'none' and kind ~= 'hash' then return -1 end
local publication = {}
local conflict = false
for i = 1, #ARGV, 5 do
    local pair = redis.call('HMGET', KEYS[1], ARGV[i], ARGV[i+1])
    if (pair[1] and not pair[2]) or (pair[2] and not pair[1]) then return -1 end
    if pair[1] then
        if #pair[1] == 0 or #pair[1] > 128 or string.find(pair[1], '[^A-Za-z0-9_-]')
            or #pair[2] > 67108864 then return -1 end
    end
    if (pair[1] or '') ~= ARGV[i+2] then conflict = true end
    publication[#publication+1] = ARGV[i]
    publication[#publication+1] = ARGV[i+3]
    publication[#publication+1] = ARGV[i+1]
    publication[#publication+1] = ARGV[i+4]
end
if conflict then return 0 end
-- No mutations precede this single command: script runtime failures therefore
-- cannot expose a partially published checkpoint batch.
redis.call('HSET', KEYS[1], unpack(publication))
return 1
"#;

const RATE: &str = r#"
local kind = redis.call('TYPE', KEYS[1]).ok
if kind ~= 'none' and kind ~= 'string' then return -1 end
local current = redis.call('GET', KEYS[1])
if current and (not string.match(current, '^%d+$') or #current > 10) then return -1 end
local count = tonumber(current or '0')
if not count or count < 0 or count > 4294967295 then return -1 end
if count >= tonumber(ARGV[1]) then return 0 end
-- One SET includes expiry; a failure cannot strand a counter without a TTL.
redis.call('SET', KEYS[1], tostring(count + 1), 'EX', 180)
return 1
"#;

#[async_trait]
impl BrainStore for RedisStore {
    async fn load(&self, key: &str) -> StoreResult<Option<StoredValue>> {
        validate_key(key)?;
        let mut connection = self.connection.clone();
        let (status, revision, data): (i64, Option<Vec<u8>>, Option<Vec<u8>>) =
            ::redis::Script::new(LOAD)
                .key(&self.root)
                .arg(revision_field(key))
                .arg(data_field(key))
                .invoke_async(&mut connection)
                .await
                .map_err(|_| unavailable())?;
        match (status, revision, data) {
            (0, None, None) => Ok(None),
            (1, Some(revision), Some(data)) => {
                let revision = String::from_utf8(revision).map_err(|_| corrupt())?;
                if revision.is_empty()
                    || revision.len() > 128
                    || !revision
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
                    || data.len() > MAX_STATE_BYTES
                {
                    return Err(corrupt());
                }
                Ok(Some(StoredValue { revision, data }))
            }
            _ => Err(corrupt()),
        }
    }

    async fn commit(&self, writes: &[StoreWrite]) -> StoreResult<CommitOutcome> {
        validate_writes(writes)?;
        if writes.is_empty() {
            return Ok(CommitOutcome::Committed);
        }
        let script = ::redis::Script::new(COMMIT);
        let mut invocation = script.prepare_invoke();
        invocation.key(&self.root);
        for write in writes {
            invocation
                .arg(revision_field(&write.key))
                .arg(data_field(&write.key))
                .arg(write.expected.as_deref().unwrap_or(""))
                .arg(&write.revision)
                .arg(&write.data);
        }
        let mut connection = self.connection.clone();
        let status: i64 = invocation
            .invoke_async(&mut connection)
            .await
            .map_err(|_| unavailable())?;
        match status {
            1 => Ok(CommitOutcome::Committed),
            0 => Ok(CommitOutcome::Conflict),
            _ => Err(corrupt()),
        }
    }

    async fn allow_rate(&self, key: &str, window: u64, limit: u32) -> StoreResult<bool> {
        validate_key(key)?;
        if limit == 0 {
            return Ok(true);
        }
        i64::try_from(window)
            .map_err(|_| StoreError::Invalid("rate window out of range".into()))?;
        let bucket = format!("{}{window}:{key}", self.rate_prefix);
        let mut connection = self.connection.clone();
        let status: i64 = ::redis::Script::new(RATE)
            .key(bucket)
            .arg(limit)
            .invoke_async(&mut connection)
            .await
            .map_err(|_| unavailable())?;
        match status {
            1 => Ok(true),
            0 => Ok(false),
            _ => Err(corrupt()),
        }
    }
}
