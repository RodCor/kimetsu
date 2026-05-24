# kimetsu_harbor/archive — one-shot scripts that already ran

This folder holds gauntlet-runner scripts, cron probes, and one-time
data migrations. They're preserved in git so we can re-run a specific
historical experiment if needed, but they're out of
`kimetsu_harbor/` proper so the directory listing reflects what an
active user needs (`kimetsu_agent.py`, `codex_kimetsu_agent.py`,
`smoke_test.py`, the MCP shim, the setup docs).

## What's here

| file | purpose |
|---|---|
| `run-codex-16x3.ps1` | Windows PowerShell wrapper for the 16-task × 3-mode codex gauntlet. Last run 2026-05-22. |
| `run-codex-kimetsu-bench.sh` | Linux/WSL twin of the above. |
| `run-codex-kimetsu-wsl.ps1` | PowerShell shim that invokes the WSL bench script. |
| `povray-variance.sh` | Single-task probe used to measure variance on the povray Terminal-Bench task. |
| `stability-cron.sh` | Cron-driven stability sweep (MP-15c). |
| `stability-report.sh` | Reporter for the stability cron's output. |
| `restore-shell-memories.sh` | One-shot data-migration script that restored MP-17b shell-workflow memories after a memory-pool rotation. |
| `seed-tool-memories.sh` | One-shot seeder for the MP-17j typed-tool memories. |
| `kimetsu-brain-context.sh` | Composite shell wrapper that printed brain context in a specific MP-X probe format. Superseded by `kimetsu brain context --json`. |

## Why archived, not deleted

All of these are working scripts that produced specific historical
results. Deleting them would orphan the result data in
`docs/archive/MP-*-RESULTS.md` (no way to re-run from source). The
archive folder keeps them runnable while signaling "not part of the
active toolchain."

## Re-running a historical experiment

```bash
# Re-run the MP-17 gauntlet path on this commit:
bash kimetsu_harbor/archive/run-codex-kimetsu-bench.sh

# Probe povray variance with the same script the MP-15 run used:
bash kimetsu_harbor/archive/povray-variance.sh
```

Most of these scripts have hard-coded paths or env-var assumptions
from when they were originally run — expect to tweak.
