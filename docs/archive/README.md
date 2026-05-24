# docs/archive — historical kimetsu planning + run logs

This folder holds documents that record HOW we got here, not WHERE
we are. They stay in git so the journey is recoverable, but they're
out of `docs/` proper so new readers see only the current-state
docs (`V0.4-ROADMAP.md`, `V0.3.4-SHIP.md`, `KIMETSU-CHAT.md`,
`MEMORY-*.md`, `SWEBENCH.md`).

## What's here

| file | era | summary |
|---|---|---|
| `MVP.md` | v0.1 | The original 1,800-line vision doc kimetsu was kicked off against. Mostly historical; everything actionable was extracted into later plans. |
| `V0.2-PLAN.md` | v0.2 | Pre-ship plan for the Terminal-Bench tool surface, the 20-tool catalog, the auto-orient pre-shell. |
| `V0.2-SHIP.md` | v0.2 | What v0.2 actually shipped, with the MP-8 / MP-13 / MP-14 result data. |
| `V0.3-PLAN.md` | v0.3 | Pre-ship architecture plan for splitting harbor/chat/agent. |
| `MP-4-VERDICT.md` | MP-4 | The Anthropic provider rebuild; closed by MP-13f's retry-on-5xx work. |
| `MP-8-VERDICT.md` | MP-8 | The bare-Claude-Code baseline that defined the 18.75pp accuracy gap kimetsu eventually beat. |
| `MP-10-RESULTS.md` | MP-10 | First brain-on/brain-off gauntlet. |
| `MP-11-RESULTS.md` | MP-11 | Tool-surface gap analysis vs bare Claude Code. |
| `MP-12-RESULTS.md` | MP-12 | Composed-tool surface trial. |
| `MP-13-RESULTS.md` | MP-13 | Harness improvements gauntlet. |
| `MP-13G-RESULTS.md` | MP-13g | Brain rerun after retry-on-5xx; first time brain reached parity. |
| `MP-14-RESULTS.md` | MP-14 | First clean "brain > no-brain" margin: 7/16 vs 6/16. |
| `MP-15-PLAN.md` | MP-15 | Timeout / budget / variance / stability cron plan. |

## Useful queries

Search across the archive for a specific term:

```bash
grep -ril 'KIMETSU_CLAUDE_PERSISTENT' docs/archive/
```

Time-machine to a specific MP's state:

```bash
git log --diff-filter=A --follow -- docs/archive/MP-13G-RESULTS.md
```
