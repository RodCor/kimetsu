//! In-process store entry for Kimetsu Remote.
//!
//! HTTP MCP still serves the agent edge. Record, cite, and the other store
//! operations below call `kimetsu-sdk`, the same functions the CLI calls.
//! This module does not dial the bus and does not treat a bearer token as a
//! pack signer. Checkpoint limits stay in `projection` and `storage`.

use std::path::Path;

use kimetsu_brain::context::ContextBundle;
use kimetsu_brain::packs::{ImportSummary, MemoryExport, ScrubReport};
use kimetsu_brain::project::ProposeResult;
use kimetsu_core::KimetsuResult;
use kimetsu_core::memory::{MemoryKind, MountLevel};
use kimetsu_sdk::MountGrants;

/// Repo write, plus read of the user mount. Matches stdio MCP grants.
pub fn mcp_grants(write_tools: bool) -> MountGrants {
    MountGrants::stdio_mcp(write_tools)
}

pub fn context(
    start: &Path,
    grants: &MountGrants,
    query: &str,
    budget_tokens: u32,
) -> KimetsuResult<ContextBundle> {
    kimetsu_sdk::context(start, grants, query, budget_tokens)
}

pub fn record(
    start: &Path,
    grants: &MountGrants,
    level: MountLevel,
    kind: MemoryKind,
    text: &str,
    confidence: f32,
    session_id: Option<&str>,
) -> KimetsuResult<ProposeResult> {
    kimetsu_sdk::record(
        start,
        grants,
        level,
        kind,
        text,
        confidence,
        session_id,
    )
}

pub fn cite(
    start: &Path,
    grants: &MountGrants,
    exposure_id: &str,
    memory_id: &str,
    outcome: Option<&str>,
    note: Option<&str>,
) -> KimetsuResult<()> {
    kimetsu_sdk::cite(start, grants, exposure_id, memory_id, outcome, note)
}

pub fn search(
    start: &Path,
    grants: &MountGrants,
    query: &str,
    level_filter: Option<MountLevel>,
) -> KimetsuResult<ContextBundle> {
    kimetsu_sdk::search(start, grants, query, level_filter)
}

pub fn supersede(
    start: &Path,
    grants: &MountGrants,
    predecessor: &str,
    successor: &str,
) -> KimetsuResult<()> {
    kimetsu_sdk::supersede(start, grants, predecessor, successor)
}

pub fn export_pack(
    start: &Path,
    grants: &MountGrants,
    level: Option<MountLevel>,
) -> KimetsuResult<(Vec<MemoryExport>, ScrubReport)> {
    kimetsu_sdk::export_pack(start, grants, level)
}

pub fn import_pack(
    start: &Path,
    grants: &MountGrants,
    entries: &[MemoryExport],
    destination: MountLevel,
    replace: bool,
    quarantine: bool,
) -> KimetsuResult<ImportSummary> {
    kimetsu_sdk::import_pack(
        start,
        grants,
        entries,
        destination,
        replace,
        quarantine,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use kimetsu_brain::project;
    use kimetsu_brain::user_brain::with_user_brain_disabled;
    use kimetsu_core::paths::git_init_boundary;

    fn root(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "kimetsu-remote-store-{name}-{}-{}",
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

    #[test]
    fn remote_store_calls_the_sdk_for_each_op() {
        with_user_brain_disabled(|| {
            let grants = {
                let mut grants = MountGrants::default();
                grants.allow_write(MountLevel::Repo);
                grants
            };
            let start = root("ops");
            let first = record(
                &start,
                &grants,
                MountLevel::Repo,
                MemoryKind::Fact,
                "remote store lanternwick one",
                0.95,
                None,
            )
            .unwrap();
            let predecessor = match first {
                ProposeResult::Added(id) => id,
                other => panic!("{other:?}"),
            };
            assert!(!context(&start, &grants, "lanternwick", 400)
                .unwrap()
                .capsules
                .is_empty());
            assert!(!search(&start, &grants, "lanternwick", None)
                .unwrap()
                .capsules
                .is_empty());
            let successor = match record(
                &start,
                &grants,
                MountLevel::Repo,
                MemoryKind::Fact,
                "remote store lanternwick two",
                0.95,
                None,
            )
            .unwrap()
            {
                ProposeResult::Added(id) => id,
                other => panic!("{other:?}"),
            };
            supersede(&start, &grants, &predecessor, &successor).unwrap();
            let (rows, _) = export_pack(&start, &grants, Some(MountLevel::Repo)).unwrap();
            let other = root("import");
            let summary =
                import_pack(&other, &grants, &rows, MountLevel::Repo, false, false).unwrap();
            assert!(summary.imported >= 1);
            assert!(cite(&start, &grants, "no-exposure", &successor, Some("noop"), None).is_err());
        });
    }
}
