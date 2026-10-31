//! DynamoDB checkpoints: immutable chunks followed by atomic head publication.
//!
//! Callers allocate unique revisions. Repeating the same commit is safe, including
//! concurrent retries. Chunks include the payload hash, so even accidental reuse
//! of a revision cannot overwrite another payload. Cleanup is conditional and
//! best effort. Crashes or unavailable cleanup leave orphans; operators must only
//! sweep them while writers are quiescent (never apply TTL to checkpoint chunks).
use super::*;
use aws_sdk_dynamodb::{
    Client,
    primitives::Blob,
    types::{AttributeValue, ConditionCheck, Delete, Put, TransactWriteItem},
};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

const CHUNK_BYTES: usize = 300_000;
const LOAD_ATTEMPTS: usize = 3;

fn unavailable() -> StoreError {
    StoreError::Unavailable("dynamodb")
}
fn corrupt() -> StoreError {
    StoreError::Corrupt("dynamodb")
}
fn digest(data: &[u8]) -> String {
    format!("{:x}", Sha256::digest(data))
}

#[derive(Clone)]
pub struct DynamoDbStore {
    client: Client,
    table: String,
    namespace: String,
}
impl fmt::Debug for DynamoDbStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DynamoDbStore").finish_non_exhaustive()
    }
}

fn validate_table(table: &str) -> StoreResult<()> {
    if !(3..=255).contains(&table.len())
        || !table
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    {
        return Err(StoreError::Invalid(
            "DynamoDB table must be a 3-255 character table name".into(),
        ));
    }
    Ok(())
}

impl DynamoDbStore {
    pub fn new(client: Client, table: String, namespace: String) -> StoreResult<Self> {
        validate_namespace(&namespace)?;
        validate_table(&table)?;
        Ok(Self {
            client,
            table,
            namespace,
        })
    }

    pub async fn connect(
        table: &str,
        namespace: &str,
        region: Option<&str>,
        endpoint: Option<&str>,
    ) -> StoreResult<Self> {
        validate_namespace(namespace)?;
        validate_table(table)?;
        let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest());
        if let Some(region) = region {
            loader = loader.region(aws_sdk_dynamodb::config::Region::new(region.to_owned()));
        }
        let config = loader.load().await;
        let mut builder = aws_sdk_dynamodb::config::Builder::from(&config)
            .retry_config(
                aws_sdk_dynamodb::config::retry::RetryConfig::standard().with_max_attempts(3),
            )
            .timeout_config(
                aws_sdk_dynamodb::config::timeout::TimeoutConfig::builder()
                    .operation_timeout(std::time::Duration::from_secs(20))
                    .operation_attempt_timeout(std::time::Duration::from_secs(6))
                    .build(),
            );
        if let Some(endpoint) = endpoint {
            builder = builder.endpoint_url(endpoint);
        }
        let store = Self::new(
            Client::from_conf(builder.build()),
            table.into(),
            namespace.into(),
        )?;
        // Probing with the same data-plane permission as reads avoids requiring
        // DescribeTable and fails startup if the table/schema is unavailable.
        store.read_head("__connection_probe__").await?;
        Ok(store)
    }

    fn partition(&self, key: &str) -> String {
        format!("state#{}#{key}", self.namespace)
    }
    fn item_key(&self, key: &str, sort: &str) -> HashMap<String, AttributeValue> {
        HashMap::from([
            ("pk".into(), AttributeValue::S(self.partition(key))),
            ("sk".into(), AttributeValue::S(sort.into())),
        ])
    }

    async fn read_head(&self, key: &str) -> StoreResult<Option<Head>> {
        let response = self
            .client
            .get_item()
            .table_name(&self.table)
            .set_key(Some(self.item_key(key, "head")))
            .consistent_read(true)
            .send()
            .await
            .map_err(|_| unavailable())?;
        response.item.as_ref().map(Head::decode).transpose()
    }

    async fn read_chunks(&self, key: &str, head: &Head) -> StoreResult<Vec<u8>> {
        let mut data = Vec::with_capacity(head.size);
        // Sequential requests deliberately bound outstanding chunk memory and
        // provider load to one request per operation, even for 64 MiB states.
        for index in 0..head.count {
            let response = self
                .client
                .get_item()
                .table_name(&self.table)
                .set_key(Some(self.item_key(key, &head.chunk_key(index))))
                .consistent_read(true)
                .send()
                .await
                .map_err(|_| unavailable())?;
            let chunk = response
                .item
                .as_ref()
                .and_then(|item| item.get("payload"))
                .and_then(|value| value.as_b().ok())
                .ok_or_else(corrupt)?;
            let expected = (head.size - index * CHUNK_BYTES).min(CHUNK_BYTES);
            if chunk.as_ref().len() != expected {
                return Err(corrupt());
            }
            data.extend_from_slice(chunk.as_ref());
        }
        if data.len() != head.size || digest(&data) != head.hash {
            return Err(corrupt());
        }
        Ok(data)
    }

    async fn stage(&self, write: &StoreWrite, head: &Head) -> StoreResult<()> {
        for (index, chunk) in write.data.chunks(CHUNK_BYTES).enumerate() {
            let payload = AttributeValue::B(Blob::new(chunk));
            let mut item = self.item_key(&write.key, &head.chunk_key(index));
            item.insert("payload".into(), payload.clone());
            let result = self
                .client
                .put_item()
                .table_name(&self.table)
                .set_item(Some(item))
                .condition_expression("attribute_not_exists(pk) OR #payload = :payload")
                .expression_attribute_names("#payload", "payload")
                .expression_attribute_values(":payload", payload)
                .send()
                .await;
            if let Err(error) = result {
                return Err(
                    if error
                        .as_service_error()
                        .is_some_and(|e| e.is_conditional_check_failed_exception())
                    {
                        corrupt()
                    } else {
                        unavailable()
                    },
                );
            }
        }
        Ok(())
    }

    // The condition and deletion are one transaction, so a head cannot start
    // referencing this chunk between a read and deletion. For failed attempts,
    // also prove the original CAS condition is no longer satisfiable. A current
    // matching/absent head is insufficient proof: another identical attempt may
    // still be staging. Unique revisions/no ABA are part of the writer contract.
    async fn cleanup(&self, key: &str, head: &Head, failed_expected: Option<Option<&str>>) {
        for index in 0..head.count {
            let mut condition =
                "attribute_exists(#revision) AND (#revision <> :revision OR #hash <> :hash)"
                    .to_owned();
            let mut check = ConditionCheck::builder()
                .table_name(&self.table)
                .set_key(Some(self.item_key(key, "head")))
                .expression_attribute_names("#revision", "revision")
                .expression_attribute_names("#hash", "sha256")
                .expression_attribute_values(":revision", AttributeValue::S(head.revision.clone()))
                .expression_attribute_values(":hash", AttributeValue::S(head.hash.clone()));
            if let Some(Some(expected)) = failed_expected {
                condition.push_str(" AND #revision <> :expected");
                check = check
                    .expression_attribute_values(":expected", AttributeValue::S(expected.into()));
            }
            let Ok(check) = check.condition_expression(condition).build() else {
                return;
            };
            let Ok(delete) = Delete::builder()
                .table_name(&self.table)
                .set_key(Some(self.item_key(key, &head.chunk_key(index))))
                .build()
            else {
                return;
            };
            if self
                .client
                .transact_write_items()
                .transact_items(TransactWriteItem::builder().condition_check(check).build())
                .transact_items(TransactWriteItem::builder().delete(delete).build())
                .send()
                .await
                .is_err()
            {
                return;
            }
        }
    }

    async fn cleanup_failed(&self, writes: &[StoreWrite], heads: &[Head]) {
        for (write, head) in writes.iter().zip(heads) {
            self.cleanup(&write.key, head, Some(write.expected.as_deref()))
                .await;
        }
    }
}

#[async_trait]
impl BrainStore for DynamoDbStore {
    async fn load(&self, key: &str) -> StoreResult<Option<StoredValue>> {
        validate_key(key)?;
        for _ in 0..LOAD_ATTEMPTS {
            let Some(head) = self.read_head(key).await? else {
                return Ok(None);
            };
            match self.read_chunks(key, &head).await {
                Ok(data) => {
                    return Ok(Some(StoredValue {
                        revision: head.revision,
                        data,
                    }));
                }
                Err(StoreError::Corrupt(_)) => {
                    if self.read_head(key).await?.as_ref() == Some(&head) {
                        return Err(corrupt());
                    }
                }
                Err(error) => return Err(error),
            }
        }
        Err(unavailable())
    }

    async fn commit(&self, writes: &[StoreWrite]) -> StoreResult<CommitOutcome> {
        validate_writes(writes)?;
        if writes.is_empty() {
            return Ok(CommitOutcome::Committed);
        }
        // Capture old manifests for post-publication cleanup. The transaction
        // remains authoritative; this preflight merely avoids needless staging.
        let mut old_heads = Vec::with_capacity(writes.len());
        for write in writes {
            let old = self.read_head(&write.key).await?;
            if old.as_ref().map(|h| h.revision.as_str()) != write.expected.as_deref() {
                return Ok(CommitOutcome::Conflict);
            }
            old_heads.push(old);
        }
        let heads: Vec<_> = writes.iter().map(Head::for_write).collect();
        for (index, (write, head)) in writes.iter().zip(&heads).enumerate() {
            if let Err(error) = self.stage(write, head).await {
                self.cleanup_failed(&writes[..=index], &heads[..=index])
                    .await;
                return Err(error);
            }
        }
        let mut items = Vec::with_capacity(writes.len());
        for (write, head) in writes.iter().zip(&heads) {
            let mut item = self.item_key(&write.key, "head");
            item.extend(head.attributes());
            let mut put = Put::builder().table_name(&self.table).set_item(Some(item));
            put = match &write.expected {
                Some(expected) => put
                    .condition_expression("#revision = :expected")
                    .expression_attribute_names("#revision", "revision")
                    .expression_attribute_values(":expected", AttributeValue::S(expected.clone())),
                None => put.condition_expression("attribute_not_exists(pk)"),
            };
            items.push(
                TransactWriteItem::builder()
                    .put(put.build().map_err(|_| unavailable())?)
                    .build(),
            );
        }
        // SDK 1.104's operation interceptor supplies a stable client request
        // token for retries of this send, including a lost successful response.
        let result = self
            .client
            .transact_write_items()
            .set_transact_items(Some(items))
            .send()
            .await;
        if let Err(error) = result {
            let conflict = match error.as_service_error() {
                Some(aws_sdk_dynamodb::operation::transact_write_items::TransactWriteItemsError::TransactionCanceledException(error)) => {
                    let reasons = error.cancellation_reasons();
                    reasons.len() == writes.len()
                        && reasons.iter().any(|r| r.code() == Some("ConditionalCheckFailed"))
                        && reasons.iter().all(|r| matches!(r.code(), Some("None" | "ConditionalCheckFailed")))
                }
                _ => false,
            };
            self.cleanup_failed(writes, &heads).await;
            return if conflict {
                Ok(CommitOutcome::Conflict)
            } else {
                Err(unavailable())
            };
        }
        for ((write, head), old) in writes.iter().zip(&heads).zip(old_heads) {
            if let Some(old) = old.filter(|old| old != head) {
                self.cleanup(&write.key, &old, None).await;
            }
        }
        Ok(CommitOutcome::Committed)
    }

    async fn allow_rate(&self, key: &str, window: u64, limit: u32) -> StoreResult<bool> {
        validate_key(key)?;
        if limit == 0 {
            return Ok(true);
        }
        let expires = window
            .checked_add(2)
            .and_then(|w| w.checked_mul(60))
            .filter(|expires| *expires <= i64::MAX as u64)
            .ok_or_else(|| StoreError::Invalid("rate window out of range".into()))?;
        let result = self
            .client
            .update_item()
            .table_name(&self.table)
            .key(
                "pk",
                AttributeValue::S(format!("rate#{}#{key}", self.namespace)),
            )
            .key("sk", AttributeValue::S(window.to_string()))
            .update_expression("SET expires_at = :expires ADD #count :one")
            .condition_expression(
                "attribute_not_exists(#count) OR (#count >= :zero AND #count < :limit)",
            )
            .expression_attribute_names("#count", "count")
            .expression_attribute_values(":expires", AttributeValue::N(expires.to_string()))
            .expression_attribute_values(":one", AttributeValue::N("1".into()))
            .expression_attribute_values(":zero", AttributeValue::N("0".into()))
            .expression_attribute_values(":limit", AttributeValue::N(limit.to_string()))
            .send()
            .await;
        match result {
            Ok(_) => Ok(true),
            Err(error)
                if error
                    .as_service_error()
                    .is_some_and(|e| e.is_conditional_check_failed_exception()) =>
            {
                Ok(false)
            }
            Err(_) => Err(unavailable()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Head {
    revision: String,
    count: usize,
    size: usize,
    hash: String,
}

impl Head {
    fn for_write(write: &StoreWrite) -> Self {
        Self {
            revision: write.revision.clone(),
            count: write.data.len().div_ceil(CHUNK_BYTES),
            size: write.data.len(),
            hash: digest(&write.data),
        }
    }

    fn decode(item: &HashMap<String, AttributeValue>) -> StoreResult<Self> {
        let number = |name: &str| -> StoreResult<usize> {
            item.get(name)
                .and_then(|v| v.as_n().ok())
                .and_then(|v| v.parse().ok())
                .ok_or_else(corrupt)
        };
        let string = |name: &str| -> StoreResult<String> {
            item.get(name)
                .and_then(|v| v.as_s().ok())
                .cloned()
                .ok_or_else(corrupt)
        };
        let head = Self {
            revision: string("revision")?,
            count: number("chunks")?,
            size: number("size")?,
            hash: string("sha256")?,
        };
        if number("format")? != 1
            || head.size > MAX_STATE_BYTES
            || head.count != head.size.div_ceil(CHUNK_BYTES)
            || head.revision.is_empty()
            || head.revision.len() > 128
            || !head
                .revision
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
            || head.hash.len() != 64
            || !head
                .hash
                .bytes()
                .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
        {
            return Err(corrupt());
        }
        Ok(head)
    }

    fn chunk_key(&self, index: usize) -> String {
        format!("chunk#{}#{}#{index}", self.revision, self.hash)
    }

    fn attributes(&self) -> HashMap<String, AttributeValue> {
        HashMap::from([
            ("format".into(), AttributeValue::N("1".into())),
            ("revision".into(), AttributeValue::S(self.revision.clone())),
            ("chunks".into(), AttributeValue::N(self.count.to_string())),
            ("size".into(), AttributeValue::N(self.size.to_string())),
            ("sha256".into(), AttributeValue::S(self.hash.clone())),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local_client(endpoint: &str) -> aws_sdk_dynamodb::Client {
        // Never use aws_config::load_defaults or a credential provider chain in tests.
        let config = aws_sdk_dynamodb::Config::builder()
            .behavior_version_latest()
            .region(aws_sdk_dynamodb::config::Region::new("us-east-1"))
            .credentials_provider(aws_sdk_dynamodb::config::Credentials::new(
                "dummy",
                "dummy",
                None,
                None,
                "local-test",
            ))
            .endpoint_url(endpoint)
            .retry_config(
                aws_sdk_dynamodb::config::retry::RetryConfig::standard().with_max_attempts(1),
            )
            .build();
        aws_sdk_dynamodb::Client::from_conf(config)
    }

    // A scripted local HTTP peer exercises the real SDK serialization/error path.
    // Dropping it always aborts the listener, including when an assertion panics.
    type PendingResponses = std::collections::VecDeque<(&'static str, u16, serde_json::Value)>;

    struct Peer {
        endpoint: String,
        pending: std::sync::Arc<std::sync::Mutex<PendingResponses>>,
        task: tokio::task::JoinHandle<()>,
    }
    impl Drop for Peer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }
    impl Peer {
        async fn start(responses: Vec<(&'static str, u16, serde_json::Value)>) -> Self {
            use axum::{
                Json, Router,
                body::Bytes,
                http::{HeaderMap, StatusCode},
                routing::post,
            };
            let pending = std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::VecDeque::from(responses),
            ));
            let state = pending.clone();
            let app = Router::new().route(
                "/",
                post(move |headers: HeaderMap, body: Bytes| {
                    let state = state.clone();
                    async move {
                        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
                        let (action, status, response) = state
                            .lock()
                            .unwrap()
                            .pop_front()
                            .expect("unexpected SDK request");
                        assert_eq!(
                            headers.get("x-amz-target").unwrap().to_str().unwrap(),
                            format!("DynamoDB_20120810.{action}")
                        );
                        if action == "GetItem" {
                            assert_eq!(body["ConsistentRead"], true);
                        }
                    if action == "TransactWriteItems" && !body["TransactItems"][0]["Put"].is_null() {
                        assert!(body["ClientRequestToken"].as_str().is_some_and(|token| !token.is_empty()), "head publication needs an idempotency token for ambiguous SDK retries");
                    }
                        (StatusCode::from_u16(status).unwrap(), Json(response))
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let task = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            Self {
                endpoint,
                pending,
                task,
            }
        }
        fn store(&self) -> DynamoDbStore {
            DynamoDbStore::new(
                local_client(&self.endpoint),
                "test-table".into(),
                "test".into(),
            )
            .unwrap()
        }
        fn done(&self) {
            assert!(
                self.pending.lock().unwrap().is_empty(),
                "unconsumed SDK responses"
            );
        }
    }

    #[tokio::test]
    async fn cleanup_racing_load_restarts_from_the_new_head() {
        use serde_json::json;
        let head = |revision: &str, hash: &str| json!({"Item": {"format":{"N":"1"},"revision":{"S":revision},"chunks":{"N":"1"},"size":{"N":"3"},"sha256":{"S":hash}}});
        let peer = Peer::start(vec![
            (
                "GetItem",
                200,
                head(
                    "old",
                    "0000000000000000000000000000000000000000000000000000000000000000",
                ),
            ),
            ("GetItem", 200, json!({})),
            (
                "GetItem",
                200,
                head(
                    "new",
                    "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
                ),
            ),
            (
                "GetItem",
                200,
                head(
                    "new",
                    "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
                ),
            ),
            ("GetItem", 200, json!({"Item":{"payload":{"B":"YWJj"}}})),
        ])
        .await;
        assert_eq!(
            peer.store().load("repo").await.unwrap(),
            Some(StoredValue {
                revision: "new".into(),
                data: b"abc".to_vec()
            })
        );
        peer.done();
    }

    #[tokio::test]
    async fn stable_missing_chunk_is_corrupt_and_provider_failure_is_unavailable() {
        use serde_json::json;
        let head = json!({"Item": {"format":{"N":"1"},"revision":{"S":"r1"},"chunks":{"N":"1"},"size":{"N":"3"},"sha256":{"S":"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"}}});
        let peer = Peer::start(vec![
            ("GetItem", 200, head.clone()),
            ("GetItem", 200, json!({})),
            ("GetItem", 200, head),
        ])
        .await;
        assert_eq!(peer.store().load("repo").await, Err(corrupt()));
        peer.done();
        let peer = Peer::start(vec![(
            "GetItem",
            500,
            json!({"__type":"InternalServerError", "message":"secret-provider-url"}),
        )])
        .await;
        let error = peer.store().load("repo").await.unwrap_err();
        assert_eq!(error, unavailable());
        assert!(!format!("{error:?}").contains("secret"));
        peer.done();
    }

    #[tokio::test]
    async fn changing_heads_bound_reader_retries() {
        use serde_json::json;
        let head = |revision: &str| json!({"Item": {"format":{"N":"1"},"revision":{"S":revision},"chunks":{"N":"1"},"size":{"N":"3"},"sha256":{"S":"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"}}});
        let peer = Peer::start(vec![
            ("GetItem", 200, head("one")),
            ("GetItem", 200, json!({})),
            ("GetItem", 200, head("two")),
            ("GetItem", 200, head("two")),
            ("GetItem", 200, json!({})),
            ("GetItem", 200, head("three")),
            ("GetItem", 200, head("three")),
            ("GetItem", 200, json!({})),
            ("GetItem", 200, head("four")),
        ])
        .await;
        assert_eq!(peer.store().load("repo").await, Err(unavailable()));
        peer.done();
    }

    #[tokio::test]
    async fn invalid_chunk_length_and_hash_are_rejected() {
        use serde_json::json;
        let head = json!({"Item": {"format":{"N":"1"},"revision":{"S":"r1"},"chunks":{"N":"1"},"size":{"N":"3"},"sha256":{"S":"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"}}});
        // "a" has wrong length; "abd" has correct length and wrong digest.
        for payload in ["YQ==", "YWJk"] {
            let peer = Peer::start(vec![
                ("GetItem", 200, head.clone()),
                ("GetItem", 200, json!({"Item":{"payload":{"B":payload}}})),
                ("GetItem", 200, head.clone()),
            ])
            .await;
            assert_eq!(peer.store().load("repo").await, Err(corrupt()));
            peer.done();
        }
    }

    #[tokio::test]
    async fn existing_inconsistent_chunk_is_corrupt_not_a_cas_conflict() {
        use serde_json::json;
        let peer = Peer::start(vec![
            ("GetItem", 200, json!({})),
            ("PutItem", 400, json!({"__type":"ConditionalCheckFailedException"})),
            ("TransactWriteItems", 400, json!({"__type":"TransactionCanceledException","CancellationReasons":[{"Code":"ConditionalCheckFailed"},{"Code":"None"}]})),
        ]).await;
        assert_eq!(peer.store().commit(&[write(vec![1])]).await, Err(corrupt()));
        peer.done();
    }

    #[tokio::test]
    async fn partial_staging_failure_never_publishes_a_head() {
        use serde_json::json;
        let peer = Peer::start(vec![
            ("GetItem", 200, json!({})),
            ("PutItem", 200, json!({})),
            ("PutItem", 500, json!({"__type":"InternalServerError"})),
            // Cleanup cannot prove there is no identical attempt still staging.
            ("TransactWriteItems", 400, json!({"__type":"TransactionCanceledException","CancellationReasons":[{"Code":"ConditionalCheckFailed"},{"Code":"None"}]})),
        ]).await;
        assert_eq!(
            peer.store().commit(&[write(vec![0; 300_001])]).await,
            Err(unavailable())
        );
        peer.done();
    }

    #[tokio::test]
    async fn transaction_capacity_failures_are_not_reported_as_cas_conflicts() {
        use serde_json::json;
        for (code, expected) in [
            ("ConditionalCheckFailed", Ok(CommitOutcome::Conflict)),
            ("ProvisionedThroughputExceeded", Err(unavailable())),
            ("ValidationError", Err(unavailable())),
            ("TransactionConflict", Err(unavailable())),
        ] {
            let peer = Peer::start(vec![
                ("GetItem", 200, json!({})),
                ("TransactWriteItems", 400, json!({"__type":"TransactionCanceledException", "CancellationReasons":[{"Code":code}]})),
            ]).await;
            assert_eq!(peer.store().commit(&[write(vec![])]).await, expected);
            peer.done();
        }
    }

    #[tokio::test]
    async fn rate_exhaustion_is_distinct_from_provider_failure_and_overflow() {
        use serde_json::json;
        let peer = Peer::start(vec![
            ("UpdateItem", 200, json!({})),
            (
                "UpdateItem",
                400,
                json!({"__type":"ConditionalCheckFailedException"}),
            ),
            ("UpdateItem", 500, json!({"__type":"InternalServerError"})),
        ])
        .await;
        let store = peer.store();
        assert_eq!(store.allow_rate("token", 1, 1).await, Ok(true));
        assert_eq!(store.allow_rate("token", 1, 1).await, Ok(false));
        assert_eq!(store.allow_rate("token", 2, 1).await, Err(unavailable()));
        assert!(matches!(
            store.allow_rate("token", u64::MAX, 1).await,
            Err(StoreError::Invalid(_))
        ));
        assert_eq!(store.allow_rate("token", u64::MAX, 0).await, Ok(true));
        assert!(store.allow_rate("", 1, 0).await.is_err());
        peer.done();
    }

    #[tokio::test]
    async fn validation_rejects_bad_writes_before_any_network_request() {
        let peer = Peer::start(vec![]).await;
        let store = peer.store();
        let mut invalid = write(vec![1]);
        invalid.expected = Some("invalid revision".into());
        assert!(matches!(
            store.commit(&[invalid]).await,
            Err(StoreError::Invalid(_))
        ));
        assert!(matches!(
            store.commit(&[write(vec![]), write(vec![])]).await,
            Err(StoreError::Invalid(_))
        ));
        assert!(matches!(
            store.commit(&[write(vec![0; MAX_STATE_BYTES + 1])]).await,
            Err(StoreError::Invalid(_))
        ));
        assert!(matches!(store.load("").await, Err(StoreError::Invalid(_))));
        assert_eq!(store.commit(&[]).await, Ok(CommitOutcome::Committed));
        assert!(
            DynamoDbStore::new(
                local_client(&peer.endpoint),
                "https://secret@example.com".into(),
                "test".into()
            )
            .is_err()
        );
        assert!(
            DynamoDbStore::new(
                local_client(&peer.endpoint),
                "valid-table".into(),
                "bad#namespace".into()
            )
            .is_err()
        );
        assert!(!format!("{store:?}").contains(&peer.endpoint));
        peer.done();
    }

    fn local_endpoint(raw: &str) -> Result<&str, &'static str> {
        let address: std::net::SocketAddr = raw
            .strip_prefix("http://")
            .ok_or("test endpoint must be explicit HTTP loopback")?
            .parse()
            .map_err(|_| "test endpoint must use a numeric loopback address and port")?;
        if !address.ip().is_loopback() || address.port() == 0 {
            return Err("test endpoint must be loopback");
        }
        Ok(raw)
    }

    #[test]
    fn emulator_endpoint_cannot_target_live_aws_or_use_embedded_credentials() {
        for endpoint in [
            "https://dynamodb.us-east-1.amazonaws.com",
            "http://192.0.2.1:8000",
            "http://127.0.0.1:8000@example.com",
            "http://localhost.attacker.test:8000",
            "http://127.0.0.1:0",
            "http://127.0.0.1:8000/path",
        ] {
            assert!(local_endpoint(endpoint).is_err());
        }
        assert!(local_endpoint("http://127.0.0.1:8000").is_ok());
        assert!(local_endpoint("http://[::1]:8000").is_ok());
    }

    #[tokio::test]
    #[ignore = "requires KIMETSU_TEST_DYNAMODB_ENDPOINT=http://127.0.0.1:PORT and DynamoDB Local"]
    async fn local_emulator_contract() {
        use aws_sdk_dynamodb::types::{
            AttributeDefinition, BillingMode, KeySchemaElement, KeyType, ScalarAttributeType,
        };
        use std::sync::Arc;
        let raw = std::env::var("KIMETSU_TEST_DYNAMODB_ENDPOINT")
            .expect("explicit local endpoint required");
        let endpoint = local_endpoint(&raw).expect("refusing non-loopback endpoint");
        let client = local_client(endpoint);
        let table = format!("kimetsu-test-{}", ulid::Ulid::new());
        client
            .create_table()
            .table_name(&table)
            .billing_mode(BillingMode::PayPerRequest)
            .attribute_definitions(
                AttributeDefinition::builder()
                    .attribute_name("pk")
                    .attribute_type(ScalarAttributeType::S)
                    .build()
                    .unwrap(),
            )
            .attribute_definitions(
                AttributeDefinition::builder()
                    .attribute_name("sk")
                    .attribute_type(ScalarAttributeType::S)
                    .build()
                    .unwrap(),
            )
            .key_schema(
                KeySchemaElement::builder()
                    .attribute_name("pk")
                    .key_type(KeyType::Hash)
                    .build()
                    .unwrap(),
            )
            .key_schema(
                KeySchemaElement::builder()
                    .attribute_name("sk")
                    .key_type(KeyType::Range)
                    .build()
                    .unwrap(),
            )
            .send()
            .await
            .expect("create isolated local table");
        let namespace = ulid::Ulid::new().to_string();
        let first = DynamoDbStore::new(client.clone(), table.clone(), namespace.clone()).unwrap();
        let second = DynamoDbStore::new(local_client(endpoint), table.clone(), namespace).unwrap();
        let other = DynamoDbStore::new(
            local_client(endpoint),
            table.clone(),
            ulid::Ulid::new().to_string(),
        )
        .unwrap();
        assert_eq!(first.load("repo").await.unwrap(), None);
        let initial = write(vec![0, 255, 17]);
        assert_eq!(
            first.commit(std::slice::from_ref(&initial)).await.unwrap(),
            CommitOutcome::Committed
        );
        assert_eq!(
            second.load("repo").await.unwrap().unwrap().data,
            initial.data
        );
        assert_eq!(other.load("repo").await.unwrap(), None);
        assert_eq!(
            second.commit(&[initial]).await.unwrap(),
            CommitOutcome::Conflict
        );

        let update =
            |key: &str, expected: Option<&str>, revision: &str, data: Vec<u8>| StoreWrite {
                key: key.into(),
                expected: expected.map(str::to_owned),
                revision: revision.into(),
                data,
            };
        let rollback = [
            update("new", None, "r2", b"new".to_vec()),
            update("repo", Some("stale"), "r2", vec![]),
        ];
        assert_eq!(
            second.commit(&rollback).await.unwrap(),
            CommitOutcome::Conflict
        );
        assert_eq!(first.load("new").await.unwrap(), None);

        let a = [
            update("repo", Some("r1"), "a", b"a".to_vec()),
            update("org", None, "a", b"a".to_vec()),
        ];
        let b = [
            update("org", None, "b", b"b".to_vec()),
            update("repo", Some("r1"), "b", b"b".to_vec()),
        ];
        let (a, b) = tokio::join!(first.commit(&a), second.commit(&b));
        assert!(matches!(
            (a.unwrap(), b.unwrap()),
            (CommitOutcome::Committed, CommitOutcome::Conflict)
                | (CommitOutcome::Conflict, CommitOutcome::Committed)
        ));
        assert_eq!(
            first.load("repo").await.unwrap().unwrap().revision,
            second.load("org").await.unwrap().unwrap().revision
        );

        // Exercise the full four-head transaction and cross-item binary payloads.
        let batch: Vec<_> = (0..4)
            .map(|n| update(&format!("large-{n}"), None, "large", vec![n; 600_001]))
            .collect();
        assert_eq!(
            first.commit(&batch).await.unwrap(),
            CommitOutcome::Committed
        );
        for write in &batch {
            assert_eq!(
                second.load(&write.key).await.unwrap().unwrap().data,
                write.data
            );
        }

        // Concurrent identical attempts share immutable chunks. The loser's
        // cleanup must retain chunks now referenced by the winner.
        let repeated = [update("repeat", None, "same", vec![8; 300_001])];
        let (a, b) = tokio::join!(first.commit(&repeated), second.commit(&repeated));
        assert!(matches!(
            (a.unwrap(), b.unwrap()),
            (CommitOutcome::Committed, CommitOutcome::Conflict)
                | (CommitOutcome::Conflict, CommitOutcome::Committed)
        ));
        assert_eq!(
            first.load("repeat").await.unwrap().unwrap().data,
            repeated[0].data
        );

        // Failed staging cleanup cannot delete chunks that an identical attempt
        // could still publish while the expected head remains unchanged.
        let pending = update("pending", None, "pending", vec![9; 300_001]);
        let pending_head = Head::for_write(&pending);
        first.stage(&pending, &pending_head).await.unwrap();
        first.cleanup("pending", &pending_head, Some(None)).await;
        assert_eq!(
            first.read_chunks("pending", &pending_head).await.unwrap(),
            pending.data
        );
        assert_eq!(
            second.commit(std::slice::from_ref(&pending)).await.unwrap(),
            CommitOutcome::Committed
        );
        first.cleanup("pending", &pending_head, Some(None)).await;
        assert_eq!(
            first.load("pending").await.unwrap().unwrap().data,
            pending.data
        );

        // Reusing a revision with different content cannot replace live chunks.
        let different = update("pending", None, "pending", vec![1; 300_001]);
        let different_head = Head::for_write(&different);
        first.stage(&different, &different_head).await.unwrap();
        first.cleanup("pending", &different_head, Some(None)).await;
        assert!(first.read_chunks("pending", &different_head).await.is_err());
        assert_eq!(
            first.load("pending").await.unwrap().unwrap().data,
            pending.data
        );

        // Old chunks are reclaimed after replacement; empty states remain valid.
        let replacement = update("pending", Some("pending"), "replacement", vec![]);
        assert_eq!(
            second.commit(&[replacement]).await.unwrap(),
            CommitOutcome::Committed
        );
        assert!(first.read_chunks("pending", &pending_head).await.is_err());
        assert_eq!(
            first.load("pending").await.unwrap().unwrap().data,
            Vec::<u8>::new()
        );

        let mut requests = tokio::task::JoinSet::new();
        for index in 0..20 {
            let store = if index % 2 == 0 {
                first.clone()
            } else {
                second.clone()
            };
            requests.spawn(async move { store.allow_rate("token", 100, 7).await.unwrap() });
        }
        let mut allowed = 0;
        while let Some(result) = requests.join_next().await {
            allowed += usize::from(result.unwrap());
        }
        assert_eq!(allowed, 7);
        assert!(first.allow_rate("token", 101, 7).await.unwrap());
        assert!(other.allow_rate("token", 100, 7).await.unwrap());
        assert!(first.allow_rate("different-token", 100, 7).await.unwrap());
        assert!(first.allow_rate("token", 100, 0).await.unwrap());

        // Exercise real portable brain records, not only opaque byte fixtures.
        let state = crate::durable::DurableState::new(
            Arc::new(
                DynamoDbStore::new(client.clone(), table.clone(), "projection-contract".into())
                    .unwrap(),
            ),
            true,
        );
        state
            .execute("one", "local-test".into(), |root| {
                kimetsu_brain::project::add_memory(
                    root,
                    kimetsu_core::memory::MemoryScope::GlobalUser,
                    kimetsu_core::memory::MemoryKind::Convention,
                    "Local DynamoDB orchid gateway convention",
                )
                .map(|id| serde_json::json!({"id":id}))
                .map_err(|e| e.to_string())
            })
            .await
            .unwrap();
        let fresh = crate::durable::DurableState::new(
            Arc::new(
                DynamoDbStore::new(
                    local_client(endpoint),
                    table.clone(),
                    "projection-contract".into(),
                )
                .unwrap(),
            ),
            true,
        );
        let recalled = fresh
            .execute("two", "local-test".into(), |_| {
                let conn = kimetsu_brain::user_brain::open_user_brain_readonly()
                    .map_err(|e| e.to_string())?
                    .unwrap();
                let count: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM memories WHERE text LIKE '%orchid%'",
                        [],
                        |row| row.get(0),
                    )
                    .map_err(|e| e.to_string())?;
                Ok(serde_json::json!(count))
            })
            .await
            .unwrap();
        assert_eq!(recalled, 1);

        // Provider corruption is never silently decoded as an empty checkpoint.
        let head = first.read_head("large-0").await.unwrap().unwrap();
        client
            .put_item()
            .table_name(&table)
            .set_item(Some({
                let mut item = first.item_key("large-0", &head.chunk_key(0));
                item.insert(
                    "payload".into(),
                    AttributeValue::B(Blob::new(vec![1; 300_000])),
                );
                item
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(first.load("large-0").await, Err(corrupt()));
        client
            .delete_table()
            .table_name(&table)
            .send()
            .await
            .expect("delete isolated local table");
        assert_eq!(first.load("repo").await, Err(unavailable()));
    }

    fn write(data: Vec<u8>) -> StoreWrite {
        StoreWrite {
            key: "repo".into(),
            expected: None,
            revision: "r1".into(),
            data,
        }
    }

    #[test]
    fn chunk_manifest_covers_empty_boundary_and_partial_data() {
        for (size, count) in [
            (0, 0),
            (1, 1),
            (300_000, 1),
            (300_001, 2),
            (64 * 1024 * 1024, 224),
        ] {
            let head = Head::for_write(&write(vec![7; size]));
            assert_eq!(head.count, count);
            assert_eq!(head.size, size);
        }
        assert_eq!(
            Head::for_write(&write(vec![])).hash,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn repeated_revision_cannot_overwrite_different_payload_chunks() {
        let first = Head::for_write(&write(vec![1]));
        let second = Head::for_write(&write(vec![2]));
        assert_ne!(first.chunk_key(0), second.chunk_key(0));
        assert_ne!(first.chunk_key(0), first.chunk_key(1));
    }

    fn valid_head() -> HashMap<String, AttributeValue> {
        HashMap::from([
            ("format".into(), AttributeValue::N("1".into())),
            ("revision".into(), AttributeValue::S("r1".into())),
            ("chunks".into(), AttributeValue::N("1".into())),
            ("size".into(), AttributeValue::N("3".into())),
            (
                "sha256".into(),
                AttributeValue::S(
                    "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".into(),
                ),
            ),
        ])
    }

    #[test]
    fn malformed_manifest_is_never_loaded_as_empty_state() {
        let head = Head::decode(&valid_head()).unwrap();
        assert_eq!(head.size, 3);
        for (field, value) in [
            ("format", AttributeValue::N("2".into())),
            ("revision", AttributeValue::S("bad/revision".into())),
            ("chunks", AttributeValue::N("0".into())),
            ("chunks", AttributeValue::N("225".into())),
            ("size", AttributeValue::N("67108865".into())),
            ("size", AttributeValue::N("-1".into())),
            ("sha256", AttributeValue::S("invalid".into())),
        ] {
            let mut item = valid_head();
            item.insert(field.into(), value);
            assert!(matches!(
                Head::decode(&item),
                Err(StoreError::Corrupt("dynamodb"))
            ));
        }
        assert!(Head::decode(&HashMap::new()).is_err());
    }
}
