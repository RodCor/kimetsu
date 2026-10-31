use std::sync::Arc;

use kimetsu_remote::storage::{
    BrainStore, CommitOutcome, MAX_STATE_BYTES, StoreError, StoreWrite, StoredValue,
    sqlite::SqliteStore,
};

fn write(key: &str, expected: Option<&str>, revision: &str, data: &[u8]) -> StoreWrite {
    StoreWrite {
        key: key.into(),
        expected: expected.map(str::to_owned),
        revision: revision.into(),
        data: data.to_vec(),
    }
}

// Removing CAS checks, publishing before all checks, losing namespace isolation,
// or using instance-local counters must break this same contract on every adapter.
async fn contract(
    first: Arc<dyn BrainStore>,
    second: Arc<dyn BrainStore>,
    other: Arc<dyn BrainStore>,
) {
    assert_eq!(first.load("repo").await.unwrap(), None);
    assert_eq!(first.commit(&[]).await.unwrap(), CommitOutcome::Committed);
    let initial = write("repo", None, "v1", &[0, 255, 1]);
    assert_eq!(
        first.commit(std::slice::from_ref(&initial)).await.unwrap(),
        CommitOutcome::Committed
    );
    assert_eq!(
        second.load("repo").await.unwrap(),
        Some(StoredValue {
            revision: "v1".into(),
            data: vec![0, 255, 1],
        })
    );
    assert_eq!(other.load("repo").await.unwrap(), None);
    assert_eq!(
        second.commit(&[initial]).await.unwrap(),
        CommitOutcome::Conflict
    );
    let rollback = [
        write("new", None, "v2", b"new"),
        write("repo", Some("stale"), "v2", b"bad"),
    ];
    assert_eq!(
        second.commit(&rollback).await.unwrap(),
        CommitOutcome::Conflict
    );
    assert_eq!(first.load("new").await.unwrap(), None);
    assert_eq!(first.load("repo").await.unwrap().unwrap().revision, "v1");
    let winner_a = [
        write("repo", Some("v1"), "v2a", b"a"),
        write("org", None, "v2a", b"a"),
    ];
    let winner_b = [
        write("org", None, "v2b", b"b"),
        write("repo", Some("v1"), "v2b", b"b"),
    ];
    let (a, b) = tokio::join!(first.commit(&winner_a), second.commit(&winner_b));
    assert!(matches!(
        (a.unwrap(), b.unwrap()),
        (CommitOutcome::Committed, CommitOutcome::Conflict)
            | (CommitOutcome::Conflict, CommitOutcome::Committed)
    ));
    assert_eq!(
        first.load("repo").await.unwrap().unwrap().revision,
        second.load("org").await.unwrap().unwrap().revision
    );
    let create_a = [
        write("absent-a", None, "create_a", b"a"),
        write("absent-b", None, "create_a", b"a"),
    ];
    let create_b = [
        write("absent-b", None, "create_b", b"b"),
        write("absent-a", None, "create_b", b"b"),
    ];
    let (a, b) = tokio::join!(first.commit(&create_a), second.commit(&create_b));
    assert!(matches!(
        (a.unwrap(), b.unwrap()),
        (CommitOutcome::Committed, CommitOutcome::Conflict)
            | (CommitOutcome::Conflict, CommitOutcome::Committed)
    ));
    assert_eq!(
        first.load("absent-a").await.unwrap().unwrap().revision,
        second.load("absent-b").await.unwrap().unwrap().revision
    );
    let distinct = [
        write("a:b", None, "x1", b"one"),
        write("a", None, "x2", b"two"),
        write("r:é", None, "x3", b"three"),
    ];
    assert_eq!(
        first.commit(&distinct).await.unwrap(),
        CommitOutcome::Committed
    );
    assert_eq!(second.load("a:b").await.unwrap().unwrap().data, b"one");
    assert_eq!(second.load("a").await.unwrap().unwrap().data, b"two");
    assert_eq!(second.load("r:é").await.unwrap().unwrap().data, b"three");

    let duplicate = [
        write("fresh", None, "v1", b"1"),
        write("fresh", None, "v2", b"2"),
    ];
    assert!(matches!(
        first.commit(&duplicate).await,
        Err(StoreError::Invalid(_))
    ));
    assert_eq!(second.load("fresh").await.unwrap(), None);
    let invalid_expected = [write("fresh", Some("invalid revision"), "v1", b"one")];
    assert!(matches!(
        first.commit(&invalid_expected).await,
        Err(StoreError::Invalid(_))
    ));
    let too_many: Vec<_> = (0..5)
        .map(|i| write(&format!("batch-{i}"), None, "v1", b"one"))
        .collect();
    assert!(matches!(
        first.commit(&too_many).await,
        Err(StoreError::Invalid(_))
    ));
    assert_eq!(second.load("batch-0").await.unwrap(), None);
    let invalid = [
        write("fresh", None, "v1", b"1"),
        write("bad", None, "bad revision", b"2"),
    ];
    assert!(matches!(
        first.commit(&invalid).await,
        Err(StoreError::Invalid(_))
    ));
    assert_eq!(second.load("fresh").await.unwrap(), None);
    assert!(matches!(
        first
            .commit(&[write("big", None, "v1", &vec![0; MAX_STATE_BYTES + 1])])
            .await,
        Err(StoreError::Invalid(_))
    ));
    assert!(matches!(first.load("").await, Err(StoreError::Invalid(_))));
    assert!(matches!(
        first.allow_rate("bad\nkey", 1, 3).await,
        Err(StoreError::Invalid(_))
    ));

    let mut jobs = Vec::new();
    for i in 0..12 {
        let store = if i % 2 == 0 {
            first.clone()
        } else {
            second.clone()
        };
        jobs.push(tokio::spawn(async move {
            store.allow_rate("token", 100, 3).await.unwrap()
        }));
    }
    let mut allowed = 0;
    for job in jobs {
        allowed += usize::from(job.await.unwrap());
    }
    assert_eq!(allowed, 3);
    assert!(first.allow_rate("token", 101, 3).await.unwrap());
    assert!(other.allow_rate("token", 100, 3).await.unwrap());
    assert!(first.allow_rate("token", 100, 0).await.unwrap());
    assert!(!second.allow_rate("token", 100, 3).await.unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_contract_across_independent_instances() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("checkpoints.sqlite3");
    contract(
        Arc::new(SqliteStore::open(path.clone(), "contract").unwrap()),
        Arc::new(SqliteStore::open(path.clone(), "contract").unwrap()),
        Arc::new(SqliteStore::open(path.clone(), "other").unwrap()),
    )
    .await;
    let reopened = SqliteStore::open(path, "contract").unwrap();
    assert!(reopened.load("repo").await.unwrap().is_some());
}

#[tokio::test]
async fn sqlite_rejects_corrupt_records_without_publishing_a_batch() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("corrupt.sqlite3");
    let store = SqliteStore::open(path.clone(), "ns").unwrap();
    let connection = rusqlite::Connection::open(path).unwrap();
    connection.execute("INSERT INTO kimetsu_checkpoints(namespace, key, revision, data) VALUES('ns','bad','bad revision',X'01')", []).unwrap();
    assert!(matches!(
        store.load("bad").await,
        Err(StoreError::Corrupt("sqlite"))
    ));
    assert!(matches!(
        store
            .commit(&[
                write("new", None, "v1", b"ok"),
                write("bad", Some("v1"), "v2", b"bad")
            ])
            .await,
        Err(StoreError::Corrupt("sqlite"))
    ));
    assert_eq!(store.load("new").await.unwrap(), None);
    connection.execute("INSERT INTO kimetsu_checkpoints(namespace,key,revision,data) VALUES('ns','wrongtype','v1','text')", []).unwrap();
    assert!(matches!(
        store.load("wrongtype").await,
        Err(StoreError::Corrupt("sqlite"))
    ));
}

#[tokio::test]
async fn sqlite_rate_buckets_expire_without_losing_current_limits() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rates.sqlite3");
    let store = SqliteStore::open(path.clone(), "ns").unwrap();
    for window in 100..110 {
        assert!(store.allow_rate("token", window, 1).await.unwrap());
        assert!(!store.allow_rate("token", window, 1).await.unwrap());
    }
    let connection = rusqlite::Connection::open(path).unwrap();
    let retained: i64 = connection
        .query_row("SELECT count(*) FROM kimetsu_rate_limits", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert!(retained <= 3);
    assert!(!store.allow_rate("token", 109, 1).await.unwrap());
}

#[test]
fn sqlite_validates_namespace_and_redacts_debug() {
    let dir = tempfile::tempdir().unwrap();
    assert!(matches!(
        SqliteStore::open(dir.path().join("bad"), "bad namespace"),
        Err(StoreError::Invalid(_))
    ));
    let store = SqliteStore::open(dir.path().join("secret-path"), "ns").unwrap();
    assert!(!format!("{store:?}").contains("secret-path"));
}

#[test]
fn sqlite_refuses_memory_databases_that_cannot_survive_restarts() {
    for path in [
        "",
        ":memory:",
        "file:kimetsu-memory?mode=memory&cache=shared",
    ] {
        assert!(matches!(
            SqliteStore::open(path.into(), "ns"),
            Err(StoreError::Invalid(_))
        ));
    }
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires explicit KIMETSU_TEST_POSTGRES_URL dedicated test database"]
async fn postgres_contract_across_independent_instances() {
    use kimetsu_remote::storage::postgres::PostgresStore;
    let url = std::env::var("KIMETSU_TEST_POSTGRES_URL").expect("set dedicated test URL");
    let ns = format!("contract_{}", ulid::Ulid::new());
    contract(
        Arc::new(PostgresStore::connect(&url, &ns).await.unwrap()),
        Arc::new(PostgresStore::connect(&url, &ns).await.unwrap()),
        Arc::new(
            PostgresStore::connect(&url, &format!("{ns}_other"))
                .await
                .unwrap(),
        ),
    )
    .await;
}

#[cfg(feature = "postgres")]
#[tokio::test]
#[ignore = "requires explicit KIMETSU_TEST_POSTGRES_URL dedicated test database"]
async fn postgres_rejects_corrupt_records_without_publishing_a_batch() {
    use kimetsu_remote::storage::postgres::PostgresStore;
    let url = std::env::var("KIMETSU_TEST_POSTGRES_URL").expect("set dedicated test URL");
    let ns = format!("corrupt_{}", ulid::Ulid::new());
    let store = PostgresStore::connect(&url, &ns).await.unwrap();
    let tls = native_tls::TlsConnector::builder().build().unwrap();
    let (client, connection) =
        tokio_postgres::connect(&url, postgres_native_tls::MakeTlsConnector::new(tls))
            .await
            .unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client.execute("INSERT INTO kimetsu_checkpoints(namespace,key,revision,data) VALUES($1,'bad','bad revision',$2)", &[&ns, &vec![1_u8]]).await.unwrap();
    assert!(matches!(
        store.load("bad").await,
        Err(StoreError::Corrupt("postgres"))
    ));
    assert!(matches!(
        store
            .commit(&[
                write("new", None, "v1", b"ok"),
                write("bad", Some("v1"), "v2", b"bad")
            ])
            .await,
        Err(StoreError::Corrupt("postgres"))
    ));
    assert_eq!(store.load("new").await.unwrap(), None);
    for window in 100..110 {
        assert!(store.allow_rate("token", window, 1).await.unwrap());
        assert!(!store.allow_rate("token", window, 1).await.unwrap());
    }
    let retained: i64 = client
        .query_one(
            "SELECT count(*) FROM kimetsu_rate_limits WHERE namespace=$1",
            &[&ns],
        )
        .await
        .unwrap()
        .get(0);
    assert!(retained <= 3);
}

#[cfg(feature = "redis")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires explicit KIMETSU_TEST_REDIS_URL dedicated test database"]
async fn redis_contract_across_independent_instances() {
    use kimetsu_remote::storage::redis::RedisStore;
    let url = std::env::var("KIMETSU_TEST_REDIS_URL").expect("set dedicated test URL");
    let ns = format!("contract_{}", ulid::Ulid::new());
    contract(
        Arc::new(RedisStore::connect(&url, &ns).await.unwrap()),
        Arc::new(RedisStore::connect(&url, &ns).await.unwrap()),
        Arc::new(
            RedisStore::connect(&url, &format!("{ns}_other"))
                .await
                .unwrap(),
        ),
    )
    .await;
}

#[cfg(feature = "redis")]
#[tokio::test]
#[ignore = "requires explicit KIMETSU_TEST_REDIS_URL dedicated test database"]
async fn redis_rejects_wrong_root_type_and_corrupt_pairs_without_mutation() {
    use kimetsu_remote::storage::redis::RedisStore;
    let url = std::env::var("KIMETSU_TEST_REDIS_URL").expect("set dedicated test URL");
    let ns = format!("corrupt_{}", ulid::Ulid::new());
    let root = format!("kimetsu:{{{ns}}}:checkpoints");
    let store = RedisStore::connect(&url, &ns).await.unwrap();
    let mut connection = ::redis::Client::open(url)
        .unwrap()
        .get_multiplexed_async_connection()
        .await
        .unwrap();
    let _: () = ::redis::cmd("SET")
        .arg(&root)
        .arg("preserve-me")
        .query_async(&mut connection)
        .await
        .unwrap();
    assert!(matches!(
        store.load("repo").await,
        Err(StoreError::Corrupt("redis"))
    ));
    assert!(matches!(
        store.commit(&[write("repo", None, "v1", b"data")]).await,
        Err(StoreError::Corrupt("redis"))
    ));
    let original: String = ::redis::cmd("GET")
        .arg(&root)
        .query_async(&mut connection)
        .await
        .unwrap();
    assert_eq!(original, "preserve-me");
    let _: () = ::redis::cmd("DEL")
        .arg(&root)
        .query_async(&mut connection)
        .await
        .unwrap();
    let _: () = ::redis::cmd("HSET")
        .arg(&root)
        .arg("r:repo")
        .arg("v1")
        .query_async(&mut connection)
        .await
        .unwrap();
    assert!(matches!(
        store.load("repo").await,
        Err(StoreError::Corrupt("redis"))
    ));
    assert!(matches!(
        store
            .commit(&[
                write("fresh", None, "v1", b"one"),
                write("repo", Some("v1"), "v2", b"two")
            ])
            .await,
        Err(StoreError::Corrupt("redis"))
    ));
    assert_eq!(store.load("fresh").await.unwrap(), None);
    let _: () = ::redis::cmd("HSET")
        .arg(&root)
        .arg("d:repo")
        .arg("data")
        .arg("r:repo")
        .arg(&[255_u8][..])
        .query_async(&mut connection)
        .await
        .unwrap();
    assert!(matches!(
        store.load("repo").await,
        Err(StoreError::Corrupt("redis"))
    ));
    assert!(matches!(
        store
            .commit(&[write("repo", Some("v1"), "v2", b"two")])
            .await,
        Err(StoreError::Corrupt("redis"))
    ));
    let _: () = ::redis::cmd("DEL")
        .arg(root)
        .query_async(&mut connection)
        .await
        .unwrap();
}

#[cfg(feature = "redis")]
#[tokio::test]
#[ignore = "requires explicit KIMETSU_TEST_REDIS_URL dedicated test database"]
async fn redis_rate_counter_has_ttl_and_does_not_modify_checkpoint_hash() {
    use kimetsu_remote::storage::redis::RedisStore;
    let url = std::env::var("KIMETSU_TEST_REDIS_URL").expect("set dedicated test URL");
    let ns = format!("rates_{}", ulid::Ulid::new());
    let store = RedisStore::connect(&url, &ns).await.unwrap();
    assert_eq!(
        store
            .commit(&[write("repo", None, "v1", b"data")])
            .await
            .unwrap(),
        CommitOutcome::Committed
    );
    assert!(store.allow_rate("token", 100, 1).await.unwrap());
    assert!(!store.allow_rate("token", 100, 1).await.unwrap());
    let mut connection = ::redis::Client::open(url)
        .unwrap()
        .get_multiplexed_async_connection()
        .await
        .unwrap();
    let ttl: i64 = ::redis::cmd("TTL")
        .arg(format!("kimetsu:{{{ns}}}:rate:100:token"))
        .query_async(&mut connection)
        .await
        .unwrap();
    assert!(ttl > 0 && ttl <= 180);
    let durable_ttl: i64 = ::redis::cmd("TTL")
        .arg(format!("kimetsu:{{{ns}}}:checkpoints"))
        .query_async(&mut connection)
        .await
        .unwrap();
    assert_eq!(durable_ttl, -1);
    assert_eq!(store.load("repo").await.unwrap().unwrap().data, b"data");
}

#[cfg(any(feature = "postgres", feature = "redis"))]
#[tokio::test]
async fn network_constructors_validate_without_leaking_connection_secrets() {
    let secret = "scheme://username:super-secret@invalid endpoint";
    #[cfg(feature = "postgres")]
    {
        use kimetsu_remote::storage::postgres::PostgresStore;
        assert!(matches!(
            PostgresStore::connect(secret, "invalid namespace").await,
            Err(StoreError::Invalid(_))
        ));
        let error = PostgresStore::connect(secret, "ns").await.unwrap_err();
        assert!(!format!("{error:?} {error}").contains("super-secret"));
    }
    #[cfg(feature = "redis")]
    {
        use kimetsu_remote::storage::redis::RedisStore;
        assert!(matches!(
            RedisStore::connect(secret, "invalid namespace").await,
            Err(StoreError::Invalid(_))
        ));
        let error = RedisStore::connect(secret, "ns").await.unwrap_err();
        assert!(!format!("{error:?} {error}").contains("super-secret"));
    }
}
