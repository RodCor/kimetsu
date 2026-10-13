# Kimetsu desktop review, 2026-10-13

## Scope and provenance

Review baseline: `b5705fb201140696b739daac3de6ff1b8656716f` (2.8.0).
Local Git metadata identified RodCor/kimetsu, RodCor/kimetsu-pi,
RodCor/kimetsu-py, RodCor/kimetsu-bench and RodCor/kimetsu.dev; the local
research engine had no upstream remote. Work used isolated clones and preserved
the original core/research documentation and benchmark working-tree changes.
No unrelated data directory or private-final research vault was read.

## Pi compatibility

The npm stable version checked was `@earendil-works/pi-coding-agent@1.2.0`,
requiring Node >=22.19. The old callback assumed every before-agent-start event
had a prompt; the new event union also permits custom messages. The extension
now clears previous/pending memory and skips retrieval on custom turns.

[kimetsu-pi PR 13](https://github.com/RodCor/kimetsu-pi/pull/13) tested 36 cases,
including the real Pi loader/runner/session manager with the released Kimetsu
2.8.0 Windows lean CLI. Typecheck and npm audit passed. Independent review and
Linux/macOS/Windows CI passed before merge. Plugin 0.1.5 was published; this
core change synchronizes its canonical embedded extension. This is tested
coverage, not a claim about every possible host/version combination.

## Performance

Whole-code review concentrated on retrieval hydration/revision binding, capsule
expansion, delivery serialization, storage paths, and optional provider calls.
Two behavior-preserving changes had concrete justification: cache the repeated
claim-revision SQL statement, and read at most 2049 bytes when a file capsule
returns at most 2048. Other ideas lacked sufficient evidence for adoption.

`crates/kimetsu-brain/examples/review_perf.rs` exercises actual production
retrieval and capsule expansion without a model. Run with:

```sh
KIMETSU_USER_BRAIN=0 cargo run --release --locked -p kimetsu-brain \
  --no-default-features --example review_perf
```

Windows x86_64, Rust 1.97.0, release builds: baseline plus the benchmark alone
versus the same source with those two optimizations. Synthetic in-memory SQLite
has 1000 memories and ten revisions each; retrieval uses a 2048-token budget.
Each process warms each workload ten times, then measures five batches of 100
operations. Three processes per side ran in sequence A/B, B/A, A/B. The table
reports medians of 15 batch means (1500 timed operations per side/workload).
Warm file-cache results are intentional. Raw batch data and fingerprints are
included beside this report.

| Workload | Baseline microseconds | Candidate microseconds | Reduction |
| --- | ---: | ---: | ---: |
| Lexical retrieval | 45567.980 | 44166.160 | 3.1% |
| Recency fallback | 9774.508 | 5416.608 | 44.6% |
| 2048-byte file expansion | 77.193 | 71.508 | 7.4% |
| 32-MiB file expansion | 10949.579 | 68.392 | 99.4% |

Selected summaries, handles and claim revisions have identical fingerprints
across every batch. File output bytes also match. These fingerprints do not
cover every bundle field; regression tests cover snapshot/revision binding,
historical exposures, exact byte limits and UTF-8 boundary behavior. Small
lexical/file differences may be noise. No embedding, cold-disk, production
throughput, or answer-quality improvement is claimed.

## Auto-research results

Read-only review covered 68 experiments, 49 lineages and 51 revisions. There was
no adoption-ready positive result. Five reviewed candidates (diversity bundling,
completion handling and conflict-edge quarantine) had measured no gain.
Apparent fixture-only gains rejected by earlier reviews remain rejected;
missing holdouts, benchmark-shaped heuristics and removed public capsule IDs
are not acceptable evidence for production adoption.

The most relevant completion candidate,
`f629d96a5ed79632bc654c4c11dcc1766c6e4df5`, was freshly compared with baseline
`b5705fb201140696b739daac3de6ff1b8656716f` in three independent offline suite
pairs. The existing research engine's public `memory-core-v1` suite validated
source identities and all eight cases (six positive, two negative; three stale,
one conflict). All three pairs returned **no strict gain**:

| Metric | Baseline and candidate |
| --- | ---: |
| Quality | 0.7083333333 |
| Recall / MRR | 0.9166666667 |
| Abstention | 0.5 |
| False injection | 0.5 |
| Stale delivery | 0 |
| Conflict resolution | 1 |
| Total wire bytes | 4879 |

Fresh comparisons are preserved in `research-retest.jsonl`. Timing is not used
as evidence because some runs overlapped a build. The unresolved missing-port
case and false irrelevant-memory injection remain limitations. No candidate
was promoted, no model was called, and live research state was not changed.

Historical selected-only MCP experiment
`experiment-df24d7e08a74460b914519748edd6e03` reduced serialized bytes from 38190
to 6273 with the same three summaries, but never established tokenizer/budget
quality. Current compact capsule delivery already supersedes that mechanism;
the old patch was not reintroduced.

## Optional decision providers

[Setup and contracts](../../../examples/decisions/README.md) document TypeSafe
Jev, OpenAI Decisions and Perplexity Decisions with official source links.
The implementation is a `brain context` post-retrieval pass, enabled only by
configured provider plus `--decisions --decision-min-probability <value>`.
Defaults and Pi/MCP/hooks are unchanged. Requests expose selected summaries
and query to the chosen service and can incur charges when users opt in.

Tests exercise typed contracts, strict response validation, transport via a
loopback mock, no redirects, bounded input/output, abstention preservation,
budget/conflict invariants, CLI opt-in and missing credentials. Python probe
tests additionally reject malformed credentials without revealing them.
There was no hosted API call: access, quality, calibration, cost and real-service
latency remain unvalidated. No threshold is represented as calibrated, and
these typed decisions cannot substitute for the generation distiller.
