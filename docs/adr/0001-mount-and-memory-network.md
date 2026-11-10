# ADR 0001 — Mount contract and memory network

- **Status:** Implemented on main, with the gaps listed in this header. The sections below stay the contract.
- **Date:** 2026-11-05
- **Scope:** The phases below are what main actually contains. This file adds no benchmark number. The two-physical-host LAN row and the WAN row stay pending.

Kimetsu is a sidecar brain: one Rust binary, one SQLite file per project, delivered to a coding agent over MCP. This ADR names the mount contract, the store operations that respect it, signed packs, and a peer-to-peer bus that moves packs. The bus is inspired by the transport shape of [Reason Network](https://github.com/reason-machines/reason-network/blob/main/DESIGN.md). It is not that system, and it does not carry chat.

Landed on main. Each line is a file or test that exists in the tree:

- **F0 — local mounts and the seven operations.** `crates/kimetsu-brain/src/mount.rs` (`context`, `record`, `cite`, `search`, `supersede`, `export_pack`, `import_pack`). Levels are `MountLevel` and `MemoryScope` in `crates/kimetsu-core/src/memory.rs`. Contamination tests in `mount.rs`: `merge_refuses_cross_mount_and_alias_collapse`, `replace_retires_repo_alias_and_leaves_user`, `replay_preserves_session_level`, `warm_start_keeps_session_off_the_repo_digest`, `cite_helped_raises_usefulness_without_changing_level`. MCP payload: `f0_repo_read_context_excludes_user_and_org_text` in `crates/kimetsu-chat/src/mcp_server.rs`.
- **F1 — signed packs.** `crates/kimetsu-brain/src/signed_pack.rs` (`kimetsu_pack` 2, BLAKE3, Ed25519). Tests `canonical_profile_is_pinned`, `tamper_scope_memory_reorder_and_strip_signature_reject_with_no_sql`, and `trusted_signer_merges_and_untrusted_signer_only_proposes`.
- **F2 — bus.** `crates/kimetsu-bus` (`src/lib.rs`, `src/endpoint.rs`, `src/protocol.rs`). Exit tests in `crates/kimetsu-bus/tests/f2_exit.rs`: `direct_announce_request_verifies_and_imports`, `non_member_handshake_fails_and_is_not_dialed`, `scope_mismatch_does_not_write`.
- **Invitations.** `crates/kimetsu-bus/src/roster.rs`. Tests in `crates/kimetsu-bus/tests/invitation.rs`: `invitation_converges_and_then_a_signed_pack_imports`, `removal_reissue_blocks_request_pack_and_the_next_bind`, `same_generation_fork_and_other_group_do_not_replace_the_roster`.
- **F4 — host adapters.** `crates/kimetsu-chat/src/host_f4.rs`: `four_hosts_generate_stdio_mcp_and_drive_context_record_cite`, `grok_bot_remote_snippet_names_the_http_endpoint_and_no_config_file`.
- **F5 — federated supersede.** `crates/kimetsu-brain/src/federated.rs`. Two peers and one conflict row: `peers_exchange_disagreeing_supersedes_and_record_one_conflict` in `crates/kimetsu-bus/tests/f5_supersede.rs`.
- **Directory sync (F6), the offline complement of the bus.** `crates/kimetsu-brain/src/sync.rs`. Tests `directory_import_refuses_a_pack_before_sql`, `directory_import_refuses_a_gzip_pack_file`, `same_supersede_pair_does_not_write_a_second_event`, `two_directory_successors_still_record_one_conflict`. A JSONL batch is refused by `verify_and_v1_parser_refuse_a_directory_sync_batch` in `crates/kimetsu-brain/src/signed_pack.rs`.

Partial. These are not closed:

- **F3 — budgets, Remote alignment, and the Rust SDK.** The in-process store entry is `crates/kimetsu-sdk/src/lib.rs`. `kimetsu-remote` calls the store functions from `crates/kimetsu-remote/src/store.rs` and does not enable the `bus` feature (`crates/kimetsu-remote/Cargo.toml`). The namespace-to-roster note is `docs/operators/remote-namespace-roster.md`. Warm `context`, presence, loopback, and veth samples live in `docs/adr/0001-f3-measurements.md`. That file's two-physical-host LAN row and its WAN row have no number. They stay pending. Loopback and the veth pair are not the [§7](#7-performance) LAN figure, and no number from that file is copied here.
- **CLI `request_pack`.** Pending on a separate branch, not done. `crates/kimetsu-cli/src/network.rs:560` calls `BusEndpoint::request_pack` directly. It does not call `kimetsu_sdk::request_pack`.

How to read the rest of the document:

- Paragraphs marked **Today**, and the Today column, are the baseline this decision was written against. A sentence that cited a current source file for a fact main no longer matches is corrected in place, with that file named.
- **MUST** / **MUST NOT** remain the contract.
- This file does not record a two-host LAN or WAN `request_pack` number. Those rows stay pending in [§7](#7-performance). Numbers that already exist stay in [0001-f3-measurements.md](0001-f3-measurements.md).

## Context

The pieces this contract has to name already exist. They do not share one definition of level, grant, or citation outcome.

### Brains and scopes

**Today** a project brain is one SQLite file. A second database, the user brain, holds cross-project preferences under `~/.kimetsu/` (`crates/kimetsu-core/src/paths.rs`). The baseline `MemoryScope` had four variants. Main adds `Session`, `Org`, and `Skill` (`crates/kimetsu-core/src/memory.rs:108`, `crates/kimetsu-core/src/memory.rs:109`, `crates/kimetsu-core/src/memory.rs:110`):

| Rust variant | Wire string | Accepted aliases |
| --- | --- | --- |
| `GlobalUser` | `global_user` | `global-user`, `user` |
| `Project` | `project` | — |
| `Repo` | `repo` | `repository` |
| `Run` | `run` | — |
| `Session` | `session` | — |
| `Org` | `org` | — |
| `Skill` | `skill` | — |

`@kimetsu-ai/sdk` lists those wire strings, including `session`, `org`, and `skill` (`npm/kimetsu-sdk/src/types.ts:89`). Entries whose scope is `GlobalUser` are routed to the user-brain database; they are not rows in the project file (`crates/kimetsu-brain/src/project.rs`). `project` and `repo` can both remain stored strings until a durable migration. `project` is an input alias of the `repo` mount (`crates/kimetsu-core/src/memory.rs:46`). A new event from `MemoryScope::Project` stores `repo` (`crates/kimetsu-core/src/memory.rs:127`).

`Session`, `Org`, and `Skill` are `MemoryScope` variants on main (`crates/kimetsu-core/src/memory.rs`). What each one holds:

- **Session.** Warm-start puts task titles and progress only in the caller-selected task/session/worktree resume lane, and keeps them out of the shared repo digest (`docs/warm-start-contract.md`). The agent-memory audit found one live episode per repo root, with no task/session/branch/commit key on that payload (`docs/audits/2026-09-04-agent-memory-audit.md`, finding 9). Main keeps one live episode per repo and explicit identity lane (`crates/kimetsu-brain/src/episode.rs:7`).
- **Org.** Kimetsu Remote can store one org checkpoint shared by every repo in a namespace when `--shared-org-brain` / `KIMETSU_REMOTE_SHARED_ORG` is on. Repo tokens may retrieve it. Writing it, and resolving conflicts that can touch it, requires a global token (`docs/remote-storage.md`).
- **Skill.** A memory becomes a synthesis candidate at `CITATION_THRESHOLD` (3) distinct-run `memory.cited` events, or as a member of a tag cluster of at least 3. Detection is a query, not a model call. Acceptance installs a skill; a skill is stale when any source memory is superseded or invalidated (`crates/kimetsu-brain/src/skill_synthesis.rs`). That is a proposal table plus a `SKILL.md` on disk, not a memory scope.

The 2026-09-04 models audit showed why an unnamed level is unsafe. Consolidation joins memories above cosine 0.92 by embedding-model identity and does **not** require the same scope or kind. Union-find is not transitive: adjacent pairs at 0.92 can pull in a third memory the survivor would not match, and the survivor's scope wins (`docs/audits/2026-09-04-models-parameters-memory.md`). That is the failure this ADR calls contamination.

### Packs, sync, and Remote

**Today** `export_memories` writes `MemoryExport { text, scope, kind, confidence, created_at }` and deliberately drops `memory_id`, `usefulness_score`, and `use_count`, so an import creates a fresh row (`crates/kimetsu-brain/src/packs.rs`). The envelope is `Pack { kimetsu_pack: 1, name, version, description, exported_at, memory_count, memories }`, JSON then gzip. A bare `Vec<MemoryExport>` still parses. Export scrub runs on the copy; the source database is untouched. `--strict` aborts when the scrub report is non-empty (README, `ScrubReport`).

`import_pack` merge is additive and dedups on normalized text. `replace: true` invalidates active rows in the pack's target scopes (`invalidate_memory`, reason `replaced_by_pack:<name>@<version>`), keeps the events, then loads the pack. The README describes that path as supersede-rather-than-delete so a swap can be undone. The code path is invalidation, not a `superseded_by` link to the new row. The CLI treats `replace` and `quarantine` as mutually exclusive, because retiring local rows in favor of content that has not been reviewed is the worst mix; `import_pack` itself still invalidates and then quarantines if both flags are passed. This contract forbids that combination. Quarantine writes `memory.proposed` into the existing review queue and does not enter retrieval. A citation cannot release quarantine, because a memory outside retrieval cannot be cited (`quarantine_memories` in the same file). Import scrubs again.

`kimetsu brain sync` replicates the event log through a directory, with no server (`crates/kimetsu-brain/src/sync.rs`). The durable allowlist is `memory.accepted`, `memory.proposed`, `memory.rejected`, `memory.invalidated`, `memory.restored`, `memory.corrected`, `memory.cited`, `memory.superseded`. Excluded on purpose: `work.episode`, `context.served`, `retrieval.regret`, `digest_served`, `resume_served`, `context.injected`, and every `run.*` kind. Idempotency is `event_id` via `INSERT OR IGNORE`. `docs/memory-maintenance.md` adds the compact rule: `brain compact` may drop only `context.served`, `retrieval.stats`, `digest_served`, and `resume_served`. Claim, correction, exposure, citation, outcome, and unknown kinds stay. Projection rebuild takes the SQLite writer lock before reading the log and holds it through reset and replay.

Kimetsu Remote is a different mechanism. Each HTTP request loads repo and optional org checkpoints, builds a disposable SQLite projection, runs one allowlisted tool, and publishes the changed checkpoints against expected revisions (`docs/remote-storage.md`). The SQLite adapter uses WAL. Checkpoints are bounded to 64 MiB compressed, 256 MiB decoded, and 2,000,000 rows. Format 1 must match the binary's brain schema. `@kimetsu-ai/sdk` is the typed TypeScript client of that HTTP MCP surface (`npm/kimetsu-sdk/README.md`, `npm/README.md`). It has zero runtime dependencies and does not open SQLite.

None of the three sharing paths signs the set of mounts a pack may enter. None of them is a presence bus. Managed ingest (`--repos-file`, `--checkout-dir`) checks out repository trees as operational data of the Remote process. That checkout is not a memory pack.

### Retrieval, warm-start, and citations

**Today** the default memory pipeline is local. The README states that storage and retrieval make no LLM call unless the operator opts into `kimetsu ask`, a distiller, or the decision-provider pass. The agent-memory audit sharpens the wording: embeddings and a cross-encoder are local inference, not "model-free"; "no generative LLM" is the accurate claim (`docs/audits/2026-09-04-agent-memory-audit.md`, "What 0 tokens can actually mean").

Warm-start (`docs/warm-start-contract.md`) assembles a bounded digest with no model and no network. A cached text match avoids a redundant disk write and never replaces the freshly assembled result. Automatic injection requires a candidate to cover at least half of the turn's content terms, admitted text to cover at least 60% collectively, and suppresses the entire injection when uncertainty (normalized Shannon score entropy times the score-weighted unsupported fraction) is above 0.35. Those thresholds are lexical heuristics, not calibrated probabilities. Dates and tag prefixes are not topical evidence. Missing intent, invalid scores, insufficient support, or excessive uncertainty produce empty output: no partial-evidence warning and no startup digest. Explicit MCP retrieval keeps partial-coverage reporting. Warm text is not treated as verified, and it does not receive fabricated capsule outcome credit. Stdio MCP remembers delivered warm-start keys for the latest 256 workspace/identity pairs; keys over 4096 combined path/identity units are not cached.

`kimetsu_brain_context` is the MCP read (`crates/kimetsu-chat/src/mcp_server.rs`). `kimetsu_brain_record` writes `repo` through `kimetsu_sdk::record` (kind mapped from `anti_pattern` → `FailurePattern`, `convention` → `Convention`, otherwise `Fact`, default confidence 0.8) (`crates/kimetsu-chat/src/mcp_server.rs:1116`). `kimetsu_brain_cite` requires `memory_id` and `exposure_id`, and takes an optional `note` and an optional `outcome` (`helped`, `noop`, or `harmed`; omitted is `noop`). It writes `memory.cited`. The tool's own usage string says reliance was recorded and that this is not verification. The server instructions repeat that: "Citations record reliance, not verification."

Usefulness follows the cite outcome. `helped` adds `CITED_DELTA` (+1.0) when the cite is projected. `noop` and an omitted outcome add nothing. `harmed` subtracts `CITED_DELTA` and sets `invalidated_reason` to `cite_harmed`. On `run.finished`, a silent passenger stays at +0.0. On `run.failed` outside category `Gate`, a cited memory takes `-CITED_DELTA / (1 + prior_citations / FAILURE_PENALTY_CITES_DIVISOR)` with divisor 3.0, and a silent passenger takes `-PASSENGER_DELTA` (−0.1). `run.aborted` and Gate failures do not move usefulness (`crates/kimetsu-brain/src/projector.rs:464`, `crates/kimetsu-brain/src/projector.rs:469`, `crates/kimetsu-brain/src/projector.rs:706`, `crates/kimetsu-brain/src/projector.rs:795`, `crates/kimetsu-brain/src/scoring.rs:39`). Usefulness decay is `exp(-ln(2) * age_days / half_life)` from `last_useful_at`, else `created_at`, default half-life 30 days (`crates/kimetsu-core/src/config.rs`). The broker turns the score into a capped multiplier; decay is not a license to change scope.

The agent-memory audit, commit `3ec56a8`, finding 6, is the reason a bare cite cannot stay a success signal: an MCP citation becomes a positive outcome with a fresh group id and no query, and trust treats `last_useful_at` as corroboration. No independent test is required. The same audit's finding 1 is in force for any new read path: `kimetsu_brain_context` was observed serializing excluded capsules into the model-visible payload, so a larger rejected pool grew the tool response. Finding 2: `edit_memory` updated the projection without a durable correction event, and rebuild restored the old text. A mount level that lives only in the projection will not survive replay.

Local stdio MCP enables recording unless `kimetsu.mcp_write_tools` is turned off. Remote write tools stay off unless the server process has `KIMETSU_MCP_ENABLE_WRITE_TOOLS=1`. A healthy empty deployment can serve reads and offer no writes (`docs/remote-storage.md`, `crates/kimetsu-chat/src/mcp_server.rs`).

### What is missing

One rule that says which mount a byte belongs to, which grant was required to put it there, and which citation outcome is allowed to raise its usefulness. Sharing then has to carry that rule inside the signature, and a bus has to move the signed pack without cloning the repository or blocking recall on presence.

## Decision

Adopt six mounts, seven store operations, signed packs whose scopes are inside the signature, and a Rust store+bus SDK. MCP and `@kimetsu-ai/sdk` stay the agent-facing edge. Kimetsu Remote stays an HTTP checkpoint server of the same mounts. Directory sync is the offline event-log complement. The bus carries signed packs. The split is [Directory sync and the bus](#directory-sync-and-the-bus).

### 1. Mount contract

A mount is the triple `(level, identity, grant)`.

| Level | Identity | What lives here | What MUST NOT live here | Lifetime | Who opens the grant |
| --- | --- | --- | --- | --- | --- |
| `run` | one agent invocation (`RunId`) | run telemetry and the episode slice for that invocation | any byte that `export_pack` or directory sync would ship | the run | the CLI process or the MCP request that started the run |
| `session` | task / session / worktree, not merely the repo root | resume lane: task title, progress, open thread, dead ends for that identity | the shared repo digest; another task's resume in the same checkout | while that session identity is current; survives later runs of the same identity | the host session. `session_id` and `task_id` already ride context-exposure payloads (`record_context_delivery` in `mcp_server.rs`). The episode projection keys on that identity lane (`crates/kimetsu-brain/src/episode.rs:7`) |
| `repo` | one repository (the project `brain.db`) | project conventions, failure patterns, commands, facts, repo manifests | another repo's rows; the person's global preferences; org policy | the project database | workspace discovery. Local CLI and stdio MCP open this file |
| `org` | one trust group, equal to one Remote namespace when Remote is the store | the shared org brain for every repo in that namespace | any other namespace; a single repo's unreviewed scratch | the org checkpoint / namespace | Remote, when shared org is enabled. A roster entry may name the same group later; until an operator binds them, namespace and roster are different boundaries |
| `user` | one person (`~/.kimetsu/` user brain) | cross-project preferences, including project and environment opt-outs | org policy; a repo's task state | the user-brain file | the user-brain opener. Warm-start opens it read-only |
| `skill` | one graduated skill (proposal id / skill name) | the source lessons that justify that skill, with their memory ids as provenance | the host `SKILL.md` body; chat transcripts; a skill whose sources were superseded, presented as current | until the skill is rejected or every source is superseded or invalidated (`StalenessReport`) | `kimetsu brain skills` accept, or an `import_pack` whose signed scopes include `skill` |

`write` includes `read`. A mount that was not opened is neither readable nor writable. The grant is a property of the handle. It MUST NOT be inferred from the text of a memory, from a tag, or from cosine similarity.

#### Grants today and under this contract

| Opener | Today | This contract |
| --- | --- | --- |
| Local CLI, operator | The process can read and write the project brain and, with the user-brain path, the user brain. Warm-start's automatic opener of the user brain is read-only (`docs/warm-start-contract.md`). | The handle lists grants. An operator shell MAY hold `write` on `repo` and `user`. The warm-start opener MUST hold only `read` on `user`. `org` stays closed unless a Remote token or a roster entry opens it. `skill` write is the skills accept/import path, not an implied CLI default. |
| Stdio MCP session | Writes go through `is_privileged_write_tool`. Local default is on, via `kimetsu.mcp_write_tools`. `KIMETSU_MCP_ENABLE_WRITE_TOOLS` overrides and is required for remote. `kimetsu_brain_record` hardcodes `MemoryScope::Project`. Cite writes `memory.cited` against an `exposure_id`. | Read `repo`. Read `user` only through the read-only opener. Read `skill` when a skill's sources are current. `write` on `repo` only while write tools are enabled. No `write` on `user`, `org`, or `skill` from `kimetsu_brain_record`. A session-scoped write MUST name `session` and MUST NOT land in `repo`. |
| Remote repo token | Bearer token bound to one repo (`--tokens-file` `[per_repo]`). May retrieve the shared org brain. MUST NOT write org or shared user memory. Write tools off unless `KIMETSU_MCP_ENABLE_WRITE_TOOLS=1` (`docs/remote-storage.md`). | With write tools on: `write` on that `repo`, `read` on `org` when sharing is on. Without write tools: `read` only. No `user` mount. No `skill` write. |
| Remote global token | Required for shared org/user-memory writes and for `kimetsu_brain_conflict_resolve`, including project resolutions, because that path can affect inherited org conflicts. | `write` on `org` in that namespace. Repo write only where the token is also authorized for the repo. Conflict resolution MUST declare the mounts it can touch; a project-looking resolve that edits org state requires the org write grant. |
| Bus peer | The baseline had none. The crate on main is `kimetsu-bus` (`crates/kimetsu-bus/src/lib.rs`). | The signed roster entry lists the mounts that endpoint may serve or request. An announcement is not a grant. `request_pack` still checks the destination write grant on the receiver. |

A repo token that writes the org mount is a bug. A stdio record that writes `global_user` because the lesson "looks like a preference" is a bug. `kimetsu_brain_record` is a `write` on `repo` and reports level `repo` (`crates/kimetsu-chat/src/mcp_server.rs:1116`, `crates/kimetsu-chat/src/mcp_server.rs:1126`). Callers may still send `project`. A new event stores `repo` (`crates/kimetsu-core/src/memory.rs:127`).

#### Mapping from `MemoryScope`

| Today | Mount under this contract | Rule |
| --- | --- | --- |
| `run` | `run` | Same identity. Still excluded from packs and from directory sync. |
| `project` | `repo` | Input alias only, until an explicit migration. Callers, the CLI, and `@kimetsu-ai/sdk` may still send `project`. |
| `repo` | `repo` | Already the canonical name. |
| `global_user` / `user` | `user` | Same database. The alias `user` already parses. |
| `session` | `session` | `MemoryScope::Session` stores `session` in the project database (`crates/kimetsu-brain/src/project.rs:815`). The episode projection is one live episode per repo and identity lane (`crates/kimetsu-brain/src/episode.rs:7`). |
| `org` | `org` | `MemoryScope::Org` stores `org` (`crates/kimetsu-core/src/memory.rs:135`). The Remote org brain is still a checkpoint selected by namespace and flag (`docs/remote-storage.md`). |
| `skill` | `skill` | `MemoryScope::Skill` stores `skill` (`crates/kimetsu-core/src/memory.rs:136`). `MemoryKind` in Rust has no `skill` variant (`Preference`, `Convention`, `Command`, `FailurePattern`, `Fact`). The TypeScript client's `MemoryKind` includes `'skill'`; that kind string is not a mount. |

Alias window, which F0 MUST implement before any rewrite of old rows:

- New events written by the contract store the level `repo`, never a second level named `project`.
- Retrieval for the `repo` mount reads rows whose stored scope is `repo` or `project`, and labels every returned record `repo`.
- The two stored strings stay distinct until the migration. A similarity merge MUST NOT collapse a `project` row into a `repo` row, or the reverse, as a side effect of the alias. They are the same mount for grants and for contamination, and they remain two projection rows until replay rewrites them.
- The migration is a durable event (or a versioned rebuild that replays from events), not an `UPDATE` of the projection alone. The audit's edit/rebuild failure is the reason: a projection-only edit reverts (`docs/audits/2026-09-04-agent-memory-audit.md`, finding 2).

`session`, `org`, and `skill` MUST appear on the durable event before any code path treats them as scopes. Adding them only to `MemoryScope`'s `Display` without writing the level into `memory.accepted` / `memory.cited` / `memory.superseded` is not an implementation of this contract.

#### Contamination

Contamination is a bug. It has three forms:

1. An accepted memory is readable from a mount other than its home mount, and no `record` or `import_pack` on the destination wrote it there.
2. A read returns a memory whose mount the handle does not have `read` on.
3. A write, including usefulness updates that change level, touches a mount the handle does not have `write` on.

Invariants:

- Home mount is fixed at `record` or `import_pack` time and is copied from the durable event on replay. Replay MUST NOT infer the level from prose, from the database file that happens to be open, or from the writer that holds the lock.
- Promote and decay change usefulness inside the home mount. They MUST NOT change level.
- Cosine merge, graph consolidation, and distiller merge MUST refuse pairs whose mounts differ. The 0.92 union-find path that ignores scope (`docs/audits/2026-09-04-models-parameters-memory.md`) does not satisfy this invariant.
- Warm-start's repo digest and the session resume lane MUST NOT mix. A cache hit MUST NOT publish a previous generation into the model (`docs/warm-start-contract.md`).
- The user-brain opener used by warm-start is `read` on `user`. It MUST NOT write.
- `run` events and `work.episode` stay out of packs and out of directory sync, matching `SYNC_ALLOWED_KINDS` (`crates/kimetsu-brain/src/sync.rs`).
- Quarantine proposals are not accepted memories. They MUST NOT appear in `context`, `search`, or warm-start.
- A pack entry's `scope` MUST be an element of the pack's signed scope list. A destination mount outside that list MUST NOT receive the row.

Required tests, all of which fail the build if the cross-mount case succeeds:

| Test | Setup | Pass condition |
| --- | --- | --- |
| Merge | Two active memories, cosine above 0.92, mounts `repo` and `user` | Both rows remain. Neither inherits the other's level. |
| Replace | `import_pack` with `replace` on a pack signed for `repo`, while the brain also has `user` rows | Only `repo` (and alias-window `project`) rows are retired. `user` rows stay active. |
| Replay | `record` at `session`, rebuild under the writer lock | The reconstructed row is `session`. A second run's row is not. |
| Warm-start | Repo fact plus a session title plus a user preference | Digest contains the repo fact. Session title stays on the resume lane. User preference appears only via the read-only opener. |
| MCP | Handle with `read` on `repo` only; brain also has `user` and `org` rows | `kimetsu_brain_context` payload contains no `user` or `org` text, including excluded-capsule summaries (audit finding 1). |
| Bus | `request_pack` whose signature lists `repo`, destination grant is `write` on `org` | Import is rejected. The org mount is unchanged. No quarantine row is created for a scope mismatch. |
| Cite | `cite(helped)` on a `repo` memory | Usefulness may rise. Level stays `repo`. |

A test that asserts the cross-mount result is a broken test.

#### Level changes

Moving a lesson up a level is a new write, with `write` on the destination.

| From | To | Legal path |
| --- | --- | --- |
| `session` | `repo` | `record` or `import_pack` on `repo`. The session row stays the session row. |
| `repo` | `org` or `user` | `record` or `import_pack` on that destination. A repo token cannot do the org write. |
| cited lesson | `skill` | `record` or `import_pack` on `skill`, after a human accept or an equivalent explicit install. The third citation (`CITATION_THRESHOLD`) MAY mark a candidate. It MUST NOT copy the lesson into the skill mount by itself. |

`run` does not promote into `session`. Compaction of expendable telemetry (`docs/memory-maintenance.md`) is not a level change.

### 2. Store operations

The store surface is these seven operations. Host-only tools (`kimetsu ask`, distiller, decision providers, skill drafting) are outside the surface and outside the default path.

Evidence-first, default path: `context` and warm-start MUST NOT call a generative LLM. Embeddings and the reranker, when the build includes them, are local opt-in inference and are outside the hot-path budget in [§7](#7-performance). Empty support yields empty output on the automatic path. Explicit `search` MAY report partial coverage, as explicit MCP retrieval does today (`docs/warm-start-contract.md`).

#### `context`

| | |
| --- | --- |
| Inputs | query string; the open read grants; optional `session_id`, `task_id`, token budget. Same arguments the MCP tool already threads into exposure payloads. |
| Reads | every mount the handle has `read` on. Each returned record carries its level. |
| Writes | an exposure / `context.injected` (or the successor event) for what was actually delivered. That event is telemetry-class for sync purposes and MUST stay on the sync denylist. It MUST record the ids that were delivered, because the audit found credit given to memories the agent never saw. |
| Output | a bounded bundle: selected evidence, stable ids, level, validity, provenance. `skipped: true` when nothing qualifies. |
| Empty | automatic path returns no capsules, no partial-evidence warning, and no startup digest when term coverage, the 60% collective bar, or uncertainty above 0.35 fails. Explicit MCP `search` is the path that may explain a partial. |
| Errors | unknown mount in the grant set; budget that cannot be applied to the final payload. A missing brain is an error payload, not an invented memory. |
| Idempotency | a repeated `context` with the same query MAY return a new exposure id. It MUST NOT create memories. |
| LLM | none on this path. |

The model-visible payload MUST NOT grow because the rejected pool grew. Excluded text stays off the default response (audit finding 1). Warm-start credit is recorded only after the block is actually emitted (`docs/warm-start-contract.md`).

#### `record`

| | |
| --- | --- |
| Inputs | destination mount; kind (`MemoryKind`); text; optional confidence; optional tags. |
| Grant | `write` on that exact mount. |
| Writes | one durable lifecycle event (`memory.accepted`, or `memory.proposed` when the existing confidence gate defers acceptance) carrying the level. The projection, FTS, and any embedding row are derived. |
| Output | `added`, `merged`, `duplicate`, or `proposed`, plus the id. Same result vocabulary as `kimetsu_brain_record` today, plus the level. |
| Merge | allowed only into an active row of the same mount and a compatible kind. Cross-mount merge is contamination. |
| Errors | missing text; unknown kind; mount not writable; level absent from the event the implementation tried to append. |
| Idempotency | a second `record` of the same normalized text (`normalize_memory_text`) on the same mount returns the existing id and `duplicate`. It MUST NOT mint a second accepted row. |
| Default MCP | `kimetsu_brain_record` remains a `repo` write. It does not gain a hidden `user` or `org` target. |

`record` MUST NOT be implemented as a projection `UPDATE` without an event. That is the edit/rebuild bug.

#### `cite(helped \| noop \| harmed)`

| | |
| --- | --- |
| Inputs | `memory_id`; `exposure_id` of a delivery that contained that id; `outcome` ∈ {`helped`, `noop`, `harmed`}; optional note. |
| Grant | `write` on the memory's home mount. Cite does not open a second mount. |
| Preconditions | the exposure exists, the id was in the delivered set, the memory is accepted (not quarantined, not superseded, not invalidated). |
| Writes | one `memory.cited` event whose payload includes the outcome. |
| Output | `ok` and the id. The response MUST keep the current honesty line: this records reliance, not verification. |
| Errors | missing `exposure_id` (already an error today); id not in that exposure; memory not readable under the handle; outcome string outside the three values. |
| Idempotency | the same `(exposure_id, memory_id, outcome)` twice is success with no second usefulness delta. A different outcome for the same pair is a conflict error. It MUST NOT apply both. |

Usefulness:

- `helped` is the only outcome that may increase usefulness. The existing magnitude is `CITED_DELTA` (+1.0) until a measured replacement exists. `helped` is not "verified", and it MUST NOT clear an origin penalty by itself (audit finding 6 asked for this split: exposure, reliance, verified outcome, and harm are different facts).
- `noop` is the silent passenger. It MUST NOT increase usefulness. It feeds decay (the 30-day half-life already configured). `run.finished` leaves an uncited memory's usefulness unchanged. The upward `PASSENGER_DELTA` nudge is gone (`crates/kimetsu-brain/src/scoring.rs:30`, `crates/kimetsu-brain/src/projector.rs:706`).
- `harmed` feeds the existing negative-feedback path (the scaled cited-failure penalty and the prune ratio). It MUST NOT increase usefulness. It removes the memory from injection until `supersede` or a human review restores it. A pure passenger still cannot fall through the prune floor on the weak delta alone (`scoring.rs`: a silent ratio bottoms out at −0.1, prune floor −0.2). `harmed` is the explicit signal that is allowed to count as the strong negative.

A missing `outcome` on an old caller is `noop`, not `helped`. Treating "any cite" as success is the bug the audit named. `run.finished` and `run.failed` stay separate events. They MUST NOT be rewritten as a batch of `helped` or `harmed` cites. Gate failures and `run.aborted` still change nothing, matching the projector.

#### `search`

| | |
| --- | --- |
| Inputs | query; read grants; optional level filter that can only narrow the grant set. |
| Reads | mounts with `read`, intersected with the filter. |
| Writes | none required. A diagnostic exposure MAY be recorded and stays off the sync allowlist. |
| Output | matching records with level, kind, and the coverage fields explicit MCP already reports (`evidence_coverage`, partial-evidence notice). `@kimetsu-ai/sdk` documents that a bundle covering half the question looks like one that answers it unless the caller reads those fields (`npm/kimetsu-sdk/README.md`). |
| Errors | filter names a mount the handle cannot read. That is an error, not a silent empty set, so a mis-granted client can tell the mount was refused. |
| Idempotency | read-only. |
| LLM | none. |

`search` is the explicit tool. It is not warm-start. Warm-start stays the strict empty-or-inject path.

#### `supersede`

| | |
| --- | --- |
| Inputs | predecessor id; successor id or successor text; both on one mount. |
| Grant | `write` on that mount. |
| Writes | `memory.superseded` (already on the sync allowlist) naming the successor. The predecessor leaves retrieval (`superseded_by IS NULL` is already the read filter throughout `project.rs`). The event stays for replay. |
| Output | both ids. |
| Errors | cross-mount pair; missing predecessor; successor on another mount. |
| Idempotency | repeating the same pair is success and does not chain a second successor. |
| Undo | restore/replay of the supersede event reverses the link. Deleting the event is not the undo. |

Today's `import_pack(..., replace: true)` invalidates every active row in the target scopes and then inserts. That is reversible because the events remain, and it is the wrong shape for a single-memory correction: it does not point at a successor, and a projection-only edit would not survive rebuild. F0 MUST make the replace-of-one-memory path emit `memory.superseded`. Wholesale pack replace MAY keep invalidating the scope, and it MUST record a durable event per retired id that names the pack version, so replay does not resurrect the retired set.

#### `export_pack`

| | |
| --- | --- |
| Inputs | source mount or mounts the handle can `read`; optional kind filter; `strict` flag. |
| Reads | accepted, not invalidated, not superseded rows, as `export_memories` already filters. |
| Writes | a file. The source database MUST NOT be modified by the scrub. |
| Output | a pack envelope (version 2 once F1 lands; version 1 remains the local unsigned export during F0) and a `ScrubReport`. |
| Scrub | credentials and PII are removed from the exported copy before any digest is computed. `--strict` / `strict: true` fails the export when `ScrubReport.total > 0`. |
| Excludes | `memory_id`, usefulness, use counts, `run` rows, session resume text, exposures, and `run.*`. |
| Errors | a requested mount without `read`; strict scrub failure. |
| Idempotency | same source rows and same signer produce the same canonical body. Gzip is not part of the canonical bytes (see [§3](#3-signed-packs)). |

#### `import_pack`

| | |
| --- | --- |
| Inputs | pack bytes; destination mount; mode `merge` or `replace`; quarantine flag. |
| Grant | `write` on the destination. |
| Checks, in order | gunzip; parse; signature (F1 onward, and always on the bus); every entry scope ∈ signed scopes; destination ∈ signed scopes; destination grant. |
| Writes | merge: `memory.accepted` or `memory.proposed` (quarantine) via the existing dedup. Replace: retire current rows in the destination only, then merge. Provenance stamps `source: pack`, name, and version (`ImportProvenanceScope`). |
| Output | `imported`, `deduped`, `quarantined`, `superseded` counts (`ImportSummary`). |
| Errors | bad signature; scope mismatch; replace combined with quarantine; entry kind/scope unparsable. A scope mismatch is a hard reject of the whole pack. It MUST NOT skip the bad entry and import the rest, and it MUST NOT quarantine the pack "for later widening". |
| Idempotency | re-import of the same normalized text dedups. Re-import of a quarantined pack MUST NOT re-propose rows already accepted or already queued (the in-batch and existing-text checks in `quarantine_memories`). |
| Scrub | import scrubs again even if export claimed it did. |

Quarantine stays the gate for "signature valid, operator has not trusted this signer." Scope failure is not that gate. Replace plus quarantine stays forbidden.

#### Event log, projector, lock, sync

The event log is the source of truth. The projector, FTS, and embeddings are a rebuildable projection (`crates/kimetsu-brain/src/sync.rs`, `docs/memory-maintenance.md`).

- Every accepted write takes the SQLite writer lock for the event append and the projection update that must be atomic with it. Rebuild takes that lock before reading the log and holds it through reset and replay. A concurrent accepted write is in the snapshot or runs after the rebuild.
- FTS and embedding rows that disagree with the log are a bug, not a cache. The hot path may serve the projection; it MUST NOT serve a row the log has superseded.
- Directory sync keeps today's allowlist and denylist. `run.*`, episodes, and the telemetry kinds stay local. Mount level rides inside the allowed memory events, so a synced `memory.accepted` cannot arrive without a level.
- Compact may drop only the expendable telemetry kinds listed in `docs/memory-maintenance.md`. It MUST NOT drop cite, supersede, or accept events. Compaction is not archival; forgetting stays the reversible archive/restore path.
- Failed trace import rolls back both log and projection, as it does today.

### 3. Signed packs

Version 1 (`kimetsu_pack: 1`, gzip JSON, bare arrays) stays importable as a local file during F0 and F1, only into the mount the operator names, and never onto the bus. Version 2 is the signed envelope.

Canonical body, signed, UTF-8 JSON with sorted keys and no insignificant whitespace:

- `kimetsu_pack`: `2`
- `name`, `version`, `description`, `exported_at` (optional, same as v1)
- `scopes`: sorted unique list drawn only from `run`, `session`, `repo`, `org`, `user`, `skill`. `run` in this list is legal only for a pack that will be rejected by export; export MUST NOT emit it. Listing it and then stripping it after the signature is the attack this section exists to stop.
- `memory_count`
- `memories`: the v1 `MemoryExport` objects, order stable (export order: `created_at DESC`, as `export_memories` queries today)

`signature` and `signer` are siblings of that body. They are not inputs to the digest.

- Digest: BLAKE3 of the canonical body. BLAKE3 is already a workspace dependency (`Cargo.toml`).
- Signed message: the canonical encoding of `{ "digest": <hex>, "scopes": <the same list> }`. Ed25519 over that message. The `scopes` inside the message and `scopes` inside the body MUST be equal; inequality is a failed signature, not a warning.
- `signer` is the ed25519 public key. F1 pinned the profile `kimetsu-canon-json/1` and checks the signature with `ed25519-dalek` `verify_strict` (`crates/kimetsu-brain/src/signed_pack.rs`). The pin is in [Resolved](#resolved).
- Gzip wraps the JSON for the file and for the bus. It is outside the digest, so two compressors cannot break the signature. Scrub finishes before the canonical body is hashed. Scrubbing after signing would change the body and is a failed signature on import.

Verification order on import: gunzip, parse, recompute digest, check `scopes` equality, verify Ed25519, then the mount checks in `import_pack`. Failure at any signature step rejects the pack before quarantine and before any SQL write.

Rejection rules:

| Condition | Result |
| --- | --- |
| Bad gzip, bad JSON, unknown `kimetsu_pack` | reject |
| Signature does not verify | reject |
| Body scopes ≠ signed scopes | reject |
| Any `MemoryExport.scope` not in the signed list | reject the whole pack |
| Destination mount not in the signed list | reject |
| Destination not writable by the handle | reject |
| v1 or bare array presented to the bus | reject |
| v1 local file, destination named by the operator, entry scopes wider than that destination | reject |
| Signature verifies, signer not in the local trust store and not on the roster for that mount | quarantine, zero retrieval |
| Signature verifies, signer trusted, quarantine flag set | quarantine |
| `replace` and quarantine together | reject |

Trust, at design level:

- A signing key and an iroh endpoint key MAY be the same ed25519 key. They MUST still be named separately in the roster (role `signer` vs role `endpoint`) so a relay endpoint cannot be assumed to be a pack author.
- The local trust store is an operator-edited list of public keys allowed to land in a given mount without quarantine. First sight of a key is not trust.
- The org roster (next section) can add a key for mounts it administers. A roster signature is what makes that addition durable across machines. A bearer token from Kimetsu Remote is not an ed25519 pack signer.
- Signature proves the bytes and the scopes. It does not prove the lesson is true. Quarantine stays because a poisoned memory persists (the `quarantine_memories` comment: a weight from `trust.rs` only demotes a pack, and a citation cannot be the release condition).

### 4. P2P memory bus

The bus transports announcements and signed packs between Kimetsu processes. The protocol has no clone, fetch, checkout, or worktree operation. Ingest-with-checkout stays a Remote operator feature (`docs/remote-storage.md`) and is not a bus message.

This is the Reason Network transport shape, used for a different object. Reason Network (`DESIGN.md` in `reason-machines/reason-network`) is a team directory and a message bus for live coding-agent sessions. Kimetsu copies none of its mail semantics.

| | Reason Network (their design) | This bus |
| --- | --- | --- |
| Purpose | Who is working on what, and a message to that session | Move a durable memory pack into a mount |
| Payload | `Envelope` text, at most 64 KB; up to 10 UTF-8 text files, 256 KB each, 512 KB total, inside the same envelope | One signed pack per `request_pack`. Small-pack budget in [§7](#7-performance). Not a chat body |
| Addressing | session names (`alice/api-cursor`), plus person/repo/broadcast for humans at a terminal | pack digest, then a destination mount on the receiver |
| Retention | mail waits for a session and is dropped about a day after the session is forgotten | imported packs become brain events. They are not mail. They follow supersede, compact, and replay |
| Presence | full mesh, `Presence` pushed on every local change, including session lists (capped at 100 names) | light announcements on a side task. `context` MUST NOT wait for them |
| Roster | membership of machines; any member may invite; removed peers are disconnected but not treated as cryptographically powerless ("this is for friends") | membership plus the mounts and signer keys each endpoint may use. A removed endpoint cannot be dialed for `request_pack` |
| Identity | iroh `EndpointId`, persistent machine key | same idea: a persistent endpoint key |

Transport, iroh:

- **QUIC.** An endpoint is dialed by its public key. The peer connection is authenticated by that key.
- **mDNS.** LAN discovery without a public DNS dependency. The discovery payload is the endpoint id and the roster id. It MUST NOT contain memory text.
- **Hole-punch.** Used when the peers are not on the same LAN segment.
- **Encrypted relay.** Used when hole-punch does not open a direct path. The relay carries ciphertext. It is not an authority, it is not a second brain, and it MUST NOT be required for a local `context`. Operators MAY run their own relay and limit it to roster endpoint ids, which is the deployment Reason Network documents for a team that does not want a public relay. Main does not ship Reason's relay list, n0's relays, or a Kimetsu-hosted relay. The pin is `BusRelay::Disabled` unless a caller passes `BusRelay::Custom` ([Resolved](#resolved)).

Roster, signed:

- A roster document names the group, the endpoints, and for each endpoint the mounts it may serve and the pack-signing keys it may publish.
- The document is signed by the org or user key that administers the group. Entries from an endpoint that is not on the roster are ignored.
- F2 ships a static signed roster the operator installs, plus rejection of non-members. F2 does not require a full mesh. Invitation and federated supersede are the later slice in [Invitation, supersede, and conflicts](#invitation-supersede-and-conflicts).
- Remote namespaces are not auto-enrolled. Binding a namespace to a roster is an explicit operator act (F3).

Announcements, light:

- Fields: endpoint id, pack digest, byte length, signed scopes, signer public key, a short TTL.
- No memory text, no events, no embeddings, no repository URL.
- Persisted off the `memories` table and off the FTS index, so a presence write cannot take the recall lock.
- An announcement does not import, does not quarantine, and does not grant.

`request_pack`:

1. Caller asks a roster member for a digest it announced.
2. Response is the signed pack bytes (gzip outside the digest).
3. Receiver runs `import_pack` checks. Scope or signature failure rejects with no SQL write.
4. A trusted signer imports under the destination grant. An untrusted signer quarantines only when the scope check passed.
5. The call is explicit. `context` and warm-start MUST NOT invoke it.

#### Invitation, supersede, and conflicts

F2's static roster is what the operator installs. An invitation is a later `kimetsu_roster` 1 document signed by that same admin key. Generation 0 omits the field, so an F2 document still verifies. Peers exchange the signed document when they connect. The higher generation replaces the lower one and is the document the next bind reads. The same generation with a different digest is not merged. The receiver keeps its current roster and reports a roster generation conflict. A removal is a reissue at the next generation. Once adopted, that endpoint cannot finish `request_pack`.

Federated supersede is a field on `kimetsu_pack` 2, not a new frame and not a directory-sync batch. The `memories` array is unchanged: accepted rows only, no `memory_id`, no usefulness, no superseded row. When the brain has `memory.superseded` events for the exported mount, the pack adds `supersedes`. Each entry is the scrubbed predecessor text, the scrubbed successor text, the two kinds, the mount, and the HLC of that event (the peer clock, `source_hlc`, when the event was itself imported). Scrub runs before the signature. The field is omitted when empty, so a pack with no supersede stays the F1 canonical body. A link whose mount is not the destination, or whose successor is not in `memories`, rejects the whole pack before any SQL write.

A trusted merge resolves those texts to local rows and writes `memory.superseded` through the projector. The same successor again is success and does not write a second event. Two successors are both kept. Replay orders them by that source HLC. The later one is `superseded_by`. The disagreement is one `sync_conflicts` row: the predecessor id and the two survivor ids, stored once. No model reads the two texts. An untrusted signer still quarantines and does not move a local link. `replace` stays wholesale invalidation of the destination and does not also apply these links.

A cite conflict stays on the brain that holds the exposure. A second outcome for the same exposure and memory is an error and applies neither extra delta. Exposures are not pack fields and are not frames.

#### Directory sync and the bus

`kimetsu brain sync` and the bus are one replication policy with two carriers. They are not interchangeable, and neither carrier is a silent reading of the other's bytes.

What stays on the machine, for both carriers, is the complement of `SYNC_ALLOWED_KINDS` in `crates/kimetsu-brain/src/sync.rs`: `work.episode`, `context.served`, `retrieval.regret`, `digest_served`, `resume_served`, `context.injected`, and every `run.*` kind. The SQLite file, embeddings, FTS, and any git object stay too. No carrier ships `brain.db` or a clone. The bus protocol still has no clone, fetch, checkout, or worktree frame.

What the bus carries is announcements and signed `kimetsu_pack` 2 only. That is the accepted projection of a mount, plus the optional `supersedes` field from [Invitation, supersede, and conflicts](#invitation-supersede-and-conflicts). `memory_id` stays off that wire. There is no frame for a JSONL event-log batch, a chat message, or a directory cursor. A type byte outside hello, announce, request_pack, pack, reject, roster_sync, and roster drops the peer. Invitation and roster membership still gate every exchange on the bus. A non-member does not finish `request_pack`, and there is no second channel that would carry a sync batch. `BusRelay::Disabled` stays the default unless the operator sets a relay.

What the directory carries is the offline complement. `kimetsu brain sync` keeps the directory of JSONL batches, a cursor per machine, and no pack signature. The operator-configured directory is the trust boundary. The durable allowlist is unchanged: `memory.accepted`, `memory.proposed`, `memory.rejected`, `memory.invalidated`, `memory.restored`, `memory.corrected`, `memory.cited`, `memory.superseded`. The CLI stays. Callers are not moved onto the bus, and this path does not dial it. Cite events stay on this allowlist. They are not pack fields.

How a signed pack and an event-log batch relate:

- A directory batch is not a pack. `import_events` rejects a `kimetsu_pack` document, a gzip pack, or a bare memory array before any SQL write. Those bytes belong to `import_pack` / `import_signed_pack`.
- A pack is not a directory batch. `verify_pack_json` and the unsigned v1 parser reject a JSONL sync batch — a JSON object or line with `event_id` and `kind`, and no `kimetsu_pack` — before any SQL write. The bus does not call `import_events`.
- Directory import does not call `import_pack`. Pack import does not insert the sender's event rows. Reading a sync batch as an unsigned pack, or a pack as an event-log replay, is a failed import.

Trust, where a pack is involved, is the `import_pack` table in [§3](#3-signed-packs). A bad signature rejects before quarantine and before SQL. An unknown signer quarantines and does not enter retrieval. `replace` together with quarantine rejects. The directory carrier has no signer. It does not consult the roster, because it does not speak the bus. A shared directory does not make an untrusted pack signer trusted.

Idempotency and conflicts use the F5 projector rule. Directory replay still dedups on `event_id` with `INSERT OR IGNORE`. A `memory.superseded` event whose `memory_id` and `survivor_id` are already in the log is a skip even when `event_id` is new: the same successor does not write a second event. Two successors are both kept. Replay applies events in HLC order. The link moves only when the incoming `source_hlc` (otherwise the event HLC) is later. That later one is `superseded_by`. The disagreement is one `sync_conflicts` row. Pack `supersedes` links still resolve by scrubbed text through `apply_links`, which skips a successor the local log already names and does not run for an untrusted signer. A pack does not learn the directory's `memory_id`. A directory batch is not rewritten into ids the pack minted. An operator does not point both carriers at the same bytes.

`kimetsu brain sync` remains that offline complement. It is not deleted, and it is not a second bus.

#### Threat model

| Actor | Can | Cannot |
| --- | --- | --- |
| Local operator | open grants, trust a signer, accept a quarantine proposal | be overridden by an announcement |
| Roster member | dial peers, announce digests, serve packs it has signed | push a pack into a mount the signature omits; write the receiver's brain without `import_pack` |
| Non-member | — | complete a team-protocol handshake. The receiver drops the connection |
| Relay operator | see timing and sizes of ciphertext | read pack bytes; inject a pack that verifies under someone else's key |
| Malicious trusted signer | offer a false lesson inside a valid signature | widen scopes without breaking the signature; skip quarantine if the operator has not trusted them. Truth is not the signature's job |
| Compromised agent on a member machine | `export_pack` of mounts it can read, then announce | export `run`; attach the git worktree; read mounts it was not granted. Export scrub still runs |

What stays local even for a member: the SQLite file, embeddings, FTS, session resume text, `run.*`, episodes, and anything `export_pack` refuses. The event log is not a bus frame. Only the allowlist in [Directory sync and the bus](#directory-sync-and-the-bus) may leave the machine, and only through the offline directory. What is verified before any write: gzip framing, digest, signature, scope equality, entry scopes, destination grant. What is not verified by cryptography: usefulness, factual accuracy, or that the signer is a person the operator meant. That last step is the trust store and the quarantine queue.

### 5. Unified Rust SDK

One Rust crate, `kimetsu-sdk`, exposes `context`, `record`, `cite`, `search`, `supersede`, `export_pack`, `import_pack`, and `request_pack` (`crates/kimetsu-sdk/src/lib.rs`). The bus calls `import_signed_pack`, the signed path of that import (`crates/kimetsu-bus/src/endpoint.rs:452`). It does not reimplement merge, quarantine, scrub, or supersede.

Pending, not done: `kimetsu network request` still calls `BusEndpoint::request_pack` directly (`crates/kimetsu-cli/src/network.rs:560`). It does not call `kimetsu_sdk::request_pack` (`crates/kimetsu-sdk/src/lib.rs:125`). A fix is pending on a separate branch.

Why this crate is Rust:

| Reason | Grounding |
| --- | --- |
| The store is already Rust | `kimetsu-core` and `kimetsu-brain` own the log, projector, FTS5, packs, scrub, quarantine, sync, and the usefulness constants. A second store would fork the mount rule. |
| SQLite stays in-process | `rusqlite` with the bundled engine. The hot path is a local read of a projection the same process just wrote. A TypeScript store would cross FFI or a socket on every `context`. |
| iroh is a Rust stack | QUIC, hole-punch, mDNS, and the relay client live in-process beside the store. Reason Network made the same choice for the same libraries, and then used them for mail. Kimetsu uses them for packs. |
| Signatures sit next to the bytes | BLAKE3 is already in the workspace. Ed25519 verification has to happen before SQLite, in the same function as `import_pack`. |
| One binary | The CLI, the local MCP server, and the bus endpoint ship as the existing Kimetsu binary. Remote remains a separate binary (`kimetsu-remote`) that links the same crate. |

MCP stays the edge toward Claude Code, Codex, Pi, OpenClaw, and Cursor. Tool names may gain an `outcome` field on `kimetsu_brain_cite` in F0; this ADR does not add a parallel protocol. `@kimetsu-ai/sdk` stays a `fetch` client of Remote's HTTP MCP (`npm/kimetsu-sdk/src/index.ts`). It MUST NOT open `brain.db`, MUST NOT verify pack signatures itself as a second implementation, and MUST NOT grow a bus. When F0 adds fields, the client learns to pass them through. The type `MemoryScope` in that package is updated only when the wire strings exist, in the same change as the server.

Kimetsu Remote is not a second brain. It is a durable checkpoint of a mount (`repo`, and `org` when sharing is on) plus a disposable SQLite projection per request (`docs/remote-storage.md`). Under this contract:

- A repo checkpoint contains `repo` rows (and, during the alias window, stored `project` rows). It MUST NOT contain `user` rows.
- An org checkpoint contains `org` rows. Publishing it requires the global-token write grant, as today.
- The per-request database is thrown away. Losing it does not delete the checkpoint.
- Remote does not dial the bus. A pack enters Remote only through `import_pack` on a server that holds the destination grant, or through the existing checkpoint publish. F3 writes down how an operator binds a namespace to a roster; the bind is data, not an implicit trust of every Remote token as a signer.
- Checkpoint limits (64 MiB compressed, 256 MiB decoded, 2,000,000 rows, format locked to schema) are unchanged by this ADR.

### 6. Non-goals

Restated in [Non-goals](#non-goals) so the decision section can be read alone:

- Kimetsu is not a general chat bus. `kimetsu chat` remains the local coding assistant on the same brain.
- The bus does not clone repositories or ship worktrees. Remote ingest checkouts stay Remote's.
- Generative consolidation MUST NOT merge across mounts. A model that rewrites a lesson is an opt-in tool and still emits `record` / `supersede` on one mount.
- This design does not replace MCP hosts, hooks, or `@kimetsu-ai/sdk`.

### 7. Performance

Budgets below are acceptance criteria for F3. They are not measurements of the current binary. The agent-memory audit's suggested starting targets were different and looser: warm local retrieval p95 under 100 ms at 10k memories and under 250 ms at 100k, on hardware the report says must be specified (`docs/audits/2026-09-04-agent-memory-audit.md`, "Recommended implementation order"). Those remain useful broker-level targets. They are not this hot-path budget.

What has been measured, and what has not:

| Observation | Source | What it is not |
| --- | --- | --- |
| One eight-term IDF pass using `COUNT(*) ... LIKE` scanned `memories`. Median about 4.03 ms at 1k rows, 41.26 ms at 10k, 437.72 ms at 100k | audit finding 3, Python SQLite in memory, five samples, not the Rust broker | not a p99 of `context`, and not acceptable on the hot path |
| Default reranker about 138 ms, recall@4 0.949, MRR 0.914, on a 210-case export | README | not the lexical hot path. Semantic retrieval stays opt-in and outside the few-millisecond budget |
| Warm-start's synchronous read makes no constant-time claim | `docs/warm-start-contract.md` | this ADR also makes none for a million-row brain. The README's "~1M memories, sub-2s retrieval" is a different operating point |
| Remote loads a whole checkpoint and rebuilds FTS per request | `docs/remote-storage.md` | not the local hot path, and not the 100 ms LAN budget |

Hot-path acceptance, F3, on a machine whose page cache already holds the FTS index:

| Path | Budget | Fails the design if |
| --- | --- | --- |
| Local `context`, warm FTS, no embeddings, no reranker, no network, rejected SQL plans that scan `memories` with `LIKE` | p99 of a few milliseconds | the call waits on mDNS, relay, hole-punch, an LLM, a model load, or an IDF full scan |
| `request_pack` of a small pack (onboarding-sized export, not a 64 MiB checkpoint) over direct LAN QUIC, until the pack is verified in memory | around 100 ms | the figure includes relay, a failed punch, or the FTS rebuild. Those are out of budget. Signature verification is in budget |
| Presence (mDNS, roster refresh, announcements) | zero time on the `context` / `search` stack | a down relay changes local recall latency |
| Writes | WAL so a `cite` or `record` does not block readers; announce persistence is not in the recall transaction | a presence write takes the writer lock the rebuild uses, or a reader can observe a half-applied supersede |
| Generative model | absent on `context`, `search`, warm-start, cite, and pack verify | a distiller, `kimetsu ask`, or skill draft runs as part of those calls |

The durable `import_pack` after a verified `request_pack` is the same store operation as a local import. It is asynchronous with respect to recall: `context` does not wait for the new rows to be searchable. Callers that need read-your-writes say so on the import, not by stalling presence.

WAL is already the Remote SQLite checkpoint adapter (`docs/remote-storage.md`). The local brain uses the same engine; F3's criterion is that the hot path's journal mode and the lock scope match the sentence above, not a new database.

The two-physical-host `request_pack` figure is not recorded in this ADR. Operators measure it with `scripts/f3-hotpath-two-hosts.sh` on two LAN machines, as described in [f3-two-physical-hosts.md](../operators/f3-two-physical-hosts.md). A loopback run, the veth pair, and that script's self-test do not fill the figure. Numbers that already exist stay in [0001-f3-measurements.md](0001-f3-measurements.md).

An internet/WAN run is a different path. `wan-serve` and `wan-client` in that script dial an operator-supplied external UDP endpoint and label the report internet/WAN. They do not fill the physical-LAN row. No WAN number is recorded here. The operator page is [f3-internet-wan.md](../operators/f3-internet-wan.md). Relay stays disabled.

### 8. Phased delivery

Each phase leaves the default path free of a generative LLM and free of a network wait inside `context`. Later phases MUST NOT put work on the hot path that would make the F3 budgets impossible (no IDF scan, no LLM, no presence join).

#### F0 — Local mount and operations

- **Deliverable.** The seven operations honor grants and levels on the local store. `project` is an input alias of `repo` with the retrieval and non-merge rules in [§1](#mapping-from-memoryscope). New events carry the level. `cite` takes `helped | noop | harmed`, and only `helped` may increase usefulness. `supersede` writes `memory.superseded` for a single-memory replacement. Contamination tests in [§1](#contamination) exist and pass, including MCP payload and warm-start lane separation. The edit path that skipped the log either writes a correction event or is removed from the write surface.
- **Exit.** The test table is green on a lean `cargo test` of `kimetsu-brain` and the MCP contract tests. A rebuild preserves levels. Directory sync still refuses `run.*` and telemetry. No signature and no socket.
- **Unchanged.** Pack gzip v1, quarantine semantics, Remote checkpoints, `@kimetsu-ai/sdk` behavior except a pass-through of `outcome` if the MCP field lands in this phase, decay half-life, broker weights, sync directory layout.

#### F1 — Signed packs

- **Deliverable.** `kimetsu_pack` 2 with BLAKE3 and Ed25519 as in [§3](#3-signed-packs). Local trust store. Scope mismatch rejects with no SQL. Unknown signer quarantines. Bus still absent. v1 local import remains, narrowed to the named mount.
- **Exit.** Round-trip tests: tamper a scope, tamper a memory, reorder scopes, strip `signature`, import a `user` entry inside a `repo`-signed pack. All reject. A trusted signer merges. An untrusted signer produces only proposals. Export `--strict` still aborts on scrub hits, and the signature is over the scrubbed body.
- **Unchanged.** Remote HTTP, directory sync, MCP host wiring, iroh not linked.

#### F2 — Bus

- **Deliverable.** iroh endpoint, mDNS, hole-punch, encrypted relay, static signed roster, light announcements, `request_pack`. Only v2 packs on the wire. Presence tasks are separate from recall. No git operation in the protocol.
- **Exit.** Two processes on one LAN: announce, `request_pack`, verify, import. A non-member handshake fails. A scope-mismatched pack does not write. Stopping the relay does not change a local `context` latency sample. A repository fixture is not transferred (assert the protocol has no such message, and that a peer sending one is dropped).
- **Unchanged.** Remote still does not dial the bus. Directory sync still replicates events. Quarantine still requires a human to enter retrieval.

#### F3 — Budgets and Remote alignment

- **Deliverable.** Measurements for the [§7](#7-performance) table: warm FTS `context` p99, small-pack LAN QUIC, presence-off. Hardware, corpus size, and cold-vs-warm are reported beside the numbers, in the same spirit as the audit's refusal to treat a Python IDF microbench as a broker p99. An operator document for binding a Remote namespace to a roster without treating bearer tokens as pack signers. The Rust SDK is the in-process entry `kimetsu-remote` calls for these operations (`crates/kimetsu-remote/src/store.rs`). The CLI uses that crate for the store calls except `network request`, which still calls `BusEndpoint::request_pack` directly. That call is pending, not done ([§5](#5-unified-rust-sdk)).
- **Exit.** The hot path meets the budgets or the phase stays open. The two-physical-host LAN row and the WAN row have no number in `docs/adr/0001-f3-measurements.md`, so that part of the phase stays open. Loopback and veth rows in that file are not the LAN figure. A failed budget does not get "fixed" by moving presence onto `context` or by calling a model. Remote checkpoint limits and the disposable projection stay as documented.
- **Unchanged.** MCP as the agent edge, `@kimetsu-ai/sdk` as an HTTP client, decay constants, the decision not to clone repos.

#### F4 — Host adapters

- **Contract.** MCP stays the host edge. These adapters do not open `brain.db` and do not add a protocol. The Rust SDK stays in-process beside SQLite. `@kimetsu-ai/sdk` stays an HTTP client. `kimetsu plugin install` writes the host file, and the MCP `instructions` field plus that file state the same operator workflow. Stdio grants are read `repo`, read `user`, and write `repo` only while write tools are on. `kimetsu_brain_record` stores `repo` and does not accept another level. `kimetsu_brain_context` returns capsules from those read grants and does not call a generative model. Warm start is that evidence digest. `kimetsu_brain_cite` takes `exposure_id`, `memory_id`, and `outcome` `helped`, `noop`, or `harmed`. Omitting `outcome` records `noop`. Only `helped` raises usefulness. `harmed` sets `invalidated_reason` to `cite_harmed`.
- **Claude Code.** Workspace `.mcp.json` sets `mcpServers.kimetsu` to `type` `stdio`, `command` `kimetsu`, `args` `mcp serve --workspace .`. The same object is still also written under the historical `servers` key. Global install writes `mcpServers` only, in `~/.claude.json`. Source checked 2026-11-06: `https://code.claude.com/docs/en/mcp`.
- **Codex.** `.codex/config.toml` sets `[mcp_servers.kimetsu]` `command` and `args` to that same stdio server. Source checked 2026-11-06: `https://developers.openai.com/codex/mcp`.
- **Cursor.** `.cursor/mcp.json` sets `mcpServers.kimetsu` `type` `stdio` with the same command and args. Workspace install also writes `.cursor/rules/kimetsu-brain/rule.md`. Source checked 2026-11-06: `https://cursor.com/docs/mcp`.
- **Grok Bot.** No settings file is documented. Install writes `.kimetsu/hosts/grok-bot.md`. That file is an operator snippet: ask the Bot to add the stdio command, or, with `--remote`, the `https://<base>/mcp/<repo>` URL and `Authorization: Bearer ${KIMETSU_REMOTE_TOKEN}`. It does not draw a Connect card and it does not write Grok Build's `~/.grok/config.toml`. Source checked 2026-11-06: `https://docs.x.ai/grok-bot/computer-and-apps`.
- **Verified.** `kimetsu-chat` `four_hosts_generate_stdio_mcp_and_drive_context_record_cite` installs the four workspace targets, checks the generated command, and drives MCP `initialize` plus `tools/call` for record, context, and cite. `grok_bot_remote_snippet_names_the_http_endpoint_and_no_config_file` checks the remote snippet. Context in that test returns the repo lesson and no org or skill text. `helped` raises usefulness, `noop` does not, `harmed` lowers it and records `cite_harmed`.
- **Not verified.** No Claude Code, Codex, Cursor, or Grok Bot process loaded these files. Claude Code still asks a person to approve a project `.mcp.json`. Grok Bot's chat confirmation was not exercised. Pi and OpenClaw wiring is unchanged.
- **Unchanged.** No second agent protocol. No TypeScript bus. No direct `brain.db` access from `@kimetsu-ai/sdk`. No default relay. Directory JSONL sync stays the offline complement. Roster membership still gates the bus. Signed packs stay the wire format. Remote HTTP checkpoints do not dial the bus.

#### F5 — Federated supersede and conflicts

- **Deliverable.** Invitation, as in [Invitation, supersede, and conflicts](#invitation-supersede-and-conflicts), and supersede links inside signed packs. A disagreement is a `sync_conflicts` row. The later source HLC is the survivor. No generative model is on the path.
- **Exit.** Two peers that supersede one text to two successors converge on the later source HLC and each keep one conflict row. An untrusted signer does not move the local link. A link for another mount writes no SQL. An announcement still has no memory text.
- **Unchanged.** No default relay. `@kimetsu-ai/sdk` stays an HTTP client and does not open `brain.db`. Host adapters stay as F4 left them. No chat frame and no git clone. Directory sync is not a bus frame; the carrier split is [Directory sync and the bus](#directory-sync-and-the-bus).

#### F6 — Directory sync and the bus

- **Deliverable.** The contract in [Directory sync and the bus](#directory-sync-and-the-bus). `kimetsu brain sync` remains the directory replica. The bus gains no sync-batch frame. Cross-carrier bytes are rejected before SQL. The same superseded pair does not append a second event.
- **Exit.** A pack presented to directory import writes nothing. A JSONL batch presented to pack verify writes nothing. `run.*` still stays out of both. A repeated predecessor/survivor pair is one `memory.superseded` event. Two successors still produce one `sync_conflicts` row. Roster membership still gates `request_pack`. `BusRelay::Disabled` stays the default.
- **Unchanged.** No TypeScript bus. Remote does not dial the bus. MCP stays the host edge. Cite conflicts are not pack fields. No default relay, no chat frame, no git operation.

## Consequences

If this is adopted:

- CLI, MCP, warm-start, import, replay, and the bus share one contamination rule that a test can fail.
- Promote stops meaning "a cite happened." It means `cite(helped)` on a delivered id. That matches the MCP text that already denies verification, and it closes audit finding 6's conflation. The +0.1 passenger bump on success goes away; operators who liked the nudge will see usefulness move less.
- A pack cannot gain a mount by deleting a JSON field. The scopes are signed twice (body and signed message) and must match.
- Local recall does not wait on the network. A team can move a pack without a Kimetsu Remote deployment and without copying the git tree.
- The store stays in the process that already links SQLite. Remote remains a checkpoint front-end of the same mounts.

Costs:

- Six mounts are stricter than four baseline enum variants. `session`, `org`, and `skill` are stored levels on main (`crates/kimetsu-core/src/memory.rs:134`, `crates/kimetsu-core/src/memory.rs:135`, `crates/kimetsu-core/src/memory.rs:136`). `kimetsu_brain_record` stores `repo` (`crates/kimetsu-chat/src/mcp_server.rs:1116`).
- The alias window is easy to implement wrong. Reading both stored strings while forbidding cross-string merge is the whole trick. A migration that `UPDATE`s the projection without events will be reverted by the next rebuild.
- Old clients that omit `outcome` become `noop` and stop promoting. That is a silent usefulness change for every MCP cite that omits the field. It is intentional. `kimetsu_brain_cite` implements it (`crates/kimetsu-chat/src/mcp_server.rs:1185`).
- v1 URL imports (`kimetsu brain import https://…` as the README shows) cannot ride the bus. Publishers who want F2 sign the pack.
- The relay is a trust dependency for connectivity and metadata (who talked, how much) even though it cannot read the pack. Networks where hole-punch fails do not get the 100 ms budget.
- Roster key loss splits a group that still has healthy SQLite files. That is a new operational failure.
- F0 and F1 do not share anything across machines. Directory sync and Remote remain the sharing tools until F2. From [Directory sync and the bus](#directory-sync-and-the-bus), roster members share signed packs on the bus, and directory sync remains the offline event-log complement.
- Meeting a few-millisecond p99 means deleting the unbounded IDF scan from the hot path (audit finding 3) in F0's store work, not tuning the bus in F3.

Left alone until a phase says otherwise: checkpoint format and size limits, the TypeScript client's lack of a SQLite dependency, sync's allowlist, warm-start's 0.35 / 60% / half-term heuristics, the 30-day half-life, broker weights, and skill drafting's optional model.

## Alternatives considered

**Keep the short Spanish ADR.** It named the same six levels and the same bus, in a few screens. It did not fix grants to today's tokens, did not say what `project` versus stored `repo` rows do during a migration, did not split cite outcomes from `CITED_DELTA`, and did not say how v1 packs, quarantine, and replace interact with a signature. A reviewer could not implement F0 from it without inventing the contract. Rejected as the document of record. This file replaces it in place.

**Use Reason Network as the memory system.** The transport fits (iroh, roster, LAN). The object does not. Reason Network stores mail for a live session and drops it on a day-scale timer; attachments are small UTF-8 files inside a 64 KB-class envelope; presence is the product and is pushed on the mesh. Kimetsu's product is a durable, scoped, replayable brain with promote, decay, quarantine, and FTS. Bolting memory onto session mail would make contamination untestable (any session can be addressed) and would put presence on the path of every read. Rejected. The transport ideas are adopted in [§4](#4-p2p-memory-bus); the mail product is not.

**Reimplement the store in TypeScript inside `@kimetsu-ai/sdk`.** The SDK exists so Pi, OpenClaw, Cursor, and VS Code stop shelling out and parsing text (`npm/kimetsu-sdk/README.md`). It is a typed MCP client. A second store would diverge the moment FTS, the writer lock, or the cite delta changes, which is the drift the package was created to stop. The hot-path budget does not survive a hop out of `rusqlite`. Rejected.

**A CRDT or server-side memory graph (Mem0 / Zep style) as the core.** The README's comparison is about benchmark operating points and about those systems calling a model on the write and the read. The models audit already showed a similarity graph merge that crosses scope. A CRDT that converges by design would converge contaminated mounts as well, unless the mount were part of the key and merges across keys were forbidden — which is the rule this ADR states on top of the event log Kimetsu already has. The agent-memory audit's closing line still holds: replacing SQLite, or adding another generative layer, is not justified by the evidence gathered there. Rejected as the core. Optional local embeddings stay optional.

**Sync by cloning the repository, or by copying `brain.db`.** A clone ships the worktree, secrets the scrubber never saw, and the `.git` directory, to solve a memory problem. Copying the SQLite file races the writer lock and skips the event allowlist, so `run.*` and episodes leak and a half-written WAL can be the replica. Directory sync of the allowlisted log is the existing tool that avoids both. The bus moves scrubbed, signed packs instead of either. Rejected.

## Non-goals

- **Generic agent chat.** Session messaging, inboxes, delivery receipts, and "who is typing" are Reason Network's product. They are not operations of this store.
- **Cloning repositories over the bus.** No message carries git objects, a worktree, or a checkout. Remote's `--repos-file` / `--checkout-dir` ingest is unchanged and is not a peer protocol.
- **Generative consolidation across mounts.** A distiller or a decision provider MUST NOT be the thing that moves a lesson from `repo` to `org` or from `repo` to `skill`. Those tools, when the operator enables them, emit the same `record` / `supersede` calls as anyone else, on one mount, with a grant.
- **Replacing MCP hosts.** Claude Code, Codex, Pi, OpenClaw, and Cursor keep their MCP or hook wiring. This ADR does not introduce a second agent protocol.

Also out of F0–F3, stated so they are not implied: rewriting `@kimetsu-ai/sdk` in Rust, replacing Remote's checkpoint backends, retuning broker weights, changing the 30-day half-life, and claiming the few-millisecond budget for the semantic reranker. Putting the event log on the bus as a frame is decided against in [Directory sync and the bus](#directory-sync-and-the-bus): the directory replica stays, and the bus stays signed packs.

## Resolved

Main settles these. The cite is the code that decides them.

1. **Canonical JSON.** The profile is `kimetsu-canon-json/1`, not RFC 8785. The body is UTF-8, object keys are sorted by raw UTF-8 bytes, and separators are `:` and `,` with no insignificant whitespace. The Ed25519 message is that same profile applied to `{"digest":"<hex>","scopes":[...]}`. The signature is RFC 8032 Ed25519 over those raw UTF-8 bytes, checked with `ed25519-dalek` `verify_strict`. It is not Ed25519ph and not Ed25519ctx. `signer` is the 32-byte public key as lowercase hex. `signature` is the 64-byte signature as lowercase hex. `crates/kimetsu-brain/src/signed_pack.rs:3`, `crates/kimetsu-brain/src/signed_pack.rs:21`, `crates/kimetsu-brain/src/signed_pack.rs:44`, and the pin test at `crates/kimetsu-brain/src/signed_pack.rs:864`.
2. **Where `session` rows sit.** They live in the project SQLite file, keyed by level `session` and `session_id`. `crates/kimetsu-brain/src/mount.rs:3` and `crates/kimetsu-brain/src/project.rs:815`. `replay_preserves_session_level` reads `scope` from that project `memories` table and expects `session` (`crates/kimetsu-brain/src/mount.rs:480`, `crates/kimetsu-brain/src/mount.rs:518`).
3. **Default relay.** A stock endpoint uses `BusRelay::Disabled` and does not know a public relay list. LAN discovery is mDNS plus direct QUIC. `BusRelay::Custom` is the operator-supplied relay map. The CLI binds `Disabled` and has no relay flag. `crates/kimetsu-bus/src/lib.rs:20`, `crates/kimetsu-bus/src/endpoint.rs:33`, `crates/kimetsu-bus/src/endpoint.rs:44`, `crates/kimetsu-cli/src/network.rs:5`, `crates/kimetsu-cli/src/network.rs:654`.
5. **MCP `outcome`.** `kimetsu_brain_cite` takes an `outcome` field on the existing tool. Omitted outcome is `noop`. There is no second tool name. `crates/kimetsu-chat/src/mcp_server.rs:72`, `crates/kimetsu-chat/src/mcp_server.rs:1185`, `crates/kimetsu-chat/src/mcp_server.rs:2204`, `crates/kimetsu-chat/src/mcp_server.rs:2212`.

## Open questions

Main does not settle these. Nothing below is a guessed decision.

4. **Namespace ↔ roster.** A Remote namespace and a bus roster stay different trust domains. The operator page is a manual note, and it says defaulting one org key to both sign the roster and mint a global token is intentionally unset (`docs/operators/remote-namespace-roster.md:14`). `kimetsu-remote` does not dial the bus (`crates/kimetsu-remote/src/store.rs:5`) and its crate manifest does not enable `kimetsu-sdk`'s `bus` feature (`crates/kimetsu-remote/Cargo.toml:56`). Whether one org key should be allowed to do both is still undecided.
6. **Skill files versus the skill mount.** The bus frames are `hello`, `announce`, `request_pack`, `pack`, `reject`, `roster_sync`, and `roster` (`crates/kimetsu-bus/src/protocol.rs:19`). None of them is a `SKILL.md` body. Drafted skill-file text still lives on the synthesis report (`crates/kimetsu-brain/src/skill_synthesis.rs:64`). Whether a later ADR should put that installed file on the bus is still undecided.

## References

- `docs/warm-start-contract.md` — digest without a model or a network, resume lane versus repo digest, read-only user-brain opener, coverage and uncertainty gates, no fabricated warm-start credit.
- `docs/memory-maintenance.md` — writer lock, replay, compact's expendable kinds, archive versus compact.
- `docs/remote-storage.md` — repo versus global tokens, shared org brain, write-tool opt-in, WAL, checkpoint limits, disposable projection, ingest checkouts.
- `docs/audits/2026-09-04-agent-memory-audit.md` — citations treated as success (finding 6), excluded capsules in the MCP payload (finding 1), edits that do not survive replay (finding 2), IDF scans (finding 3), episode scope (finding 9), p95 targets that this budget does not replace.
- `docs/audits/2026-09-04-models-parameters-memory.md` — cross-scope consolidation above cosine 0.92.
- `README.md` — brains, packs, promote / decay, MCP, no LLM on the default pipeline, reranker timing, Remote.
- `crates/kimetsu-core/src/memory.rs` — `MemoryScope`, `MemoryKind`.
- `crates/kimetsu-core/src/config.rs` — 30-day usefulness half-life.
- `crates/kimetsu-brain/src/packs.rs` — envelope v1, scrub, merge, replace-as-invalidate, quarantine.
- `crates/kimetsu-brain/src/sync.rs` — event-log replica, allowlist, `run.*` excluded.
- `crates/kimetsu-brain/src/scoring.rs`, `crates/kimetsu-brain/src/projector.rs` — `CITED_DELTA`, `PASSENGER_DELTA`.
- `crates/kimetsu-brain/src/skill_synthesis.rs` — citation threshold 3, staleness.
- `crates/kimetsu-chat/src/mcp_server.rs` — `kimetsu_brain_context`, `kimetsu_brain_record`, `kimetsu_brain_cite`, write-tool gate.
- `docs/host-adapters.md` — F4 install files and the stdio grant workflow.
- `npm/kimetsu-sdk/README.md`, `npm/kimetsu-sdk/src/types.ts` — HTTP client, current scope union.
- Reason Network design (external): `https://github.com/reason-machines/reason-network/blob/main/DESIGN.md` — iroh session bus this ADR does not adopt as memory.
