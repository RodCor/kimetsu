# E0 step-1 fusion attribution (run1, with run2 stability)

retrieval-only numbers, not an official judge. gold hit = retrieved `labels` entry in the question's evidence set, same as `scripts/retrieval_eval.py:rank_metrics`. counts are over scored questions only (`scripts/e0_eval.py`). top 8 = `labels[:8]`.

hypothesis: unbounded bm25 on FTS candidates lets lexical hits outrank vector-only gold, so `tinybert_floor_0_fill_to_cap` (and `fused_defaults`) lose gold that the vector arm holds in its top 8.

kill rule (gauge): if fewer than 20 conv-26 questions are affected (vector has ≥1 gold in top 8 that `tinybert_floor_0_fill_to_cap` misses), fusion is not the main loss.

**kill rule does not fire.** conv-26 run1 affected questions vs fill_to_cap: 64 (threshold 20). at least 20 conv-26 questions are affected; the fusion arm stays live.

## run1

### locomo-conv26

n = 199 questions, 197 scored (from `/workspace/fuse/e0s/results/run1/locomo-conv26.jsonl`, gold `/workspace/fuse/e0/data/locomo-conv26.json`).

### run1 / locomo-conv26: vector vs tinybert_floor_0_fill_to_cap

- scored questions: 197 (measured)
- questions where vector has ≥1 gold in top 8: 132 (measured)
- questions where tinybert_floor_0_fill_to_cap has ≥1 gold in top 8: 95 (measured)
- questions where vector has ≥1 gold in top 8 that tinybert_floor_0_fill_to_cap misses: 64 (measured)
- lost gold items: 71 (measured)
- of those lost items, vector-only (not in lexical top 8): 57 items across 53 questions (measured)
- of those lost items, also in lexical top 8: 14 items across 14 questions (measured)
- reverse: questions where tinybert_floor_0_fill_to_cap has ≥1 gold in top 8 that vector misses: 24 (measured)
- reverse: gained gold items: 24 (measured)
- net gold items (lost − gained): 47 (measured)
- binary R@8 flips (retrieval_eval definition, ≥1 gold in top 8): vector=1 other=0 on 61 questions; vector=0 other=1 on 24; net question change -37 (measured)
- per-capsule scores/ranks: not available: jsonl rows have no per-capsule scores or ranks; only ordered labels and a first-hit rank
- crowding (questions with a vector-only loss, n=53): tinybert_floor_0_fill_to_cap returned median 8.000 capsules (mean 8.000); of those, median 5.000 (mean 5.151) were also in lexical's top 8; distribution of overlap 0–8: {1:1, 3:6, 4:12, 5:11, 6:14, 7:5, 8:4} (measured)

### run1 / locomo-conv26: vector vs fused_defaults

- scored questions: 197 (measured)
- questions where vector has ≥1 gold in top 8: 132 (measured)
- questions where fused_defaults has ≥1 gold in top 8: 88 (measured)
- questions where vector has ≥1 gold in top 8 that fused_defaults misses: 65 (measured)
- lost gold items: 72 (measured)
- of those lost items, vector-only (not in lexical top 8): 57 items across 53 questions (measured)
- of those lost items, also in lexical top 8: 15 items across 15 questions (measured)
- reverse: questions where fused_defaults has ≥1 gold in top 8 that vector misses: 17 (measured)
- reverse: gained gold items: 17 (measured)
- net gold items (lost − gained): 55 (measured)
- binary R@8 flips (retrieval_eval definition, ≥1 gold in top 8): vector=1 other=0 on 61 questions; vector=0 other=1 on 17; net question change -44 (measured)
- per-capsule scores/ranks: not available: jsonl rows have no per-capsule scores or ranks; only ordered labels and a first-hit rank
- crowding (questions with a vector-only loss, n=53): fused_defaults returned median 2.000 capsules (mean 3.660); of those, median 2.000 (mean 2.717) were also in lexical's top 8; distribution of overlap 0–8: {1:24, 2:10, 3:3, 4:5, 5:4, 6:1, 7:3, 8:3} (measured)

### lme-oracle

n = 500 questions, 479 scored (from `/workspace/fuse/e0s/results/run1/lme-oracle.jsonl`, gold `/workspace/fuse/e0/data/lme-oracle.json`).

### run1 / lme-oracle: vector vs tinybert_floor_0_fill_to_cap

- scored questions: 479 (measured)
- questions where vector has ≥1 gold in top 8: 456 (measured)
- questions where tinybert_floor_0_fill_to_cap has ≥1 gold in top 8: 419 (measured)
- questions where vector has ≥1 gold in top 8 that tinybert_floor_0_fill_to_cap misses: 146 (measured)
- lost gold items: 178 (measured)
- of those lost items, vector-only (not in lexical top 8): 148 items across 122 questions (measured)
- of those lost items, also in lexical top 8: 30 items across 30 questions (measured)
- reverse: questions where tinybert_floor_0_fill_to_cap has ≥1 gold in top 8 that vector misses: 82 (measured)
- reverse: gained gold items: 87 (measured)
- net gold items (lost − gained): 91 (measured)
- binary R@8 flips (retrieval_eval definition, ≥1 gold in top 8): vector=1 other=0 on 53 questions; vector=0 other=1 on 16; net question change -37 (measured)
- per-capsule scores/ranks: not available: jsonl rows have no per-capsule scores or ranks; only ordered labels and a first-hit rank
- crowding (questions with a vector-only loss, n=122): tinybert_floor_0_fill_to_cap returned median 8.000 capsules (mean 8.000); of those, median 6.000 (mean 5.533) were also in lexical's top 8; distribution of overlap 0–8: {1:1, 2:4, 3:10, 4:15, 5:18, 6:40, 7:26, 8:8} (measured)

### run1 / lme-oracle: vector vs fused_defaults

- scored questions: 479 (measured)
- questions where vector has ≥1 gold in top 8: 456 (measured)
- questions where fused_defaults has ≥1 gold in top 8: 392 (measured)
- questions where vector has ≥1 gold in top 8 that fused_defaults misses: 209 (measured)
- lost gold items: 253 (measured)
- of those lost items, vector-only (not in lexical top 8): 155 items across 129 questions (measured)
- of those lost items, also in lexical top 8: 98 items across 91 questions (measured)
- reverse: questions where fused_defaults has ≥1 gold in top 8 that vector misses: 58 (measured)
- reverse: gained gold items: 61 (measured)
- net gold items (lost − gained): 192 (measured)
- binary R@8 flips (retrieval_eval definition, ≥1 gold in top 8): vector=1 other=0 on 76 questions; vector=0 other=1 on 12; net question change -64 (measured)
- per-capsule scores/ranks: not available: jsonl rows have no per-capsule scores or ranks; only ordered labels and a first-hit rank
- crowding (questions with a vector-only loss, n=129): fused_defaults returned median 8.000 capsules (mean 5.845); of those, median 5.000 (mean 4.155) were also in lexical's top 8; distribution of overlap 0–8: {0:1, 1:26, 2:15, 3:10, 4:10, 5:15, 6:32, 7:17, 8:3} (measured)

## run2

### locomo-conv26

n = 199 questions, 197 scored (from `/workspace/fuse/e0s/results/run2/locomo-conv26.jsonl`, gold `/workspace/fuse/e0/data/locomo-conv26.json`).

### run2 / locomo-conv26: vector vs tinybert_floor_0_fill_to_cap

- scored questions: 197 (measured)
- questions where vector has ≥1 gold in top 8: 132 (measured)
- questions where tinybert_floor_0_fill_to_cap has ≥1 gold in top 8: 95 (measured)
- questions where vector has ≥1 gold in top 8 that tinybert_floor_0_fill_to_cap misses: 65 (measured)
- lost gold items: 72 (measured)
- of those lost items, vector-only (not in lexical top 8): 57 items across 53 questions (measured)
- of those lost items, also in lexical top 8: 15 items across 15 questions (measured)
- reverse: questions where tinybert_floor_0_fill_to_cap has ≥1 gold in top 8 that vector misses: 24 (measured)
- reverse: gained gold items: 24 (measured)
- net gold items (lost − gained): 48 (measured)
- binary R@8 flips (retrieval_eval definition, ≥1 gold in top 8): vector=1 other=0 on 61 questions; vector=0 other=1 on 24; net question change -37 (measured)
- per-capsule scores/ranks: not available: jsonl rows have no per-capsule scores or ranks; only ordered labels and a first-hit rank
- crowding (questions with a vector-only loss, n=53): tinybert_floor_0_fill_to_cap returned median 8.000 capsules (mean 8.000); of those, median 5.000 (mean 5.226) were also in lexical's top 8; distribution of overlap 0–8: {1:1, 3:5, 4:11, 5:13, 6:14, 7:4, 8:5} (measured)

### run2 / locomo-conv26: vector vs fused_defaults

- scored questions: 197 (measured)
- questions where vector has ≥1 gold in top 8: 132 (measured)
- questions where fused_defaults has ≥1 gold in top 8: 88 (measured)
- questions where vector has ≥1 gold in top 8 that fused_defaults misses: 65 (measured)
- lost gold items: 72 (measured)
- of those lost items, vector-only (not in lexical top 8): 57 items across 53 questions (measured)
- of those lost items, also in lexical top 8: 15 items across 15 questions (measured)
- reverse: questions where fused_defaults has ≥1 gold in top 8 that vector misses: 17 (measured)
- reverse: gained gold items: 17 (measured)
- net gold items (lost − gained): 55 (measured)
- binary R@8 flips (retrieval_eval definition, ≥1 gold in top 8): vector=1 other=0 on 61 questions; vector=0 other=1 on 17; net question change -44 (measured)
- per-capsule scores/ranks: not available: jsonl rows have no per-capsule scores or ranks; only ordered labels and a first-hit rank
- crowding (questions with a vector-only loss, n=53): fused_defaults returned median 2.000 capsules (mean 3.774); of those, median 2.000 (mean 2.830) were also in lexical's top 8; distribution of overlap 0–8: {1:24, 2:9, 3:2, 4:5, 5:4, 6:4, 7:2, 8:3} (measured)

### lme-oracle

n = 500 questions, 479 scored (from `/workspace/fuse/e0s/results/run2/lme-oracle.jsonl`, gold `/workspace/fuse/e0/data/lme-oracle.json`).

### run2 / lme-oracle: vector vs tinybert_floor_0_fill_to_cap

- scored questions: 479 (measured)
- questions where vector has ≥1 gold in top 8: 456 (measured)
- questions where tinybert_floor_0_fill_to_cap has ≥1 gold in top 8: 421 (measured)
- questions where vector has ≥1 gold in top 8 that tinybert_floor_0_fill_to_cap misses: 146 (measured)
- lost gold items: 177 (measured)
- of those lost items, vector-only (not in lexical top 8): 147 items across 121 questions (measured)
- of those lost items, also in lexical top 8: 30 items across 30 questions (measured)
- reverse: questions where tinybert_floor_0_fill_to_cap has ≥1 gold in top 8 that vector misses: 82 (measured)
- reverse: gained gold items: 87 (measured)
- net gold items (lost − gained): 90 (measured)
- binary R@8 flips (retrieval_eval definition, ≥1 gold in top 8): vector=1 other=0 on 51 questions; vector=0 other=1 on 16; net question change -35 (measured)
- per-capsule scores/ranks: not available: jsonl rows have no per-capsule scores or ranks; only ordered labels and a first-hit rank
- crowding (questions with a vector-only loss, n=121): tinybert_floor_0_fill_to_cap returned median 8.000 capsules (mean 8.000); of those, median 6.000 (mean 5.537) were also in lexical's top 8; distribution of overlap 0–8: {1:1, 2:4, 3:9, 4:16, 5:18, 6:39, 7:26, 8:8} (measured)

### run2 / lme-oracle: vector vs fused_defaults

- scored questions: 479 (measured)
- questions where vector has ≥1 gold in top 8: 456 (measured)
- questions where fused_defaults has ≥1 gold in top 8: 394 (measured)
- questions where vector has ≥1 gold in top 8 that fused_defaults misses: 209 (measured)
- lost gold items: 252 (measured)
- of those lost items, vector-only (not in lexical top 8): 154 items across 128 questions (measured)
- of those lost items, also in lexical top 8: 98 items across 91 questions (measured)
- reverse: questions where fused_defaults has ≥1 gold in top 8 that vector misses: 59 (measured)
- reverse: gained gold items: 62 (measured)
- net gold items (lost − gained): 190 (measured)
- binary R@8 flips (retrieval_eval definition, ≥1 gold in top 8): vector=1 other=0 on 74 questions; vector=0 other=1 on 12; net question change -62 (measured)
- per-capsule scores/ranks: not available: jsonl rows have no per-capsule scores or ranks; only ordered labels and a first-hit rank
- crowding (questions with a vector-only loss, n=128): fused_defaults returned median 8.000 capsules (mean 5.797); of those, median 5.000 (mean 4.133) were also in lexical's top 8; distribution of overlap 0–8: {0:1, 1:27, 2:14, 3:8, 4:12, 5:16, 6:30, 7:18, 8:2} (measured)

## run1 vs run2 stability

- locomo-conv26 vs fill_to_cap: affected questions 64 → 65; lost items 71 → 72; vector-only items 57 → 57; net items 47 → 48 (measured)
- locomo-conv26 vs fused_defaults: affected questions 65 → 65; lost items 72 → 72; vector-only items 57 → 57; net items 55 → 55 (measured)
- lme-oracle vs fill_to_cap: affected questions 146 → 146; lost items 178 → 177; vector-only items 148 → 147; net items 91 → 90 (measured)
- lme-oracle vs fused_defaults: affected questions 209 → 209; lost items 253 → 252; vector-only items 155 → 154; net items 192 → 190 (measured)

## inputs and rerun

- script: `/workspace/fuse/e0s/attribution/fusion_attribution.py` sha256 `c2fc141d9128eccccd69b670dd5fe2c54254a624020851d655a3d6a32fecbdcb`
- inputs (sha256):
  - `/workspace/fuse/e0/data/lme-oracle.json`: `1dcfd431b1a145d2a234e07d1a72f246fbc37019bb4e3409a6a191beb965b129`
  - `/workspace/fuse/e0/data/locomo-conv26.json`: `a996cc78f35f78d24114800636e4a2425ddf60639254532bfacff35a2013dcd7`
  - `/workspace/fuse/e0s/results/run1/lme-oracle.jsonl`: `e6287fb20b4c984d65e065d57aa7e63769f2ae95d96663269519d568f8434eab`
  - `/workspace/fuse/e0s/results/run1/locomo-conv26.jsonl`: `37a370eef7b7b614070a8d25620cb06bf32affdbcfb3417ad2e55199d288a3af`
  - `/workspace/fuse/e0s/results/run2/lme-oracle.jsonl`: `4d74e27d5241744d02c3bc982ae600ecf82a136a0955a94e1e2d89e3c60743e6`
  - `/workspace/fuse/e0s/results/run2/locomo-conv26.jsonl`: `d095b04b7999f65dbd8ea1116e56679a538ed507eccecae63b416e482f5fffe8`
- rerun: `python3 /workspace/fuse/e0s/attribution/fusion_attribution.py --results /workspace/fuse/e0s/results --data /workspace/fuse/e0/data --out-dir /workspace/fuse/e0s/attribution`
