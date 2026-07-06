The bridge, brain sharing, doctor, and what Kimetsu is not.

## The bridge

Kimetsu also runs as a cross-harness skill bridge. `kimetsu bridge` discovers
skills installed in supported hosts (Claude Code, Codex, Pi, OpenClaw, the
local kimetsu installation), exports a skill from one host into another, and
keeps a unified registry so the same skill works everywhere.
`kimetsu bridge status` shows what is installed where;
`kimetsu bridge export <skill> --to <target>` does the install.

---

## Sharing brains

The bridge moves skills. Memories move as **packs**: a brain exported to one
portable file that another brain can install.

```bash
kimetsu brain export team.json.gz --name rust-conventions --version 1.0.0
kimetsu brain import team.json.gz                        # merge (default)
kimetsu brain import https://example.com/pack.json.gz    # install from a URL
kimetsu brain import other.json.gz --mode replace --yes  # swap
```

- **Export** always compresses (gzip) and always scrubs: credentials and PII
  are redacted before the pack is written. `--strict` aborts on a finding;
  `--name` / `--version` / `--description` stamp a manifest.
- **Import** reads a file, stdin, or a URL, and detects gzip on its own.
  `merge` (default) dedups against what you have, so importing twice is safe.
  `--mode replace` supersedes your memories in the pack's scope first; it
  needs `--yes` and invalidates rather than deletes, so a swap is reversible.
  Imports are scrubbed again on the way in.

For continuous replication, `kimetsu brain sync` exports and imports the
durable event log (idempotent per event), converging two brains without
copying SQLite files. Kimetsu Remote serves a shared brain per repository.

---

## Doctor

`kimetsu doctor` validates that every subsystem works against the current
workspace: paths resolve, brain.db opens with a matching schema, the user
brain is reachable (or correctly disabled), the embedder loads, the MCP
server spawns, the bridge can scan hosts. Hermetic, safe in CI, `--json` for
hooks.

`kimetsu doctor --selftest` is the end-to-end proof: in a throw-away temp
project it records a memory and retrieves it back, exiting non-zero if any
step fails.

---

## What kimetsu is NOT

- Not a model. It runs through a host agent or a configured provider.
- Not a sandbox. Tools run on the host machine.
- Not an external vector DB. The brain is one SQLite file per project; on the
  embeddings build the ANN index is a `brain.usearch` sidecar next to it.
  Backups are `cp brain.db`, and the brain auto-backs-up before any schema
  migration.

---

## Where to go next

- `kimetsu doctor` to verify your install.
- The **CHANGELOG** for when each piece landed.
- Per-crate `src/lib.rs` doc comments for module-level detail.
