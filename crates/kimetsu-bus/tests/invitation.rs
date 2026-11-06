//! Invitation is a newer admin-signed roster. Connecting peers adopt the
//! higher generation and only then exchange a `kimetsu_pack` 2. A removal
//! is the same document, and it does not import.

use std::path::{Path, PathBuf};
use std::sync::Once;
use std::time::{SystemTime, UNIX_EPOCH};

use iroh::{EndpointAddr, SecretKey};
use kimetsu_brain::mount::MountGrants;
use kimetsu_brain::packs::MemoryExport;
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

fn brain_root(label: &str) -> PathBuf {
    disable_user_brain();
    let root = std::env::temp_dir().join(format!(
        "kimetsu-invite-{label}-{}-{}",
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
        "kimetsu-invite-{label}-{}-{}.json",
        std::process::id(),
        nanos()
    ))
}

fn nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
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
        name: Some("invite".into()),
        version: Some("1".into()),
        description: Some("roster".into()),
        exported_at: Some("2026-01-01T00:00:00Z".into()),
    }
}

fn memory(text: &str) -> MemoryExport {
    MemoryExport {
        text: text.into(),
        scope: "repo".into(),
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

struct Pair {
    admin: RosterAdmin,
    alice_key: SecretKey,
    bob_key: SecretKey,
    signer: PackSigner,
    genesis: SignedRoster,
    invited: SignedRoster,
}

fn pair() -> Pair {
    let admin = RosterAdmin::generate().unwrap();
    let alice_key = SecretKey::generate();
    let bob_key = SecretKey::generate();
    let signer = PackSigner::generate().unwrap();
    let alice = RosterEndpoint::new(alice_key.public(), &[MountLevel::Repo], None::<&str>).unwrap();
    let genesis = SignedRoster::open(
        &admin.sign("lab", &[alice]).unwrap(),
        &admin.public_key_hex(),
    )
    .unwrap();
    let bob = RosterEndpoint::new(
        bob_key.public(),
        &[MountLevel::Repo],
        [signer.public_key_hex()],
    )
    .unwrap();
    let invited = SignedRoster::open(
        &admin.invite(&genesis, bob).unwrap(),
        &admin.public_key_hex(),
    )
    .unwrap();
    Pair {
        admin,
        alice_key,
        bob_key,
        signer,
        genesis,
        invited,
    }
}

async fn bind(
    secret: SecretKey,
    roster: SignedRoster,
    roster_path: PathBuf,
    label: &str,
) -> BusEndpoint {
    BusEndpoint::bind(EndpointConfig {
        secret_key: secret,
        roster,
        relay: BusRelay::Disabled,
        mdns: false,
        insecure_relay_tls: false,
        presence_path: scratch(&format!("presence-{label}")),
        roster_path: Some(roster_path),
        bind_v4: None,
    })
    .await
    .unwrap_or_else(|err| panic!("bind {label}: {err}"))
}

#[tokio::test]
async fn invitation_converges_and_then_a_signed_pack_imports() {
    let built = pair();
    let alice_path = scratch("alice-roster");
    let bob_path = scratch("bob-roster");
    let alice = bind(
        built.alice_key.clone(),
        built.genesis.clone(),
        alice_path.clone(),
        "alice",
    )
    .await;
    let bob = bind(
        built.bob_key.clone(),
        built.invited.clone(),
        bob_path,
        "bob",
    )
    .await;
    assert_eq!(alice.roster_generation(), 0);
    assert!(!alice.roster_allows(bob.id()));
    let early = alice
        .converge(EndpointAddr::new(bob.id()))
        .await
        .unwrap_err();
    assert!(early.contains("non-member"), "{early}");
    assert_eq!(alice.roster_generation(), 0);

    let lesson = "INVITE_LESSON ship the signed pack after the roster converges";
    let announcement = bob
        .publish_memories(
            &manifest(),
            &["repo".into()],
            &[memory(lesson)],
            &built.signer,
        )
        .unwrap();
    let root = brain_root("import");
    let before = counts(&root);
    bob.converge(alice.direct_addr().await.unwrap())
        .await
        .expect("invitee converges the roster");
    assert_eq!(alice.roster_generation(), 1);
    assert_eq!(alice.roster_id(), bob.roster_id());
    assert!(alice.roster_allows(bob.id()));
    assert_eq!(
        counts(&root),
        before,
        "roster sync must not write the brain"
    );
    let stored = std::fs::read_to_string(&alice_path).unwrap();
    assert!(
        !stored.contains(lesson),
        "roster file must not carry memory text"
    );
    assert_eq!(
        SignedRoster::open(&stored, &built.admin.public_key_hex())
            .unwrap()
            .generation(),
        1
    );

    let mut trust = TrustStore::empty();
    trust
        .allow(&built.signer.public_key_hex(), &[MountLevel::Repo])
        .unwrap();
    let summary = alice
        .request_pack(PackRequest {
            peer: bob.direct_addr().await.unwrap(),
            digest: announcement.digest,
            destination: MountLevel::Repo,
            grants: grants(&[MountLevel::Repo]),
            trust,
            brain: root.clone(),
        })
        .await
        .expect("request_pack after the invitation");
    assert_eq!(summary.imported, 1, "{summary:?}");
    assert_eq!(summary.quarantined, 0);
    assert_eq!(active_text(&root, lesson), 1);

    alice.close().await;
    bob.close().await;
    let restarted = bind(
        built.alice_key.clone(),
        built.genesis.clone(),
        alice_path,
        "alice-restart",
    )
    .await;
    assert_eq!(restarted.roster_generation(), 1);
    assert!(restarted.roster_allows(built.bob_key.public()));
    restarted.close().await;
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn removal_reissue_blocks_request_pack_and_the_next_bind() {
    let built = pair();
    let alice_path = scratch("alice-remove");
    let bob_path = scratch("bob-remove");
    let alice = bind(
        built.alice_key.clone(),
        built.genesis.clone(),
        alice_path,
        "alice-remove",
    )
    .await;
    let bob = bind(
        built.bob_key.clone(),
        built.invited.clone(),
        bob_path.clone(),
        "bob-remove",
    )
    .await;
    bob.converge(alice.direct_addr().await.unwrap())
        .await
        .unwrap();
    let lesson = "INVITE_REMOVAL this row must stay local";
    let announcement = bob
        .publish_memories(
            &manifest(),
            &["repo".into()],
            &[memory(lesson)],
            &built.signer,
        )
        .unwrap();
    let alice_only = built
        .genesis
        .endpoints()
        .iter()
        .filter(|entry| entry.endpoint_id == built.alice_key.public())
        .cloned()
        .collect::<Vec<_>>();
    let removed = built.admin.reissue(&built.invited, &alice_only).unwrap();
    alice.install_roster(&removed).unwrap();
    assert_eq!(alice.roster_generation(), 2);
    assert!(!alice.roster_allows(bob.id()));

    let root = brain_root("removal");
    project::add_memory(&root, MemoryScope::Repo, MemoryKind::Fact, "kept local").unwrap();
    let before = counts(&root);
    let err = bob
        .converge(alice.direct_addr().await.unwrap())
        .await
        .unwrap_err();
    assert!(err.contains("removed"), "{err}");
    assert_eq!(bob.roster_generation(), 2);
    assert!(!bob.roster_allows(bob.id()));
    assert_eq!(counts(&root), before, "a removed peer must not import");

    let mut trust = TrustStore::empty();
    trust
        .allow(&built.signer.public_key_hex(), &[MountLevel::Repo])
        .unwrap();
    let err = bob
        .request_pack(PackRequest {
            peer: alice.direct_addr().await.unwrap(),
            digest: announcement.digest.clone(),
            destination: MountLevel::Repo,
            grants: grants(&[MountLevel::Repo]),
            trust: trust.clone(),
            brain: root.clone(),
        })
        .await
        .unwrap_err();
    assert!(
        err.contains("does not let this endpoint") || err.contains("non-member"),
        "{err}"
    );
    let err = alice
        .request_pack(PackRequest {
            peer: bob.direct_addr().await.unwrap(),
            digest: announcement.digest,
            destination: MountLevel::Repo,
            grants: grants(&[MountLevel::Repo]),
            trust,
            brain: root.clone(),
        })
        .await
        .unwrap_err();
    assert!(err.contains("non-member"), "{err}");
    assert_eq!(counts(&root), before);
    assert_eq!(active_text(&root, lesson), 0);

    bob.close().await;
    let rebound = BusEndpoint::bind(EndpointConfig {
        secret_key: built.bob_key.clone(),
        roster: built.invited.clone(),
        relay: BusRelay::Disabled,
        mdns: false,
        insecure_relay_tls: false,
        presence_path: scratch("bob-rebind-presence"),
        roster_path: Some(bob_path),
        bind_v4: None,
    })
    .await;
    let Err(rebound) = rebound else {
        panic!("removed endpoint must not bind");
    };
    assert!(rebound.contains("not on the roster"), "{rebound}");
    alice.close().await;
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn same_generation_fork_and_other_group_do_not_replace_the_roster() {
    let built = pair();
    let alice = bind(
        built.alice_key.clone(),
        built.genesis.clone(),
        scratch("alice-fork"),
        "alice-fork",
    )
    .await;
    let id = alice.roster_id();
    let fork = built
        .admin
        .sign_generation(
            "lab",
            &[
                RosterEndpoint::new(built.bob_key.public(), &[MountLevel::Org], None::<&str>)
                    .unwrap(),
            ],
            0,
        )
        .unwrap();
    let err = alice.install_roster(&fork).unwrap_err();
    assert!(err.contains("conflict"), "{err}");
    assert_eq!(alice.roster_generation(), 0);
    assert_eq!(alice.roster_id(), id);
    assert!(alice.roster_allows(built.alice_key.public()));
    assert!(!alice.roster_allows(built.bob_key.public()));

    let other = built
        .admin
        .sign_generation(
            "other",
            &[
                RosterEndpoint::new(built.alice_key.public(), &[MountLevel::Repo], None::<&str>)
                    .unwrap(),
            ],
            4,
        )
        .unwrap();
    let err = alice.install_roster(&other).unwrap_err();
    assert!(err.contains("group"), "{err}");
    assert_eq!(alice.roster_id(), id);
    alice.close().await;
}
