//! A7 e2e: v1→v2 schema migration end-to-end — project brain.
//!
//! Verifies that an EXISTING populated project brain upgrades from v1 to v2
//! with a backup sidecar and data preserved, using the normal open path.
//!
//! Technique: (a) init a real project and record a memory (brain.db lands at
//! v2); (b) stamp the schema_info version back to 1 to simulate a pre-upgrade
//! DB; (c) re-open via `load_project` (which calls `schema::initialize` →
//! `run_migrations`); (d) assert current_version == 2, backup sidecar exists,
//! and the seeded memory survives.
//!
//! The user-brain migration is covered by a parallel unit test in
//! `kimetsu-brain/src/user_brain.rs` (see `migration_upgrades_user_brain`).

use kimetsu_brain::migrate;
use kimetsu_brain::project;
use kimetsu_brain::user_brain::with_user_brain_disabled;
use kimetsu_core::memory::{MemoryKind, MemoryScope};
use kimetsu_e2e::prelude::*;

/// Project brain: v1→v2 migration with a populated DB creates a backup
/// sidecar and preserves the seeded memory.
#[test]
fn project_brain_v1_to_v2_migration_creates_backup_and_preserves_data() {
    with_user_brain_disabled(|| {
        let project = TempProject::init("migration_project");

        // (a) Seed a memory — this creates brain.db and runs initialize()
        //     which leaves it at v2.
        let mem_id = project::add_memory(
            project.root(),
            MemoryScope::Repo,
            MemoryKind::Preference,
            "A7 migration test memory for project brain.",
        )
        .expect("add_memory");

        // Confirm the memory landed.
        let memories_before = project::list_memories(project.root()).expect("list before stamp");
        assert!(
            memories_before.iter().any(|m| m.memory_id == mem_id),
            "seeded memory must be present before stamp-down"
        );

        // (b) Stamp the version back to 1 to simulate a pre-upgrade DB.
        //     The table structure is already v2 (idempotent migration),
        //     so re-running the migration is a safe no-op on the DDL side
        //     but WILL write a new backup (row count > 0).
        {
            let conn = rusqlite::Connection::open(project.brain_db()).expect("open for stamp-down");
            conn.execute(
                "UPDATE schema_info SET value = 1 WHERE key = 'kimetsu_schema_version'",
                [],
            )
            .expect("stamp version back to 1");
            let stamped: i64 = conn
                .query_row(
                    "SELECT value FROM schema_info WHERE key = 'kimetsu_schema_version'",
                    [],
                    |r| r.get(0),
                )
                .expect("read stamped version");
            assert_eq!(stamped, 1, "version should be 1 after stamp-down");
        }

        // (c) Re-open through the normal path — load_project calls
        //     schema::initialize which calls run_migrations.
        let (_, _, conn) = project::load_project(project.root()).expect("load_project after stamp");

        // Assert 1: version is back at 2.
        let ver = migrate::current_version(&conn).expect("current_version");
        assert_eq!(ver, 2, "project brain must be at v2 after re-open");

        // Assert 2: backup sidecar exists next to brain.db.
        let brain_dir = project
            .brain_db()
            .parent()
            .expect("brain dir")
            .to_path_buf();
        let stem = project
            .brain_db()
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("brain.db")
            .to_string();
        let bak_prefix = format!("{stem}.bak-1-2-");
        let bak_files: Vec<_> = std::fs::read_dir(&brain_dir)
            .expect("read brain dir")
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .map(|n| n.starts_with(&bak_prefix))
                    .unwrap_or(false)
            })
            .collect();
        assert_eq!(
            bak_files.len(),
            1,
            "exactly one backup sidecar brain.db.bak-1-2-* should exist; found: {:?}",
            bak_files.iter().map(|e| e.file_name()).collect::<Vec<_>>()
        );

        // Assert 3: the seeded memory still lists after migration.
        let memories_after = project::list_memories(project.root()).expect("list after migration");
        assert!(
            memories_after.iter().any(|m| m.memory_id == mem_id),
            "seeded memory must survive the v1→v2 migration; mem_id={mem_id}"
        );
    });
}
