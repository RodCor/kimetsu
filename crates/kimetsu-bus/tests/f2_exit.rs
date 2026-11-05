//! ADR 0001 F2 exit checks. Two in-process iroh endpoints stand in for two
//! LAN processes: real QUIC, real Ed25519 verify, then the store's import.

use std::path::{Path, PathBuf};
use std::sync::Once;
use std::time::{Duration, Instant};

use iroh::endpoint::presets;
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{Endpoint, EndpointAddr, SecretKey};
use kimetsu_brain::mount::{self, MountGrants};
use kimetsu_brain::packs::MemoryExport;
use kimetsu_brain::project::{self, ProposalFilter};
use kimetsu_brain::signed_pack::{self, PackManifest, PackSigner, TrustStore};
use kimetsu_bus::protocol::{self, Announcement};
use kimetsu_bus::{
    BusEndpoint, BusRelay, BusStats, EndpointConfig, PackRequest, RosterAdmin, RosterEndpoint,
    SignedRoster,
};
use kimetsu_core::memory::{MemoryKind, MemoryScope, MountLevel};

fn disable_user_brain() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        std::env::set_var("KIMETSU_USER_BRAIN", "0");
    });
}

fn brain_root(label: &str) -> PathBuf {
    disable_user_brain();
    let root = std::env::temp_dir().join(format!(
        "kimetsu-f2-{label}-{}-{}",
        std::process::id(),
        ulid_ish()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    kimetsu_core::paths::git_init_boundary(&root);
    project::init_project_at_root(&root, true).unwrap();
    root
}

fn ulid_ish() -> u128 {
    use std::time::SystemTime;
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

fn grants(levels: &[MountLevel]) -> MountGrants {
    let mut grants = MountGrants::default();
    for level in levels {
        grants.allow_write(*level);
    }
    grants
}

fn manifest() -> PackManifest {
    PackManifest {
        name: Some("f2".into()),
        version: Some("1".into()),
        description: Some("bus".into()),
        exported_at: Some("2026-01-01T00:00:00Z".into()),
    }
}

fn memory(scope: &str, text: &str) -> MemoryExport {
    MemoryExport {
        text: text.into(),
        scope: scope.into(),
        kind: "fact".into(),
        confidence: 0.95,
        created_at: Some("2026-01-01T00:00:00Z".into()),
    }
}

fn counts(root: &Path) -> (i64, i64, i64) {
    let (_, _, conn) = project::load_project_readonly(root).unwrap();
    let one = |sql: &str| -> i64 { conn.query_row(sql, [], |row| row.get(0)).unwrap() };
    (
        one("SELECT COUNT(*) FROM memories"),
        one("SELECT COUNT(*) FROM memory_proposals"),
        one("SELECT COUNT(*) FROM events"),
    )
}

fn active_text(root: &Path, text: &str) -> i64 {
    let (_, _, conn) = project::load_project_readonly(root).unwrap();
    conn.query_row(
        "SELECT COUNT(*) FROM memories WHERE text = ?1 AND invalidated_at IS NULL",
        [text],
        |row| row.get(0),
    )
    .unwrap()
}

struct Members {
    roster: SignedRoster,
    server_key: SecretKey,
    client_key: SecretKey,
    signer: PackSigner,
}

fn members(mounts: &[MountLevel], publish: bool) -> Members {
    let server_key = SecretKey::generate();
    let client_key = SecretKey::generate();
    let signer = PackSigner::generate().unwrap();
    let server_signers = if publish {
        vec![signer.public_key_hex()]
    } else {
        Vec::new()
    };
    let admin = RosterAdmin::generate().unwrap();
    let server = RosterEndpoint::new(server_key.public(), mounts, server_signers).unwrap();
    let client = RosterEndpoint::new(client_key.public(), mounts, None::<&str>).unwrap();
    let document = admin.sign("lab", &[server, client]).unwrap();
    let roster = SignedRoster::open(&document, &admin.public_key_hex()).unwrap();
    Members {
        roster,
        server_key,
        client_key,
        signer,
    }
}

async fn bind_pair(built: &Members, relay: BusRelay, insecure: bool) -> (BusEndpoint, BusEndpoint) {
    let server = bind_one(
        built.server_key.clone(),
        built.roster.clone(),
        relay.clone(),
        insecure,
        "server",
    )
    .await;
    let client = bind_one(
        built.client_key.clone(),
        built.roster.clone(),
        relay,
        insecure,
        "client",
    )
    .await;
    (server, client)
}

async fn bind_one(
    secret_key: SecretKey,
    roster: SignedRoster,
    relay: BusRelay,
    insecure: bool,
    label: &str,
) -> BusEndpoint {
    let path = std::env::temp_dir().join(format!(
        "kimetsu-f2-presence-{label}-{}-{}.json",
        std::process::id(),
        ulid_ish()
    ));
    BusEndpoint::bind(EndpointConfig {
        secret_key,
        roster,
        relay,
        mdns: true,
        insecure_relay_tls: insecure,
        presence_path: path,
    })
    .await
    .unwrap_or_else(|err| panic!("bind {label}: {err}"))
}

fn request(
    peer: EndpointAddr,
    digest: &str,
    destination: MountLevel,
    trust: TrustStore,
    brain: &Path,
    levels: &[MountLevel],
) -> PackRequest {
    PackRequest {
        peer,
        digest: digest.to_string(),
        destination,
        grants: grants(levels),
        trust,
        brain: brain.to_path_buf(),
    }
}

#[tokio::test]
async fn direct_announce_request_verifies_and_imports() {
    let built = members(&[MountLevel::Repo], true);
    let (server, client) = bind_pair(&built, BusRelay::Disabled, false).await;
    assert!(
        server.address_lookup_enabled(),
        "mDNS lookup must be attached"
    );
    let lesson = "F2_LAN_LESSON run the focused cargo test";
    let announcement = server
        .publish_memories(
            &manifest(),
            &["repo".into()],
            &[memory("repo", lesson)],
            &built.signer,
        )
        .unwrap();
    let root = brain_root("import");
    let before = counts(&root);
    let addr = server.direct_addr().await.unwrap();
    let seen = client.exchange_announcements(addr.clone()).await.unwrap();
    assert_eq!(seen[0].digest, announcement.digest);
    assert_eq!(counts(&root), before, "announce must not write the brain");
    assert!(
        client.presence().list().unwrap()[0].digest == announcement.digest,
        "presence is recorded off the brain"
    );
    let presence = std::fs::read_to_string(client.presence().path()).unwrap();
    assert!(
        !presence.contains(lesson),
        "presence file must not carry memory text"
    );

    let mut trust = TrustStore::empty();
    trust
        .allow(&built.signer.public_key_hex(), &[MountLevel::Repo])
        .unwrap();
    let summary = client
        .request_pack(request(
            addr,
            &announcement.digest,
            MountLevel::Repo,
            trust,
            &root,
            &[MountLevel::Repo],
        ))
        .await
        .unwrap();
    assert_eq!(summary.imported, 1, "{summary:?}");
    assert_eq!(summary.quarantined, 0);
    assert_eq!(active_text(&root, lesson), 1);

    server.close().await;
    client.close().await;
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn relay_path_still_verifies_before_import() {
    let (map, url, relay) = iroh::test_utils::run_relay_server()
        .await
        .expect("in-process relay");
    let built = members(&[MountLevel::Repo], true);
    let (server, client) = bind_pair(&built, BusRelay::Custom(map), true).await;
    tokio::time::timeout(Duration::from_secs(15), server.online())
        .await
        .expect("server relay");
    tokio::time::timeout(Duration::from_secs(15), client.online())
        .await
        .expect("client relay");
    let lesson = "F2_RELAY_LESSON ciphertext only";
    let announcement = server
        .publish_memories(
            &manifest(),
            &["repo".into()],
            &[memory("repo", lesson)],
            &built.signer,
        )
        .unwrap();
    let root = brain_root("relay");
    let mut trust = TrustStore::empty();
    trust
        .allow(&built.signer.public_key_hex(), &[MountLevel::Repo])
        .unwrap();
    let summary = tokio::time::timeout(
        Duration::from_secs(20),
        client.request_pack(request(
            server.relay_addr(url),
            &announcement.digest,
            MountLevel::Repo,
            trust,
            &root,
            &[MountLevel::Repo],
        )),
    )
    .await
    .expect("relay request timed out")
    .expect("relay request");
    assert_eq!(summary.imported, 1, "{summary:?}");
    assert_eq!(active_text(&root, lesson), 1);
    drop(relay);
    server.close().await;
    client.close().await;
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn non_member_handshake_fails_and_is_not_dialed() {
    let built = members(&[MountLevel::Repo], true);
    let server = bind_one(
        built.server_key.clone(),
        built.roster.clone(),
        BusRelay::Disabled,
        false,
        "gate",
    )
    .await;
    let stranger = SecretKey::generate();
    let stranger_endpoint = Endpoint::builder(presets::Minimal)
        .secret_key(stranger.clone())
        .alpns(vec![protocol::ALPN.to_vec()])
        .bind()
        .await
        .unwrap();
    let addr = server.direct_addr().await.unwrap();
    let conn = stranger_endpoint
        .connect(addr, protocol::ALPN)
        .await
        .expect("quic dial of a non-member still reaches the listener");
    let opened = conn.open_bi().await;
    if let Ok((mut send, mut recv)) = opened {
        let hello = protocol::encode_hello(&"ab".repeat(32)).unwrap();
        let _ = protocol::write_frame(&mut send, protocol::FRAME_HELLO, &hello).await;
        let _ = protocol::read_frame(&mut recv).await;
    }
    wait_stats(&server, |stats| stats.non_member_drops >= 1).await;
    let brain = brain_root("non-member");
    let before = counts(&brain);
    let started = Instant::now();
    let err = server
        .request_pack(request(
            EndpointAddr::new(stranger.public()).with_ip_addr("127.0.0.1:1".parse().unwrap()),
            &"cd".repeat(32),
            MountLevel::Repo,
            TrustStore::empty(),
            &brain,
            &[MountLevel::Repo],
        ))
        .await
        .unwrap_err();
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "refusing a non-member must not dial: {err}"
    );
    assert!(err.contains("non-member"), "{err}");
    assert_eq!(counts(&brain), before);
    stranger_endpoint.close().await;
    server.close().await;
    let _ = std::fs::remove_dir_all(&brain);
}

#[tokio::test]
async fn scope_mismatch_does_not_write() {
    let built = members(&[MountLevel::Repo, MountLevel::Org], true);
    let (server, client) = bind_pair(&built, BusRelay::Disabled, false).await;
    let lesson = "F2_SCOPE_MISMATCH repo lesson must not enter org";
    let announcement = server
        .publish_memories(
            &manifest(),
            &["repo".into()],
            &[memory("repo", lesson)],
            &built.signer,
        )
        .unwrap();
    let root = brain_root("scope");
    let before = counts(&root);
    let mut trust = TrustStore::empty();
    trust
        .allow(&built.signer.public_key_hex(), &[MountLevel::Org])
        .unwrap();
    let err = client
        .request_pack(request(
            server.direct_addr().await.unwrap(),
            &announcement.digest,
            MountLevel::Org,
            trust,
            &root,
            &[MountLevel::Org],
        ))
        .await
        .unwrap_err();
    assert!(err.contains("scope mismatch"), "{err}");
    assert_eq!(counts(&root), before, "scope mismatch must not write");
    assert_eq!(active_text(&root, lesson), 0);
    let proposals = project::list_proposals(
        &root,
        ProposalFilter {
            status: Some("pending".into()),
            ..ProposalFilter::default()
        },
    )
    .unwrap();
    assert!(proposals.is_empty(), "scope mismatch must not quarantine");
    server.close().await;
    client.close().await;
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn untrusted_signer_quarantines_until_a_human_accepts() {
    let built = members(&[MountLevel::Repo], true);
    let (server, client) = bind_pair(&built, BusRelay::Disabled, false).await;
    let lesson = "F2_QUARANTINE_PHRASE needs a human";
    let announcement = server
        .publish_memories(
            &manifest(),
            &["repo".into()],
            &[memory("repo", lesson)],
            &built.signer,
        )
        .unwrap();
    let root = brain_root("quarantine");
    let summary = client
        .request_pack(request(
            server.direct_addr().await.unwrap(),
            &announcement.digest,
            MountLevel::Repo,
            TrustStore::empty(),
            &root,
            &[MountLevel::Repo],
        ))
        .await
        .unwrap();
    assert_eq!(summary.quarantined, 1, "{summary:?}");
    assert_eq!(summary.imported, 0);
    assert_eq!(active_text(&root, lesson), 0);
    let proposals = project::list_proposals(
        &root,
        ProposalFilter {
            status: Some("pending".into()),
            ..ProposalFilter::default()
        },
    )
    .unwrap();
    assert!(
        proposals
            .iter()
            .any(|row| row.text == lesson && row.status == "pending"),
        "{proposals:?}"
    );
    let mut read = MountGrants::default();
    read.allow_read(MountLevel::Repo);
    let bundle = mount::context(&root, &read, lesson, 400).unwrap();
    assert!(
        bundle
            .capsules
            .iter()
            .all(|capsule| !capsule.summary.contains(lesson)),
        "quarantine must stay out of retrieval"
    );
    server.close().await;
    client.close().await;
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn v1_on_the_wire_does_not_import() {
    let server_key = SecretKey::generate();
    let client_key = SecretKey::generate();
    let signer = PackSigner::generate().unwrap();
    let admin = RosterAdmin::generate().unwrap();
    let digest = "ef".repeat(32);
    let server_entry = RosterEndpoint::new(
        server_key.public(),
        &[MountLevel::Repo],
        [signer.public_key_hex()],
    )
    .unwrap();
    let client_entry =
        RosterEndpoint::new(client_key.public(), &[MountLevel::Repo], None::<&str>).unwrap();
    let roster = SignedRoster::open(
        &admin.sign("lab", &[server_entry, client_entry]).unwrap(),
        &admin.public_key_hex(),
    )
    .unwrap();
    let honest = bind_one(
        server_key.clone(),
        roster.clone(),
        BusRelay::Disabled,
        false,
        "v1-honest",
    )
    .await;
    let v1 = signed_pack::encode_gzip(
        r#"{"kimetsu_pack":1,"memory_count":1,"memories":[{"confidence":0.9,"kind":"fact","scope":"repo","text":"V1_MUST_NOT_LAND"}]}"#,
    )
    .unwrap();
    let err = honest.publish_pack(&v1, &signer).unwrap_err();
    assert!(
        err.contains("version") || err.contains("kimetsu_pack"),
        "{err}"
    );
    honest.close().await;

    let evil = serve_v1(server_key, roster.id().to_string(), digest.clone(), v1).await;
    let client = bind_one(client_key, roster, BusRelay::Disabled, false, "v1-client").await;
    let root = brain_root("v1");
    let before = counts(&root);
    let err = client
        .request_pack(request(
            evil,
            &digest,
            MountLevel::Repo,
            TrustStore::empty(),
            &root,
            &[MountLevel::Repo],
        ))
        .await
        .unwrap_err();
    assert!(
        err.contains("version") || err.contains("kimetsu_pack") || err.contains("signature"),
        "{err}"
    );
    assert_eq!(counts(&root), before, "v1 must not write");
    client.close().await;
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn git_frame_is_dropped_and_is_not_a_protocol_message() {
    let banned = [
        "git_clone",
        "git_fetch",
        "git_checkout",
        "worktree",
        "checkout",
        "clone",
    ];
    let names = protocol::LEGAL_FRAME_NAMES.join(" ");
    for word in banned {
        assert!(!names.contains(word), "{word} in {names}");
    }
    let source = include_str!("../src/protocol.rs");
    assert!(
        !source.contains("fn git_") && !source.contains("enum Git"),
        "protocol source must not grow a repository operation"
    );

    let built = members(&[MountLevel::Repo], true);
    let evil_key = SecretKey::generate();
    let admin = RosterAdmin::generate().unwrap();
    let server_entry = RosterEndpoint::new(
        built.server_key.public(),
        &[MountLevel::Repo],
        [built.signer.public_key_hex()],
    )
    .unwrap();
    let evil_entry =
        RosterEndpoint::new(evil_key.public(), &[MountLevel::Repo], None::<&str>).unwrap();
    let roster = SignedRoster::open(
        &admin.sign("lab", &[server_entry, evil_entry]).unwrap(),
        &admin.public_key_hex(),
    )
    .unwrap();
    let server = bind_one(
        built.server_key.clone(),
        roster,
        BusRelay::Disabled,
        false,
        "git-drop",
    )
    .await;
    let root = brain_root("git");
    project::add_memory(
        &root,
        MemoryScope::Repo,
        MemoryKind::Fact,
        "local row stays",
    )
    .unwrap();
    let before = counts(&root);
    let evil = Endpoint::builder(presets::Minimal)
        .secret_key(evil_key)
        .alpns(vec![protocol::ALPN.to_vec()])
        .bind()
        .await
        .unwrap();
    let conn = evil
        .connect(server.direct_addr().await.unwrap(), protocol::ALPN)
        .await
        .unwrap();
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    protocol::write_frame(&mut send, 64, b"git clone --bare\nworktree\ncheckout")
        .await
        .unwrap();
    let failed = protocol::read_frame(&mut recv).await;
    assert!(
        failed.is_err(),
        "peer that sends a repository frame must be dropped"
    );
    wait_stats(&server, |stats| stats.protocol_drops >= 1).await;
    assert_eq!(counts(&root), before);
    let presence = std::fs::read_to_string(server.presence().path()).unwrap_or_default();
    assert!(!presence.contains("git clone"));
    assert!(!presence.contains("worktree"));
    evil.close().await;
    server.close().await;
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn stopping_the_relay_does_not_change_context_latency() {
    let root = brain_root("latency");
    project::add_memory(
        &root,
        MemoryScope::Repo,
        MemoryKind::Fact,
        "local lesson about cargo test",
    )
    .unwrap();
    let mut read = MountGrants::default();
    read.allow_read(MountLevel::Repo);
    for _ in 0..3 {
        mount::context(&root, &read, "cargo test", 200).unwrap();
        mount::search(&root, &read, "cargo test", Some(MountLevel::Repo)).unwrap();
    }

    let (map, _url, relay) = iroh::test_utils::run_relay_server().await.expect("relay");
    let built = members(&[MountLevel::Repo], true);
    let endpoint = bind_one(
        built.server_key.clone(),
        built.roster.clone(),
        BusRelay::Custom(map),
        true,
        "latency",
    )
    .await;
    tokio::time::timeout(Duration::from_secs(15), endpoint.online())
        .await
        .expect("online");
    let with_relay = median_context(&root, &read);
    drop(relay);
    let without_relay = median_context(&root, &read);
    assert!(
        with_relay < Duration::from_millis(500),
        "context while the relay is up took {with_relay:?}"
    );
    assert!(
        without_relay < Duration::from_millis(500),
        "context after the relay stopped took {without_relay:?}"
    );
    let delta = with_relay.abs_diff(without_relay);
    assert!(
        delta < Duration::from_millis(400),
        "stopping the relay changed context latency by {delta:?} ({with_relay:?} -> {without_relay:?})"
    );
    let started = Instant::now();
    mount::search(&root, &read, "cargo test", Some(MountLevel::Repo)).unwrap();
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "search waited on the stopped relay"
    );
    endpoint.close().await;
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn remote_does_not_dial_and_sync_still_replicates_events() {
    let remote_manifest = include_str!("../../kimetsu-remote/Cargo.toml");
    let remote_lib = include_str!("../../kimetsu-remote/src/lib.rs");
    assert!(!remote_manifest.contains("kimetsu-bus"));
    assert!(!remote_manifest.contains("iroh"));
    assert!(!remote_lib.contains("iroh"));
    assert!(!remote_lib.contains("request_pack"));

    let brain_manifest = include_str!("../../kimetsu-brain/Cargo.toml");
    let mount = include_str!("../../kimetsu-brain/src/mount.rs");
    let sync = include_str!("../../kimetsu-brain/src/sync.rs");
    assert!(
        !brain_manifest.contains("iroh"),
        "F1 crate stays off the bus"
    );
    assert!(!mount.contains("request_pack"));
    assert!(!mount.contains("iroh"));
    assert!(!sync.contains("iroh"));
    assert!(!sync.contains("request_pack"));
    assert!(kimetsu_brain::sync::is_sync_allowed("memory.accepted"));
    assert!(kimetsu_brain::sync::is_sync_allowed("memory.proposed"));
    assert!(!kimetsu_brain::sync::is_sync_allowed("run.finished"));
    assert!(!kimetsu_brain::sync::is_sync_allowed("work.episode"));
}

fn median_context(root: &Path, grants: &MountGrants) -> Duration {
    let mut samples = Vec::new();
    for _ in 0..5 {
        let started = Instant::now();
        mount::context(root, grants, "cargo test", 200).unwrap();
        samples.push(started.elapsed());
    }
    samples.sort();
    samples[samples.len() / 2]
}

async fn wait_stats(endpoint: &BusEndpoint, pred: impl Fn(BusStats) -> bool) {
    for _ in 0..50 {
        if pred(endpoint.stats()) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("bus stat not observed: {:?}", endpoint.stats());
}

async fn serve_v1(
    secret: SecretKey,
    roster_id: String,
    digest: String,
    gzip: Vec<u8>,
) -> EndpointAddr {
    let endpoint = Endpoint::builder(presets::Minimal)
        .secret_key(secret)
        .bind()
        .await
        .unwrap();
    let sockets = endpoint.bound_sockets();
    let ip = sockets
        .iter()
        .copied()
        .find(|socket| socket.is_ipv4())
        .expect("v1 peer socket");
    let ip = if ip.ip().is_unspecified() {
        std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, ip.port()))
    } else {
        ip
    };
    let addr = EndpointAddr::new(endpoint.id()).with_ip_addr(ip);
    let router = Router::builder(endpoint)
        .accept(
            protocol::ALPN,
            V1Handler {
                roster_id,
                digest,
                gzip,
            },
        )
        .spawn();
    // The accept loop has to outlive this function. The test process exits
    // after the client observes the rejection.
    std::mem::forget(router);
    addr
}

#[derive(Debug, Clone)]
struct V1Handler {
    roster_id: String,
    digest: String,
    gzip: Vec<u8>,
}

impl ProtocolHandler for V1Handler {
    async fn accept(&self, connection: iroh::endpoint::Connection) -> Result<(), AcceptError> {
        let (mut send, mut recv) = connection.accept_bi().await?;
        if protocol::read_frame(&mut recv).await.is_err() {
            return Ok(());
        }
        let Ok(hello) = protocol::encode_hello(&self.roster_id) else {
            return Ok(());
        };
        if protocol::write_frame(&mut send, protocol::FRAME_HELLO, &hello)
            .await
            .is_err()
        {
            return Ok(());
        }
        let announcement = Announcement {
            endpoint_id: "v1-peer".into(),
            digest: self.digest.clone(),
            byte_length: u64::try_from(self.gzip.len()).unwrap_or(1),
            scopes: vec!["repo".into()],
            signer: "11".repeat(32),
            ttl_secs: 60,
        };
        let Ok(body) = protocol::encode_announcements(&[announcement]) else {
            return Ok(());
        };
        if protocol::write_frame(&mut send, protocol::FRAME_ANNOUNCE, &body)
            .await
            .is_err()
        {
            return Ok(());
        }
        let _ = protocol::read_frame(&mut recv).await;
        let Ok(pack) = protocol::encode_pack_body(&self.digest, &self.gzip) else {
            return Ok(());
        };
        if protocol::write_frame(&mut send, protocol::FRAME_PACK, &pack)
            .await
            .is_err()
        {
            return Ok(());
        }
        // Stay up until the client has read the pack and finished its side.
        // Returning from accept drops the connection.
        let _ = send.finish();
        let _ = recv.read_to_end(4096).await;
        Ok(())
    }
}
