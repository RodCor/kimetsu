# E0 sweep artifacts (issue #68)

Committed copies of the E0+sweep run1 / run2 summaries, the run1-vs-run2 metric
deltas, the fusion attribution for those runs, and the commands needed to
regenerate both from the committed files.

These files live under `docs/audits/` because that is the repository's existing
home for committed measurement artifacts (for example
`docs/audits/2026-09-05-brainbench/`). A `bench/results/` path is not usable:
the repository gitignores `/bench/` (reserved for the private kimetsu-bench
clone) and also gitignores `/e0s-results/` (the default output of
`scripts/experiments/e0s/run_e0s.sh`).

Scorer: `scripts/retrieval_eval.py` rank metrics, invoked via
`scripts/e0_eval.py --profile bench-sweep`. Retrieval only. Not an official
judge, and no reader.

## Binary and runners

- Binary: `kimetsu-e0s`, sha256
  `262f17def1cfd67f07aaec325a2f2d23aa058d393adefc463d4bfc4c33391985`
  (kimetsu 2.11.0, embeddings). On the box:
  `/workspace/fuse/e0s/bin/kimetsu-e0s`.
- Run1 runner: `run_e0s.sh` at worktree commit `cc7b729`, sha256
  `4b4155f1f352a76d421fe0e4152b30be6ef0ad54a41897ee0622e04c4c48605a`.
- Run2 runner: `scripts/experiments/e0s/rerun_run2.sh`, added in PR #79,
  sha256 `0580084a8586919a84c61734672ef8f043cd9fe0753bde5554c44b196eca7545`.
- Wrapper hashes for the run1 tree (scripts and prepared data):
  [wrapper-sha256.txt](wrapper-sha256.txt).
- Full box-path manifest, including the cargo build line and eval environment:
  [MANIFEST.md](MANIFEST.md).

## Files

| File | What it is |
|---|---|
| [MANIFEST.md](MANIFEST.md) | Run manifest (commands, env, binary, data paths). |
| [wrapper-sha256.txt](wrapper-sha256.txt) | SHA-256 of the wrappers and data used for run1. |
| [determinism-diff.md](determinism-diff.md) | Row-level run1 vs run2 comparison (1099 of 6291 rows changed). |
| [run1-run2-delta.md](run1-run2-delta.md), [run1-run2-delta.json](run1-run2-delta.json) | All-split run1 vs run2 metric deltas from `run{1,2}/summary.json`. Overall max \|Δ R@8\| is 0.025381 (`locomo-conv26` `rrf_k60`); across all non-rrf arms, \|Δ R@8\| is at most 0.004175. |
| [resolved-config-per-arm.json](resolved-config-per-arm.json) | Resolved broker / embedder config for each arm. |
| [SHA256SUMS](SHA256SUMS) | SHA-256 of every committed file here, plus commented hashes of the uncompressed `lme-oracle.jsonl` files. |
| [attribution-run1.md](attribution-run1.md), [attribution-run1.json](attribution-run1.json) | Fusion attribution on run1 (kill rule is evaluated here). |
| [attribution-run2.json](attribution-run2.json) | Same analysis on run2, as a stability check. |
| `run1/`, `run2/` | Per-run `summary.md` / `summary.json`, `capsule-hist.md` / `capsule-hist.json`, `*.meta.json`, `resolved-config-*.json`, `locomo-conv26.jsonl`, and `lme-oracle.jsonl.gz`. |

Stdout logs (`*.stdout.log`, `resolved-config-*.log`) are not included:
the repository gitignores `*.log`.

### Per-question JSONL and prepared gold sets

- `run1/locomo-conv26.jsonl` and `run2/locomo-conv26.jsonl` are committed
  uncompressed (about 700 KiB each).
- `run1/lme-oracle.jsonl.gz` (108,949 bytes) and `run2/lme-oracle.jsonl.gz`
  (108,530 bytes) are committed gzipped (`gzip -9n`). Both the `.gz` hashes and
  the uncompressed JSONL hashes are listed in [SHA256SUMS](SHA256SUMS).

Prepared gold sets are not in this public repository. On the box:

- `/workspace/fuse/e0/data/locomo-conv26.json` sha256 `a996cc78f35f78d24114800636e4a2425ddf60639254532bfacff35a2013dcd7`
- `/workspace/fuse/e0/data/lme-oracle.json` sha256 `1dcfd431b1a145d2a234e07d1a72f246fbc37019bb4e3409a6a191beb965b129`

## Rerun delta table and attribution

Regenerate `run1-run2-delta.{md,json}` from the committed summaries:

```bash
python3 scripts/experiments/e0s/summary_delta.py \
  --run1 docs/audits/2026-11-e0s/run1/summary.json \
  --run2 docs/audits/2026-11-e0s/run2/summary.json \
  --out-dir docs/audits/2026-11-e0s
```

Regenerate the attribution from a checkout (reads `lme-oracle.jsonl.gz`
directly; `--results`, `--data`, and `--out-dir` are required):

```bash
python3 scripts/experiments/e0s/fusion_attribution.py \
  --results docs/audits/2026-11-e0s \
  --data /path/to/prepared/data \
  --out-dir /tmp/attribution-rerun
```

The committed `attribution-run1.{md,json}` and `attribution-run2.json` were
originally written by `fusion_attribution.py` at sha256
`c2fc141d9128eccccd69b670dd5fe2c54254a624020851d655a3d6a32fecbdcb`. The current
committed script (`scripts/experiments/e0s/fusion_attribution.py`, sha256
`6bd9f5255e943a400189d2eed45830dd7fd69856b36ec918bb9010bd203f343b`) adds
`--self-test`, `.jsonl.gz` input support, and required path flags, and
reproduces the exact same counts from `docs/audits/2026-11-e0s`.
