//! In-process entry for the ADR 0001 store operations and `request_pack`.
//!
//! `kimetsu-cli` and `kimetsu-remote` call the store functions. Those bodies
//! stay in `kimetsu-brain`: this crate does not open a second database and
//! does not verify pack signatures on its own. With the `bus` feature,
//! `request_pack` forwards to `kimetsu-bus`, which imports through
//! [`import_signed_pack`]. Remote does not enable that feature.
//!
//! `@kimetsu-ai/sdk` is a different package. It is an HTTP client of Remote's
//! MCP surface and does not link this crate.

use std::path::Path;

use kimetsu_brain::context::ContextBundle;

/// Floors on a context payload, and the tool JSON those floors deserialize from.
///
/// [`ContextToolResult::diagnostics`] is optional: payloads from before the
/// field existed, and payloads that dropped it to stay inside the output
/// budget, still deserialize. [`context`] returns [`ContextBundle`], which
/// carries the same object after retrieval resolves the floors.
pub use kimetsu_brain::context::{ContextDiagnostics, ContextToolResult};
use kimetsu_brain::packs::{ImportSummary, MemoryExport, ScrubReport};
use kimetsu_brain::project::ProposeResult;
use kimetsu_brain::signed_pack::{self, PackManifest, TrustStore};
use kimetsu_core::KimetsuResult;
use kimetsu_core::memory::{MemoryKind, MountLevel};

pub use kimetsu_brain::mount::MountGrants;

/// `context`. Lexical read of the mounts in `grants`. No network.
pub fn context(
    start: &Path,
    grants: &MountGrants,
    query: &str,
    budget_tokens: u32,
) -> KimetsuResult<ContextBundle> {
    kimetsu_brain::mount::context(start, grants, query, budget_tokens)
}

/// `record`. One write on `level`.
pub fn record(
    start: &Path,
    grants: &MountGrants,
    level: MountLevel,
    kind: MemoryKind,
    text: &str,
    confidence: f32,
    session_id: Option<&str>,
) -> KimetsuResult<ProposeResult> {
    kimetsu_brain::mount::record(start, grants, level, kind, text, confidence, session_id)
}

/// `cite`. `outcome` is `helped`, `noop`, or `harmed`. Omitted is `noop`.
pub fn cite(
    start: &Path,
    grants: &MountGrants,
    exposure_id: &str,
    memory_id: &str,
    outcome: Option<&str>,
    note: Option<&str>,
) -> KimetsuResult<()> {
    kimetsu_brain::mount::cite(start, grants, exposure_id, memory_id, outcome, note)
}

/// `search`. A level filter may only narrow `grants`.
pub fn search(
    start: &Path,
    grants: &MountGrants,
    query: &str,
    level_filter: Option<MountLevel>,
) -> KimetsuResult<ContextBundle> {
    kimetsu_brain::mount::search(start, grants, query, level_filter)
}

/// `supersede`. One predecessor, one successor, one mount.
pub fn supersede(
    start: &Path,
    grants: &MountGrants,
    predecessor: &str,
    successor: &str,
) -> KimetsuResult<()> {
    kimetsu_brain::mount::supersede(start, grants, predecessor, successor)
}

/// `export_pack`. Unsigned rows. Run and session stay out.
pub fn export_pack(
    start: &Path,
    grants: &MountGrants,
    level: Option<MountLevel>,
) -> KimetsuResult<(Vec<MemoryExport>, ScrubReport)> {
    kimetsu_brain::mount::export_pack(start, grants, level)
}

/// `import_pack` for rows the caller already parsed. v1 local files.
pub fn import_pack(
    start: &Path,
    grants: &MountGrants,
    entries: &[MemoryExport],
    destination: MountLevel,
    replace: bool,
    quarantine: bool,
) -> KimetsuResult<ImportSummary> {
    kimetsu_brain::mount::import_pack(start, grants, entries, destination, replace, quarantine)
}

/// Signed `export_pack`. Scrub finishes before the signature.
#[allow(clippy::too_many_arguments)]
pub fn export_signed_pack(
    start: &Path,
    grants: &MountGrants,
    level: MountLevel,
    redact: bool,
    redact_tags: bool,
    manifest: &PackManifest,
    strict: bool,
    signer: &signed_pack::PackSigner,
) -> KimetsuResult<(Vec<u8>, ScrubReport)> {
    kimetsu_brain::mount::export_signed_pack(
        start,
        grants,
        level,
        redact,
        redact_tags,
        manifest,
        strict,
        signer,
    )
}

/// `request_pack`. The bus dials, verifies, then calls [`import_signed_pack`].
#[cfg(feature = "bus")]
pub async fn request_pack(
    endpoint: &kimetsu_bus::BusEndpoint,
    request: kimetsu_bus::PackRequest,
) -> Result<ImportSummary, String> {
    endpoint.request_pack(request).await
}

/// Signed `import_pack`. Signature and scope checks finish before SQL.
pub fn import_signed_pack(
    start: &Path,
    grants: &MountGrants,
    bytes: &[u8],
    destination: MountLevel,
    replace: bool,
    quarantine: bool,
    trust: &TrustStore,
) -> KimetsuResult<ImportSummary> {
    kimetsu_brain::mount::import_signed_pack(
        start,
        grants,
        bytes,
        destination,
        replace,
        quarantine,
        trust,
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
            "kimetsu-sdk-{name}-{}-{}",
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

    fn repo_write() -> MountGrants {
        let mut grants = MountGrants::default();
        grants.allow_write(MountLevel::Repo);
        grants
    }

    #[test]
    fn seven_ops_share_one_store() {
        with_user_brain_disabled(|| {
            let first = root("a");
            let grants = repo_write();
            let added = record(
                &first,
                &grants,
                MountLevel::Repo,
                MemoryKind::Fact,
                "sdk lanternwick alpha",
                0.95,
                None,
            )
            .unwrap();
            let predecessor = match added {
                ProposeResult::Added(id)
                | ProposeResult::Merged(id)
                | ProposeResult::Duplicate(id) => id,
                ProposeResult::Proposed(id) => panic!("high confidence must accept, got {id}"),
            };
            let bundle = context(&first, &grants, "lanternwick", 800).unwrap();
            assert!(
                bundle
                    .capsules
                    .iter()
                    .any(|capsule| capsule.summary.contains("lanternwick")),
                "context did not read the recorded row"
            );
            let found = search(&first, &grants, "lanternwick", Some(MountLevel::Repo)).unwrap();
            assert!(!found.capsules.is_empty());
            let successor = match record(
                &first,
                &grants,
                MountLevel::Repo,
                MemoryKind::Fact,
                "sdk lanternwick beta",
                0.95,
                None,
            )
            .unwrap()
            {
                ProposeResult::Added(id) => id,
                other => panic!("expected a new row, got {other:?}"),
            };
            supersede(&first, &grants, &predecessor, &successor).unwrap();
            let (rows, _scrub) = export_pack(&first, &grants, Some(MountLevel::Repo)).unwrap();
            assert!(rows.iter().any(|row| row.text.contains("beta")));

            let second = root("b");
            let summary =
                import_pack(&second, &grants, &rows, MountLevel::Repo, false, false).unwrap();
            assert!(summary.imported + summary.deduped >= 1);

            let err = cite(
                &first,
                &grants,
                "missing-exposure",
                &successor,
                Some("helped"),
                None,
            )
            .unwrap_err()
            .to_string();
            assert!(
                err.contains("exposure") || err.contains("not"),
                "cite without a delivery must fail, got {err}"
            );
        });
    }
}
