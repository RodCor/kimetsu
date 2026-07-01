
Everything kimetsu remembers lives in **brain.db**, a single SQLite
file. Each project gets one at `<project>/.kimetsu/brain.db`. A
global user brain at `~/.kimetsu/brain.db` holds memories that
follow you across projects (set `KIMETSU_USER_BRAIN=0`, or
`[kimetsu] use_user_brain = false` in `project.toml`, to disable).

`.kimetsu/` is deliberately **lean**: a brain-only install holds just
`brain.db` (plus its `-wal` / `-shm` and any `brain.db.bak-*` migration
sidecars) and `project.toml`. Memory writes persist straight to brain.db;
they do **not** create a per-write `runs/<id>/` directory. Only a real agent
run still writes a `runs/<id>/` dir with its artifacts. The transient
non-brain working dirs (`proactive/`, `chat/`, `bench/`) live OUT of the repo,
under `~/.kimetsu/cache/<project-hash>/`, so they never clutter your tree.

The brain is event-sourced, and the **`events` table inside brain.db is the
durable event log**, not a loose pile of JSONL files. A **projector** replays
those events into materialized tables the broker can query fast.
`kimetsu brain rebuild` re-derives every projection from the `events` table
(pass `--from-traces` to re-import from legacy on-disk `trace.jsonl` files for
recovery). The materialized tables:

- `runs`: one row per agent run (started_at, terminal_kind, cost).
- `events`: every event ever written, raw; the durable source for rebuild.
- `memories`: the durable knowledge. Each row carries scope
  (`global_user`, `project`, `repo`, `run`), kind (`preference`,
  `convention`, `command`, `failure_pattern`, `fact`), text, confidence,
  use_count, usefulness_score, and last_useful_at.
- `memory_proposals`: pending suggestions awaiting human review.
- `memory_citations`: which memories the model cited during which
  run, on which turn.
- `memory_conflicts`: ingest-time hits where a new memory's
  embedding was too close to an existing one with contradictory text.
- `repo_files`, `repo_files_fts`, `repo_manifests`,
  `repo_manifests_fts`: file-level indexes built by
  `kimetsu brain ingest repo`.
- `memories_fts`: FTS5 index of memory text for lexical retrieval.

## Durable upgrades: schema migrations

brain.db carries a schema version (`KIMETSU_SCHEMA_VERSION`, currently **3**)
in its `schema_info` table. On every read-write open, a versioned,
forward-only migration runner brings the DB up to the binary's target. Each
migration runs inside **one transaction** (the DDL and the version bump commit
together), so a crash mid-upgrade leaves the DB cleanly stamped at an
intermediate version rather than half-applied. Before any version-advancing
migration the runner takes an online-backup snapshot to a
`brain.db.bak-<from>-<to>-<ts>` sidecar next to the DB (skipped for empty
brains, since a fresh install has nothing to protect; the three newest backups are
kept). A read-only open of an un-migrated brain degrades gracefully: it reports
"needs migration" and the next read-write open performs it.

This DB schema version is **decoupled from the `project.toml` config version**
(`KIMETSU_CONFIG_VERSION`, still `1`). So `[kimetsu] schema_version = 1` in
`project.toml` is the *config-format* version, not the DB schema: the database
can evolve (and migrate) without forcing every project.toml to be rewritten.
The old "forward-additive `add_column_if_missing`, no rebuild" patches from
v0.1-v0.5 are now folded into the single v1→v2 migration.

## Memory kinds

| Kind | Use |
|------|-----|
| `preference` | User-stated style choices ("prefer thiserror") |
| `convention` | Repo conventions ("always run cargo fmt") |
| `command` | Useful shell incantations ("regen with `cargo xtask gen`") |
| `failure_pattern` | "Don't do X, it caused Y last time" |
| `fact` | Domain knowledge: APIs, gotchas, architectural notes |

## Memory scopes

| Scope | Lives | Use |
|-------|-------|-----|
| `run` | This run only | Ephemeral notes, discarded at end |
| `repo` | This repo | Project conventions, code-specific facts |
| `project` | This project (== repo today) | Synonym for repo |
| `global_user` | User-wide brain | Personal preferences, cross-project knowledge |

---
