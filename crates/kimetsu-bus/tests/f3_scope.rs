//! Announce persistence stays off the recall transaction and off the
//! writer lock that rebuild uses.

use std::path::Path;
use std::time::{Duration, Instant};

use kimetsu_brain::lock::ProjectLock;
use kimetsu_brain::mount::{self, MountGrants};
use kimetsu_brain::project;
use kimetsu_bus::PresenceStore;
use kimetsu_bus::protocol::Announcement;
use kimetsu_core::memory::{MemoryKind, MemoryScope, MountLevel};
use kimetsu_core::paths::{ProjectPaths, git_init_boundary};

fn root(label: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "kimetsu-f3-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    git_init_boundary(&path);
    project::init_project_at_root(&path, true).unwrap();
    path
}

fn announcement() -> Announcement {
    Announcement {
        endpoint_id: "f3-endpoint".into(),
        digest: "ab".repeat(32),
        byte_length: 128,
        scopes: vec!["repo".into()],
        signer: "cd".repeat(32),
        ttl_secs: 60,
    }
}

#[test]
fn announce_persistence_is_outside_recall_and_the_rebuild_lock() {
    unsafe {
        std::env::set_var("KIMETSU_USER_BRAIN", "0");
        std::env::set_var("KIMETSU_BRAIN_EMBEDDER", "off");
    }
    let brain = root("presence");
    let id = project::add_memory(
        &brain,
        MemoryScope::Repo,
        MemoryKind::Fact,
        "f3 presence lesson stays local",
    )
    .unwrap();
    let presence_path = brain.join("presence.json");
    assert_ne!(
        presence_path.file_name().and_then(|n| n.to_str()),
        Some("brain.db")
    );
    let presence = PresenceStore::open(&presence_path).unwrap();
    let paths = ProjectPaths::at_root(&brain);
    let lock = ProjectLock::acquire(&paths, "rebuild", None).unwrap();
    let started = Instant::now();
    presence.record(announcement()).unwrap();
    let mut grants = MountGrants::default();
    grants.allow_read(MountLevel::Repo);
    mount::context(&brain, &grants, "presence lesson", 400).unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "context waited on the rebuild lock or on presence"
    );
    let body = std::fs::read_to_string(presence.path()).unwrap();
    assert!(body.contains(&"ab".repeat(32)));
    assert!(!body.contains("f3 presence lesson"));
    let (_, _, conn) = project::load_project_readonly(&brain).unwrap();
    let still: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memories WHERE memory_id = ?1",
            [id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(still, 1);
    drop(lock);
    drop(conn);
    let _ = std::fs::remove_dir_all(&brain);
    let _ = Path::new(&presence_path);
}
