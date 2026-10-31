//! HTTP acceptance across independent instances with disposable query roots.
use async_trait::async_trait;
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use kimetsu_remote::{
    app::build_router,
    auth::AuthConfig,
    state::AppState,
    storage::{
        BrainStore, CommitOutcome, StoreError, StoreResult, StoreWrite, StoredValue,
        sqlite::SqliteStore,
    },
};
use serde_json::{Value, json};
use std::{path::Path, sync::Arc};
use tower::ServiceExt;

fn state(path: &Path, rate: u32, org: bool) -> AppState {
    let mut state = state_for(
        Arc::new(SqliteStore::open(path.to_path_buf(), "http-test").unwrap()),
        rate,
        org,
    );
    state.data_dir = Arc::new(path.with_extension("unused"));
    state
}

fn state_for(store: Arc<dyn BrainStore>, rate: u32, org: bool) -> AppState {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        // Test-binary policy, fixed before any request; never changed per request.
        unsafe {
            std::env::set_var("KIMETSU_MCP_ENABLE_WRITE_TOOLS", "1");
            std::env::set_var("KIMETSU_BRAIN_EMBEDDER", "noop");
        }
    });
    let auth = AuthConfig {
        global: vec!["admin-fixture".into()],
        per_repo: [("one".into(), vec!["repo-fixture".into()])].into(),
        ..Default::default()
    };
    let mut state =
        AppState::with_rate_limit(Default::default(), auth, rate).with_storage(store, org);
    Arc::get_mut(state.durable.as_mut().unwrap())
        .unwrap()
        .project_config
        .embedder
        .enabled = false;
    state
}

#[cfg(any(feature = "postgres", feature = "redis"))]
async fn live_roundtrip(first: Arc<dyn BrainStore>, fresh: Arc<dyn BrainStore>) {
    let (status, result) = send(
        state_for(first, 0, false),
        "one",
        "admin-fixture",
        call(
            "kimetsu_brain_record",
            json!({"lesson":"Live orchid gateway listens on port 7319"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(inner(&result)["ok"], true, "{result}");
    let (status, result) = send(
        state_for(fresh, 0, false),
        "one",
        "admin-fixture",
        call(
            "kimetsu_brain_context",
            json!({"query":"orchid gateway", "min_score":0.0}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert!(
        inner(&result)["capsules"].to_string().contains("7319"),
        "{result}"
    );
}

#[cfg(feature = "postgres")]
#[tokio::test]
#[ignore = "requires explicit KIMETSU_TEST_POSTGRES_URL dedicated test database"]
async fn postgres_http_fresh_instance_roundtrip() {
    use kimetsu_remote::storage::postgres::PostgresStore;
    let url = std::env::var("KIMETSU_TEST_POSTGRES_URL").expect("set dedicated test URL");
    let ns = format!("http_{}", ulid::Ulid::new());
    live_roundtrip(
        Arc::new(PostgresStore::connect(&url, &ns).await.unwrap()),
        Arc::new(PostgresStore::connect(&url, &ns).await.unwrap()),
    )
    .await;
}

#[cfg(feature = "redis")]
#[tokio::test]
#[ignore = "requires explicit KIMETSU_TEST_REDIS_URL dedicated test database"]
async fn redis_http_fresh_instance_roundtrip() {
    use kimetsu_remote::storage::redis::RedisStore;
    let url = std::env::var("KIMETSU_TEST_REDIS_URL").expect("set dedicated test URL");
    let ns = format!("http_{}", ulid::Ulid::new());
    live_roundtrip(
        Arc::new(RedisStore::connect(&url, &ns).await.unwrap()),
        Arc::new(RedisStore::connect(&url, &ns).await.unwrap()),
    )
    .await;
}

fn call(name: &str, arguments: Value) -> Value {
    json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":arguments}})
}

async fn send(state: AppState, repo: &str, token: &str, body: Value) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri(format!("/mcp/{repo}"))
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = build_router(state).oneshot(request).await.unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

fn inner(value: &Value) -> Value {
    serde_json::from_str(
        value["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("no tool response: {value}")),
    )
    .unwrap()
}

#[tokio::test]
async fn fresh_http_instances_recall_committed_records_and_isolate_repos() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("authority.db");
    let (status, recorded) = send(
        state(&path, 0, false),
        "one",
        "admin-fixture",
        call(
            "kimetsu_brain_record",
            json!({"lesson":"The quibblefrotz gateway listens on port 7319"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(inner(&recorded)["ok"], true, "{recorded}");
    // Constructing another state reopens the authority; no request root survives.
    let query = call(
        "kimetsu_brain_context",
        json!({"query":"quibblefrotz gateway", "min_score":0.0}),
    );
    let (status, recalled) = send(
        state(&path, 0, false),
        "one",
        "admin-fixture",
        query.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        inner(&recalled)["capsules"]
            .to_string()
            .contains("quibblefrotz"),
        "{recalled}"
    );
    let (_, other) = send(state(&path, 0, false), "two", "admin-fixture", query).await;
    assert!(
        !inner(&other)["capsules"]
            .to_string()
            .contains("quibblefrotz")
    );
    assert!(!path.with_extension("unused").exists());
}

#[tokio::test]
async fn independent_http_instances_share_the_durable_rate_limit() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("authority.db");
    let request = json!({"jsonrpc":"2.0","id":1,"method":"tools/list"});
    assert_eq!(
        send(
            state(&path, 1, false),
            "one",
            "admin-fixture",
            request.clone()
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        send(state(&path, 1, false), "two", "admin-fixture", request)
            .await
            .0,
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[tokio::test]
async fn repo_token_cannot_write_shared_org_or_access_another_repo() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("authority.db");
    let write = call(
        "kimetsu_brain_memory_add",
        json!({"text":"Orchid shared convention", "kind":"convention", "scope":"global_user"}),
    );
    assert_eq!(
        send(state(&path, 0, true), "one", "repo-fixture", write)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(
            state(&path, 0, true),
            "two",
            "repo-fixture",
            json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let store = SqliteStore::open(path, "http-test").unwrap();
    assert!(store.load("org").await.unwrap().is_none());
    let server = state_for(Arc::new(store), 0, true);
    assert_eq!(
        send(
            server,
            "one",
            "repo-fixture",
            call(
                "kimetsu_brain_conflict_resolve",
                json!({"conflict_id":"unknown", "winner":"left"})
            )
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
}

struct Unavailable(Arc<dyn BrainStore>);
#[async_trait]
impl BrainStore for Unavailable {
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
async fn publication_failure_returns_503_without_an_acknowledged_write() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("authority.db");
    let mut server = state(&path, 0, false);
    let store = server.durable.as_ref().unwrap().store.clone();
    server = server.with_storage(Arc::new(Unavailable(store.clone())), false);
    let (status, response) = send(
        server,
        "one",
        "admin-fixture",
        call(
            "kimetsu_brain_record",
            json!({"lesson":"This failed publication must not be acknowledged"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{response}");
    assert!(response.get("error").is_some());
    assert!(store.load("repo/one").await.unwrap().is_none());
}
