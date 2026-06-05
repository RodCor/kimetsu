//! The remote tool allowlist: the pure-DB, agent-facing memory subset of the
//! full MCP catalog. Workdir-dependent tools (`ingest_repo`), host-local tools
//! (`bridge_*`, `plugin_install`, `skills_search`, `delegate`), and heavy/admin
//! ops (`reindex`, `model_set`) are intentionally excluded — those run on the
//! server via the `kimetsu` CLI, not over the network.

use std::collections::BTreeSet;
use std::sync::LazyLock;

/// Tools exposed over the remote HTTP MCP surface. Passed to
/// `kimetsu_chat::dispatch` so `tools/list` is filtered to these and
/// `tools/call` rejects anything else with a clear "not available in remote
/// mode" error.
pub static REMOTE_TOOLS: LazyLock<BTreeSet<&'static str>> = LazyLock::new(|| {
    [
        // retrieval / record / status
        "kimetsu_brain_status",
        "kimetsu_brain_insights",
        "kimetsu_brain_context",
        "kimetsu_brain_record",
        "kimetsu_brain_memory_search",
        // memory curation (pure-DB)
        "kimetsu_brain_memory_list",
        "kimetsu_brain_memory_top",
        "kimetsu_brain_memory_add",
        "kimetsu_brain_memory_proposals",
        "kimetsu_brain_memory_accept",
        "kimetsu_brain_memory_reject",
        "kimetsu_brain_memory_invalidate",
        "kimetsu_brain_memory_blame",
        "kimetsu_brain_memory_conflicts",
        "kimetsu_brain_conflict_resolve",
        "kimetsu_brain_prune",
        // read-only config + model listing
        "kimetsu_brain_config_show",
        "kimetsu_brain_model_list",
        // benchmark context (pure-DB)
        "kimetsu_benchmark_context",
        "kimetsu_benchmark_record_outcome",
    ]
    .into_iter()
    .collect()
});

/// The allowlist plus `kimetsu_brain_ingest_repo`, used when server-side ingest
/// is enabled (`--repos-file`). The ingest call is intercepted and handled by
/// the remote server (clone + ingest from the managed checkout), not the normal
/// dispatch.
pub static REMOTE_TOOLS_WITH_INGEST: LazyLock<BTreeSet<&'static str>> = LazyLock::new(|| {
    let mut set = REMOTE_TOOLS.clone();
    set.insert("kimetsu_brain_ingest_repo");
    set
});

/// Pick the active allowlist based on whether ingest is enabled.
pub fn allowlist(ingest_enabled: bool) -> &'static BTreeSet<&'static str> {
    if ingest_enabled {
        &REMOTE_TOOLS_WITH_INGEST
    } else {
        &REMOTE_TOOLS
    }
}
