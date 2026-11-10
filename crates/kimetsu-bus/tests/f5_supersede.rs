//! ADR 0001 F5. Two peers disagree about which text supersedes one lesson.
//! The bus carries the signed pack. The store records the conflict. No model
//! picks a winner, and the announcement still has no memory text.

use std::path::{Path, PathBuf};
use std::sync::Once;
use std::time::{SystemTime, UNIX_EPOCH};

use iroh::EndpointAddr;
use iroh::SecretKey;
use kimetsu_brain::mount::{self, MountGrants};
use kimetsu_brain::project::{self};
use kimetsu_brain::signed_pack::{PackManifest, PackSigner, TrustStore};
use kimetsu_bus::{
    BusEndpoint, BusRelay, EndpointConfig, PackRequest, RosterAdmin, RosterEndpoint, SignedRoster,
};
use kimetsu_core::memory::{MemoryKind, MemoryScope, MountLevel};

fn disable_user_brain() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        std::env::set_var("KIMETSU_USER_BRAIN", "0");
    });
}

fn nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

fn brain_root(label: &str) -> PathBuf {
    disable_user_brain();
    let root = std::env::temp_dir().join(format!(
        "kimetsu-f5-bus-{label}-{}-{}",
        std::process::id(),
        nanos()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    kimetsu_core::paths::git_init_boundary(&root);
    project::init_project_at_root(&root, true).unwrap();
    root
}

fn scratch(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "kimetsu-f5-bus-{label}-{}-{}.json",
        std::process::id(),
        nanos()
    ))
}

fn grants() -> MountGrants {
    let mut grants = MountGrants::default();
    grants.allow_write(MountLevel::Repo);
    grants
}

fn manifest() -> PackManifest {
    PackManifest {
        name: Some("f5".into()),
        version: Some("1".into()),
        description: Some("bus supersede".into()),
        exported_at: Some("2026-01-01T00:00:00Z".into()),
    }
}

fn successor_of(root: &Path, predecessor: &str) -> Option<String> {
    let (_, _, conn) = project::load_project_readonly(root).unwrap();
    conn.query_row(
        "SELECT succ.text
         FROM memories pred
         JOIN memories succ ON succ.memory_id = pred.superseded_by
         WHERE pred.text = ?1",
        [predecessor],
        |row| row.get(0),
    )
    .unwrap()
}

fn superseded_events(root: &Path) -> i64 {
    let (_, _, conn) = project::load_project_readonly(root).unwrap();
    conn.query_row(
        "SELECT COUNT(*) FROM events WHERE kind = 'memory.superseded'",
        [],
        |row| row.get(0),
    )
    .unwrap()
}

fn conflict_count(root: &Path) -> i64 {
    let (_, _, conn) = project::load_project_readonly(root).unwrap();
    kimetsu_brain::sync::sync_conflict_count(&conn).unwrap()
}

struct Pair {
    alice_key: SecretKey,
    bob_key: SecretKey,
    signer: PackSigner,
    roster: SignedRoster,
}

fn pair() -> Pair {
    let admin = RosterAdmin::generate().unwrap();
    let alice_key = SecretKey::generate();
    let bob_key = SecretKey::generate();
    let signer = PackSigner::generate().unwrap();
    let alice = RosterEndpoint::new(
        alice_key.public(),
        &[MountLevel::Repo],
        [signer.public_key_hex()],
    )
    .unwrap();
    let bob = RosterEndpoint::new(
        bob_key.public(),
        &[MountLevel::Repo],
        [signer.public_key_hex()],
    )
    .unwrap();
    let roster = SignedRoster::open(
        &admin.sign("lab", &[alice, bob]).unwrap(),
        &admin.public_key_hex(),
    )
    .unwrap();
    Pair {
        alice_key,
        bob_key,
        signer,
        roster,
    }
}

async fn bind(secret: SecretKey, roster: SignedRoster, label: &str) -> BusEndpoint {
    BusEndpoint::bind(EndpointConfig {
        secret_key: secret,
        roster,
        relay: BusRelay::Disabled,
        mdns: false,
        insecure_relay_tls: false,
        presence_path: scratch(&format!("presence-{label}")),
        roster_path: None,
        bind_v4: None,
        map_ports: false,
    })
    .await
    .unwrap_or_else(|err| panic!("bind {label}: {err}"))
}

fn seed(root: &Path, predecessor: &str, successor: &str) {
    let pred = project::add_memory(root, MemoryScope::Repo, MemoryKind::Fact, predecessor).unwrap();
    let succ = project::add_memory(root, MemoryScope::Repo, MemoryKind::Fact, successor).unwrap();
    mount::supersede(root, &grants(), &pred, &succ).unwrap();
}

#[tokio::test]
async fn peers_exchange_disagreeing_supersedes_and_record_one_conflict() {
    let built = pair();
    let alice = bind(built.alice_key.clone(), built.roster.clone(), "alice").await;
    let bob = bind(built.bob_key.clone(), built.roster.clone(), "bob").await;
    assert_eq!(alice.roster_generation(), 0);
    assert_eq!(bob.roster_generation(), 0);

    let predecessor = "F5_BUS predecessor alpha rule";
    let earlier = "F5_BUS successor earlier";
    let later = "F5_BUS successor later";
    let alice_root = brain_root("alice");
    let bob_root = brain_root("bob");
    seed(&alice_root, predecessor, earlier);
    seed(&bob_root, predecessor, later);

    let alice_bytes = mount::export_signed_pack(
        &alice_root,
        &grants(),
        MountLevel::Repo,
        true,
        false,
        &manifest(),
        false,
        &built.signer,
    )
    .unwrap()
    .0;
    let bob_bytes = mount::export_signed_pack(
        &bob_root,
        &grants(),
        MountLevel::Repo,
        true,
        false,
        &manifest(),
        false,
        &built.signer,
    )
    .unwrap()
    .0;
    let alice_announcement = alice.publish_pack(&alice_bytes, &built.signer).unwrap();
    let bob_announcement = bob.publish_pack(&bob_bytes, &built.signer).unwrap();
    let announced = serde_json::to_string(&alice_announcement).unwrap();
    assert!(
        !announced.contains(predecessor) && !announced.contains("alpha rule"),
        "announcements must not carry the predecessor text"
    );

    let mut trust = TrustStore::empty();
    trust
        .allow(&built.signer.public_key_hex(), &[MountLevel::Repo])
        .unwrap();
    let alice_addr = alice.direct_addr().await.unwrap();
    let bob_addr = bob.direct_addr().await.unwrap();
    let request = |peer: EndpointAddr, digest: &str, brain: PathBuf| PackRequest {
        peer,
        digest: digest.to_string(),
        destination: MountLevel::Repo,
        grants: grants(),
        trust: trust.clone(),
        brain,
    };
    let to_bob = bob
        .request_pack(request(
            alice_addr.clone(),
            &alice_announcement.digest,
            bob_root.clone(),
        ))
        .await
        .expect("bob imports alice");
    let to_alice = alice
        .request_pack(request(
            bob_addr.clone(),
            &bob_announcement.digest,
            alice_root.clone(),
        ))
        .await
        .expect("alice imports bob");
    assert_eq!(to_bob.conflicts, 1, "{to_bob:?}");
    assert_eq!(to_alice.conflicts, 1, "{to_alice:?}");
    assert_eq!(
        successor_of(&alice_root, predecessor).as_deref(),
        Some(later)
    );
    assert_eq!(successor_of(&bob_root, predecessor).as_deref(), Some(later));
    assert_eq!(conflict_count(&alice_root), 1);
    assert_eq!(conflict_count(&bob_root), 1);
    assert_eq!(superseded_events(&alice_root), 2);
    assert_eq!(
        alice.roster_generation(),
        0,
        "a memory conflict is not a roster edit"
    );
    assert_eq!(bob.roster_generation(), 0);

    let again = alice
        .request_pack(request(
            bob_addr,
            &bob_announcement.digest,
            alice_root.clone(),
        ))
        .await
        .unwrap();
    assert_eq!(again.conflicts, 0);
    assert_eq!(superseded_events(&alice_root), 2);

    alice.close().await;
    bob.close().await;
    let _ = std::fs::remove_dir_all(&alice_root);
    let _ = std::fs::remove_dir_all(&bob_root);
}
