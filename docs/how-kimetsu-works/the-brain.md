brain.db: the event-sourced SQLite file that holds everything Kimetsu remembers, and how it migrates safely.

Everything kimetsu remembers lives in **brain.db**, a single SQLite file per
project at `<project>/.kimetsu/brain.db`. A global user brain at
`~/.kimetsu/brain.db` holds memories that follow you across projects
(disable with `KIMETSU_USER_BRAIN=0` or `[kimetsu] use_user_brain = false`).

`.kimetsu/` stays lean: just `brain.db` (plus WAL and migration backups) and
`project.toml`. Transient working dirs live under
`~/.kimetsu/cache/<project-hash>/`, never in your tree.

The brain is event-sourced: the **`events` table is the durable log**, and a
projector replays it into materialized tables the broker queries fast.
`kimetsu brain rebuild` re-derives every projection from the log. The tables:

- `runs`: one row per agent run.
- `events`: every event ever written; the source for rebuild.
- `memories`: the durable knowledge, with scope, kind, text, confidence,
  use_count, usefulness_score, and last_useful_at.
- `memory_proposals`: pending suggestions awaiting review.
- `memory_citations`: which memories the model cited, in which run.
- `memory_conflicts`: ingest-time contradiction hits.
- `repo_files*`, `repo_manifests*`: file indexes from `brain ingest repo`.
- `memories_fts`: FTS5 index for lexical retrieval.

## Durable upgrades: schema migrations

brain.db carries a schema version, and a forward-only migration runner brings
it up to the binary's target on every read-write open. Each migration runs in
one transaction, so a crash leaves the DB cleanly stamped at an intermediate
version, never half-applied. Before any migration the runner snapshots to a
`brain.db.bak-*` sidecar (three newest kept). A read-only open of an
un-migrated brain reports "needs migration" instead of failing.

The DB schema version is decoupled from the `project.toml` config version, so
the database can evolve without rewriting every project's config file.

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
