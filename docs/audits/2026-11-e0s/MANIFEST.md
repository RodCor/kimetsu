# E0+sweep manifest (run1 2026-11-13; run2 rerun 2026-11-20 via /workspace/fuse/e0s/rerun_run2.sh, worktree HEAD cc7b7299709a874f5930dff1f0c47a224b8a6200)

- spec: gauge, ML room 2026-11-13 (E0+sweep, #68). runner: /workspace/fuse/e0s/run_e0s.sh under /workspace/fuse/e0s/memcap.py (5 GiB tree RSS cap), nice 19
- run2 rerun started: 2026-11-20 16:51:02 UTC; finished: 2026-11-20 19:32:01 -03. run1 log: /workspace/fuse/e0s/run_e0s.log
- code: RodCor/kimetsu main @ 19112dae937fb3b1842983943a0def6fad75f4b1; worktree /workspace/fuse/e0s/kimetsu @ 61dc9f01a60999d18d8614e0337272f893643fa0 (branch cursor/e0-sweep-profile, scripts-only diff, not pushed)
- binary: /workspace/fuse/e0s/bin/kimetsu-e0s, kimetsu 2.11.0 (embeddings), sha256 262f17def1cfd67f07aaec325a2f2d23aa058d393adefc463d4bfc4c33391985; build log /workspace/fuse/e0s/logs/build.log
- build (run1 only; run2 reused the binary): (cd /workspace/fuse/e0s/kimetsu && env PATH=/home/box/.cargo/bin:/workspace/fuse/e0s/toolchain/bin:/usr/local/bin:/usr/bin:/bin CXX=/workspace/fuse/e0s/toolchain/zig-cxx OPENSSL_INCLUDE_DIR=/usr/include OPENSSL_LIB_DIR=/usr/lib/x86_64-linux-gnu LIBRARY_PATH=/workspace/fuse/e0s/toolchain/lib CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/workspace/fuse/e0s/target nice -n 19 /home/box/.cargo/bin/cargo rustc -p kimetsu-cli --bin kimetsu --release --locked --features embeddings -j 2 -- -C link-arg=-L/workspace/fuse/e0s/toolchain/lib -C link-arg=-lc++ -C link-arg=-lc++abi -C link-arg=-lunwind)
- data: /workspace/fuse/e0/data/locomo-conv26.json (199 q / 197 scored), /workspace/fuse/e0/data/lme-oracle.json (500 q / 479 scored)
- scorer: /workspace/fuse/e0s/kimetsu/scripts/retrieval_eval.py metrics via scripts/e0_eval.py --profile bench-sweep; retrieval only, no reader, NOT an official judge
- seed: none (expected deterministic; determinism-diff.md shows it is not: 1099/6291 rows changed); PYTHONHASHSEED=0. threads: 1 ONNX thread
- latency/RSS: NOT comparable; do not report
- rrf: only k=60 (fusion.rs:188 Fusion::Rrf => rrf_fuse(lists, DEFAULT_RRF_K), DEFAULT_RRF_K=60.0 at fusion.rs:64; no config knob)
- eval env: env PYTHONHASHSEED=0 KIMETSU_INTRA_THREADS=1 OMP_NUM_THREADS=1 OPENBLAS_NUM_THREADS=1 FASTEMBED_CACHE_DIR=/workspace/fuse/e0s/model-cache HF_HOME=/workspace/fuse/e0s/model-cache KIMETSU_USER_BRAIN=0 KIMETSU_BRAIN_AMBIENT=off KIMETSU_BRAIN_LOG_RETRIEVAL=0 PATH=/usr/local/bin:/usr/bin:/bin
- eval commands:
  - TMPDIR=/workspace/fuse/e0s/tmp/run1 nice -n 19 /usr/bin/python3 -u /workspace/fuse/e0s/kimetsu/scripts/e0_eval.py --bin /workspace/fuse/e0s/bin/kimetsu-e0s --profile bench-sweep --name locomo-conv26 --dataset /workspace/fuse/e0/data/locomo-conv26.json --out /workspace/fuse/e0s/results/run1/locomo-conv26.jsonl
  - TMPDIR=/workspace/fuse/e0s/tmp/run1 nice -n 19 /usr/bin/python3 -u /workspace/fuse/e0s/kimetsu/scripts/e0_eval.py --bin /workspace/fuse/e0s/bin/kimetsu-e0s --profile bench-sweep --name lme-oracle --dataset /workspace/fuse/e0/data/lme-oracle.json --out /workspace/fuse/e0s/results/run1/lme-oracle.jsonl
  - TMPDIR=/workspace/fuse/e0s/tmp/run2 nice -n 19 /usr/bin/python3 -u /workspace/fuse/e0s/kimetsu/scripts/e0_eval.py --bin /workspace/fuse/e0s/bin/kimetsu-e0s --profile bench-sweep --name locomo-conv26 --dataset /workspace/fuse/e0/data/locomo-conv26.json --out /workspace/fuse/e0s/results/run2/locomo-conv26.jsonl
  - TMPDIR=/workspace/fuse/e0s/tmp/run2 nice -n 19 /usr/bin/python3 -u /workspace/fuse/e0s/kimetsu/scripts/e0_eval.py --bin /workspace/fuse/e0s/bin/kimetsu-e0s --profile bench-sweep --name lme-oracle --dataset /workspace/fuse/e0/data/lme-oracle.json --out /workspace/fuse/e0s/results/run2/lme-oracle.jsonl
- results: /workspace/fuse/e0s/results/run{1,2} (JSONL, meta, stdout, summary.md/json, capsule-hist.md/json), /workspace/fuse/e0s/results/determinism-diff.md (exit 1), /workspace/fuse/e0s/results/resolved-config-per-arm.json
- hashes: /workspace/fuse/e0s/wrapper-sha256.txt. memcap log: /workspace/fuse/e0s/memcap.log. MemAvailable trace: /workspace/fuse/e0s/logs/memavailable.log

## run1 summary

# E0 summary: run1

scorer: scripts/retrieval_eval.py metrics (retrieval only, NOT an official judge, no reader). recall/MRR over scored questions; empty rate and capsules over all questions. vector arm = script extra (cosine over stored blobs, not a binary path).

## lme-oracle

| arm | type | n | scored | R@1 | R@3 | R@8 | MRR | empty | empty rate | caps min/med/p90/max | caps mean | errors | peak RSS MiB |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---|---:|---:|---:|
| lexical | all | 500 | 479 | 0.451 | 0.678 | 0.881 | 0.591 | 0 | 0.000 | 2/8.0/8/8 | 7.90 | 0 | 25.4 |
| lexical | abstention | 30 | 9 | 0.333 | 0.333 | 0.778 | 0.416 | 0 | 0.000 | 8/8.0/8/8 | 8.00 | 0 | 23.8 |
| lexical | knowledge-update | 72 | 72 | 0.528 | 0.764 | 0.917 | 0.668 | 0 | 0.000 | 8/8.0/8/8 | 8.00 | 0 | 23.2 |
| lexical | multi-session | 121 | 121 | 0.372 | 0.620 | 0.818 | 0.518 | 0 | 0.000 | 8/8/8/8 | 8.00 | 0 | 24.7 |
| lexical | single-session-assistant | 56 | 56 | 0.286 | 0.625 | 0.964 | 0.506 | 0 | 0.000 | 2/8.0/8/8 | 7.11 | 0 | 17.2 |
| lexical | single-session-preference | 30 | 30 | 0.467 | 0.700 | 0.867 | 0.595 | 0 | 0.000 | 8/8.0/8/8 | 8.00 | 0 | 16.8 |
| lexical | single-session-user | 64 | 64 | 0.625 | 0.891 | 0.969 | 0.757 | 0 | 0.000 | 8/8.0/8/8 | 8.00 | 0 | 2.1 |
| lexical | temporal-reasoning | 127 | 127 | 0.472 | 0.622 | 0.850 | 0.584 | 0 | 0.000 | 8/8/8/8 | 8.00 | 0 | 25.4 |
| vector | all | 500 | 479 | 0.411 | 0.727 | 0.952 | 0.589 | 0 | 0.000 | 2/8.0/8/8 | 7.90 | 0 | - |
| vector | abstention | 30 | 9 | 0.444 | 0.778 | 1.000 | 0.608 | 0 | 0.000 | 8/8.0/8/8 | 8.00 | 0 | - |
| vector | knowledge-update | 72 | 72 | 0.389 | 0.708 | 0.944 | 0.576 | 0 | 0.000 | 8/8.0/8/8 | 8.00 | 0 | - |
| vector | multi-session | 121 | 121 | 0.455 | 0.752 | 0.975 | 0.626 | 0 | 0.000 | 8/8/8/8 | 8.00 | 0 | - |
| vector | single-session-assistant | 56 | 56 | 0.268 | 0.625 | 0.964 | 0.489 | 0 | 0.000 | 2/8.0/8/8 | 7.11 | 0 | - |
| vector | single-session-preference | 30 | 30 | 0.567 | 0.900 | 1.000 | 0.740 | 0 | 0.000 | 8/8.0/8/8 | 8.00 | 0 | - |
| vector | single-session-user | 64 | 64 | 0.500 | 0.828 | 0.969 | 0.658 | 0 | 0.000 | 8/8.0/8/8 | 8.00 | 0 | - |
| vector | temporal-reasoning | 127 | 127 | 0.362 | 0.661 | 0.906 | 0.534 | 0 | 0.000 | 8/8/8/8 | 8.00 | 0 | - |
| rrf_k60 | all | 500 | 479 | 0.415 | 0.566 | 0.624 | 0.495 | 0 | 0.000 | 1/8.0/8/8 | 5.27 | 0 | 273.2 |
| rrf_k60 | abstention | 30 | 9 | 0.111 | 0.222 | 0.222 | 0.148 | 0 | 0.000 | 1/8.0/8/8 | 6.00 | 0 | 244.6 |
| rrf_k60 | knowledge-update | 72 | 72 | 0.431 | 0.611 | 0.639 | 0.517 | 0 | 0.000 | 1/2.0/8/8 | 3.39 | 0 | 273.2 |
| rrf_k60 | multi-session | 121 | 121 | 0.355 | 0.471 | 0.554 | 0.426 | 0 | 0.000 | 1/8/8/8 | 5.92 | 0 | 271.7 |
| rrf_k60 | single-session-assistant | 56 | 56 | 0.625 | 0.786 | 0.839 | 0.710 | 0 | 0.000 | 1/2.0/8/8 | 3.05 | 0 | 250.5 |
| rrf_k60 | single-session-preference | 30 | 30 | 0.267 | 0.467 | 0.667 | 0.382 | 0 | 0.000 | 1/8.0/8/8 | 7.33 | 0 | 241.9 |
| rrf_k60 | single-session-user | 64 | 64 | 0.500 | 0.562 | 0.578 | 0.531 | 0 | 0.000 | 1/3.0/8/8 | 4.22 | 0 | 269.5 |
| rrf_k60 | temporal-reasoning | 127 | 127 | 0.386 | 0.583 | 0.630 | 0.486 | 0 | 0.000 | 1/8/8/8 | 6.55 | 0 | 252.9 |
| fused_defaults | all | 500 | 479 | 0.516 | 0.741 | 0.818 | 0.631 | 0 | 0.000 | 1/6.0/8/8 | 5.04 | 0 | 273.6 |
| fused_defaults | abstention | 30 | 9 | 0.556 | 0.667 | 0.778 | 0.611 | 0 | 0.000 | 1/8.0/8/8 | 5.80 | 0 | 242.3 |
| fused_defaults | knowledge-update | 72 | 72 | 0.514 | 0.806 | 0.833 | 0.642 | 0 | 0.000 | 1/3.0/8/8 | 3.64 | 0 | 248.8 |
| fused_defaults | multi-session | 121 | 121 | 0.504 | 0.719 | 0.777 | 0.608 | 0 | 0.000 | 1/8/8/8 | 5.45 | 0 | 273.6 |
| fused_defaults | single-session-assistant | 56 | 56 | 0.625 | 0.821 | 0.893 | 0.732 | 0 | 0.000 | 1/2.5/8/8 | 3.27 | 0 | 250.7 |
| fused_defaults | single-session-preference | 30 | 30 | 0.300 | 0.533 | 0.767 | 0.448 | 0 | 0.000 | 1/8.0/8/8 | 7.33 | 0 | 238.9 |
| fused_defaults | single-session-user | 64 | 64 | 0.625 | 0.828 | 0.875 | 0.729 | 0 | 0.000 | 1/2.5/8/8 | 3.88 | 0 | 272.1 |
| fused_defaults | temporal-reasoning | 127 | 127 | 0.472 | 0.701 | 0.803 | 0.599 | 0 | 0.000 | 1/8/8/8 | 6.09 | 0 | 243.8 |
| semantic_floor_0 | all | 500 | 479 | 0.518 | 0.741 | 0.818 | 0.633 | 0 | 0.000 | 1/6.0/8/8 | 5.04 | 0 | 273.2 |
| semantic_floor_0 | abstention | 30 | 9 | 0.556 | 0.667 | 0.778 | 0.611 | 0 | 0.000 | 1/8.0/8/8 | 5.80 | 0 | 238.9 |
| semantic_floor_0 | knowledge-update | 72 | 72 | 0.528 | 0.806 | 0.833 | 0.649 | 0 | 0.000 | 1/3.0/8/8 | 3.62 | 0 | 247.5 |
| semantic_floor_0 | multi-session | 121 | 121 | 0.504 | 0.719 | 0.777 | 0.609 | 0 | 0.000 | 1/8/8/8 | 5.45 | 0 | 273.2 |
| semantic_floor_0 | single-session-assistant | 56 | 56 | 0.625 | 0.821 | 0.893 | 0.732 | 0 | 0.000 | 1/2.5/8/8 | 3.27 | 0 | 252.0 |
| semantic_floor_0 | single-session-preference | 30 | 30 | 0.300 | 0.533 | 0.767 | 0.448 | 0 | 0.000 | 1/8.0/8/8 | 7.33 | 0 | 240.6 |
| semantic_floor_0 | single-session-user | 64 | 64 | 0.625 | 0.828 | 0.875 | 0.729 | 0 | 0.000 | 1/2.5/8/8 | 3.88 | 0 | 270.0 |
| semantic_floor_0 | temporal-reasoning | 127 | 127 | 0.472 | 0.701 | 0.803 | 0.599 | 0 | 0.000 | 1/8/8/8 | 6.09 | 0 | 244.1 |
| tinybert_floor_0.2 | all | 500 | 479 | 0.516 | 0.743 | 0.816 | 0.632 | 0 | 0.000 | 1/5.0/8/8 | 4.88 | 0 | 269.3 |
| tinybert_floor_0.2 | abstention | 30 | 9 | 0.556 | 0.667 | 0.778 | 0.611 | 0 | 0.000 | 1/8.0/8/8 | 5.60 | 0 | 240.3 |
| tinybert_floor_0.2 | knowledge-update | 72 | 72 | 0.514 | 0.806 | 0.833 | 0.642 | 0 | 0.000 | 1/3.0/7/8 | 3.64 | 0 | 252.0 |
| tinybert_floor_0.2 | multi-session | 121 | 121 | 0.504 | 0.719 | 0.777 | 0.609 | 0 | 0.000 | 1/8/8/8 | 5.25 | 0 | 266.4 |
| tinybert_floor_0.2 | single-session-assistant | 56 | 56 | 0.625 | 0.821 | 0.893 | 0.732 | 0 | 0.000 | 1/2.5/7/8 | 3.21 | 0 | 252.7 |
| tinybert_floor_0.2 | single-session-preference | 30 | 30 | 0.300 | 0.500 | 0.733 | 0.437 | 0 | 0.000 | 1/8.0/8/8 | 7.10 | 0 | 240.1 |
| tinybert_floor_0.2 | single-session-user | 64 | 64 | 0.625 | 0.844 | 0.891 | 0.736 | 0 | 0.000 | 1/2.5/8/8 | 3.89 | 0 | 269.3 |
| tinybert_floor_0.2 | temporal-reasoning | 127 | 127 | 0.472 | 0.709 | 0.795 | 0.600 | 0 | 0.000 | 1/8/8/8 | 5.78 | 0 | 247.8 |
| rerank_off | all | 500 | 479 | 0.443 | 0.672 | 0.875 | 0.583 | 0 | 0.000 | 2/8.0/8/8 | 7.90 | 0 | 211.3 |
| rerank_off | abstention | 30 | 9 | 0.333 | 0.444 | 0.778 | 0.433 | 0 | 0.000 | 8/8.0/8/8 | 8.00 | 0 | 211.2 |
| rerank_off | knowledge-update | 72 | 72 | 0.583 | 0.764 | 0.931 | 0.701 | 0 | 0.000 | 8/8.0/8/8 | 8.00 | 0 | 211.2 |
| rerank_off | multi-session | 121 | 121 | 0.347 | 0.595 | 0.835 | 0.497 | 0 | 0.000 | 8/8/8/8 | 8.00 | 0 | 211.2 |
| rerank_off | single-session-assistant | 56 | 56 | 0.304 | 0.625 | 0.946 | 0.518 | 0 | 0.000 | 2/8.0/8/8 | 7.11 | 0 | 211.2 |
| rerank_off | single-session-preference | 30 | 30 | 0.500 | 0.700 | 0.800 | 0.587 | 0 | 0.000 | 8/8.0/8/8 | 8.00 | 0 | 211.2 |
| rerank_off | single-session-user | 64 | 64 | 0.531 | 0.906 | 0.953 | 0.704 | 0 | 0.000 | 8/8.0/8/8 | 8.00 | 0 | 211.0 |
| rerank_off | temporal-reasoning | 127 | 127 | 0.465 | 0.606 | 0.835 | 0.575 | 0 | 0.000 | 8/8/8/8 | 8.00 | 0 | 211.3 |
| tinybert_floor_0.1 | all | 500 | 479 | 0.516 | 0.741 | 0.808 | 0.631 | 0 | 0.000 | 1/4.0/8/8 | 4.77 | 0 | 272.1 |
| tinybert_floor_0.1 | abstention | 30 | 9 | 0.556 | 0.667 | 0.778 | 0.611 | 0 | 0.000 | 1/8.0/8/8 | 5.67 | 0 | 238.9 |
| tinybert_floor_0.1 | knowledge-update | 72 | 72 | 0.514 | 0.819 | 0.847 | 0.649 | 0 | 0.000 | 1/3.0/7/8 | 3.81 | 0 | 248.1 |
| tinybert_floor_0.1 | multi-session | 121 | 121 | 0.504 | 0.719 | 0.777 | 0.609 | 0 | 0.000 | 1/6/8/8 | 5.05 | 0 | 269.2 |
| tinybert_floor_0.1 | single-session-assistant | 56 | 56 | 0.625 | 0.821 | 0.839 | 0.725 | 0 | 0.000 | 1/2.5/6/8 | 2.82 | 0 | 254.5 |
| tinybert_floor_0.1 | single-session-preference | 30 | 30 | 0.300 | 0.500 | 0.733 | 0.437 | 0 | 0.000 | 1/8.0/8/8 | 7.17 | 0 | 243.3 |
| tinybert_floor_0.1 | single-session-user | 64 | 64 | 0.625 | 0.828 | 0.875 | 0.731 | 0 | 0.000 | 1/3.0/8/8 | 3.80 | 0 | 272.1 |
| tinybert_floor_0.1 | temporal-reasoning | 127 | 127 | 0.472 | 0.701 | 0.787 | 0.596 | 0 | 0.000 | 1/8/8/8 | 5.61 | 0 | 242.2 |
| tinybert_floor_0_fill_to_cap | all | 500 | 479 | 0.516 | 0.770 | 0.875 | 0.650 | 0 | 0.000 | 2/8.0/8/8 | 7.90 | 0 | 274.9 |
| tinybert_floor_0_fill_to_cap | abstention | 30 | 9 | 0.556 | 0.667 | 0.778 | 0.611 | 0 | 0.000 | 8/8.0/8/8 | 8.00 | 0 | 238.9 |
| tinybert_floor_0_fill_to_cap | knowledge-update | 72 | 72 | 0.514 | 0.833 | 0.931 | 0.672 | 0 | 0.000 | 8/8.0/8/8 | 8.00 | 0 | 242.9 |
| tinybert_floor_0_fill_to_cap | multi-session | 121 | 121 | 0.504 | 0.760 | 0.835 | 0.630 | 0 | 0.000 | 8/8/8/8 | 8.00 | 0 | 272.5 |
| tinybert_floor_0_fill_to_cap | single-session-assistant | 56 | 56 | 0.625 | 0.857 | 0.946 | 0.750 | 0 | 0.000 | 2/8.0/8/8 | 7.11 | 0 | 251.1 |
| tinybert_floor_0_fill_to_cap | single-session-preference | 30 | 30 | 0.300 | 0.533 | 0.800 | 0.457 | 0 | 0.000 | 8/8.0/8/8 | 8.00 | 0 | 241.8 |
| tinybert_floor_0_fill_to_cap | single-session-user | 64 | 64 | 0.625 | 0.875 | 0.953 | 0.755 | 0 | 0.000 | 8/8.0/8/8 | 8.00 | 0 | 274.9 |
| tinybert_floor_0_fill_to_cap | temporal-reasoning | 127 | 127 | 0.472 | 0.717 | 0.835 | 0.609 | 0 | 0.000 | 8/8/8/8 | 8.00 | 0 | 245.3 |

## locomo-conv26

| arm | type | n | scored | R@1 | R@3 | R@8 | MRR | empty | empty rate | caps min/med/p90/max | caps mean | errors | peak RSS MiB |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---|---:|---:|---:|
| lexical | all | 199 | 197 | 0.203 | 0.315 | 0.569 | 0.295 | 0 | 0.000 | 8/8/8/8 | 8.00 | 0 | 29.0 |
| lexical | 1 | 32 | 32 | 0.094 | 0.188 | 0.469 | 0.187 | 0 | 0.000 | 8/8.0/8/8 | 8.00 | 0 | 29.0 |
| lexical | 2 | 37 | 37 | 0.054 | 0.243 | 0.784 | 0.211 | 0 | 0.000 | 8/8/8/8 | 8.00 | 0 | 29.0 |
| lexical | 3 | 13 | 11 | 0.000 | 0.091 | 0.364 | 0.096 | 0 | 0.000 | 8/8/8/8 | 8.00 | 0 | 29.0 |
| lexical | 4 | 70 | 70 | 0.286 | 0.386 | 0.529 | 0.357 | 0 | 0.000 | 8/8.0/8/8 | 8.00 | 0 | 29.0 |
| lexical | 5 | 47 | 47 | 0.319 | 0.404 | 0.574 | 0.389 | 0 | 0.000 | 8/8/8/8 | 8.00 | 0 | 28.6 |
| vector | all | 199 | 197 | 0.320 | 0.523 | 0.670 | 0.436 | 0 | 0.000 | 8/8/8/8 | 8.00 | 0 | - |
| vector | 1 | 32 | 32 | 0.125 | 0.344 | 0.594 | 0.273 | 0 | 0.000 | 8/8.0/8/8 | 8.00 | 0 | - |
| vector | 2 | 37 | 37 | 0.622 | 0.865 | 0.892 | 0.725 | 0 | 0.000 | 8/8/8/8 | 8.00 | 0 | - |
| vector | 3 | 13 | 11 | 0.182 | 0.545 | 0.727 | 0.371 | 0 | 0.000 | 8/8/8/8 | 8.00 | 0 | - |
| vector | 4 | 70 | 70 | 0.343 | 0.471 | 0.629 | 0.429 | 0 | 0.000 | 8/8.0/8/8 | 8.00 | 0 | - |
| vector | 5 | 47 | 47 | 0.213 | 0.447 | 0.596 | 0.342 | 0 | 0.000 | 8/8/8/8 | 8.00 | 0 | - |
| rrf_k60 | all | 199 | 197 | 0.340 | 0.447 | 0.487 | 0.400 | 0 | 0.000 | 1/2/8/8 | 3.47 | 0 | 239.5 |
| rrf_k60 | 1 | 32 | 32 | 0.188 | 0.281 | 0.312 | 0.237 | 0 | 0.000 | 1/3.0/8/8 | 3.78 | 0 | 239.3 |
| rrf_k60 | 2 | 37 | 37 | 0.405 | 0.568 | 0.595 | 0.491 | 0 | 0.000 | 1/2/8/8 | 2.92 | 0 | 239.4 |
| rrf_k60 | 3 | 13 | 11 | 0.273 | 0.273 | 0.273 | 0.273 | 0 | 0.000 | 1/8/8/8 | 5.69 | 0 | 239.3 |
| rrf_k60 | 4 | 70 | 70 | 0.386 | 0.500 | 0.500 | 0.443 | 0 | 0.000 | 1/2.0/8/8 | 2.86 | 0 | 239.5 |
| rrf_k60 | 5 | 47 | 47 | 0.340 | 0.426 | 0.553 | 0.407 | 0 | 0.000 | 1/2/8/8 | 3.98 | 0 | 238.9 |
| fused_defaults | all | 199 | 197 | 0.345 | 0.431 | 0.447 | 0.388 | 0 | 0.000 | 1/2/8/8 | 3.19 | 0 | 239.5 |
| fused_defaults | 1 | 32 | 32 | 0.125 | 0.250 | 0.250 | 0.177 | 0 | 0.000 | 1/3.0/8/8 | 3.91 | 0 | 239.5 |
| fused_defaults | 2 | 37 | 37 | 0.486 | 0.568 | 0.568 | 0.527 | 0 | 0.000 | 1/1/8/8 | 2.19 | 0 | 239.4 |
| fused_defaults | 3 | 13 | 11 | 0.182 | 0.182 | 0.182 | 0.182 | 0 | 0.000 | 1/8/8/8 | 5.54 | 0 | 239.5 |
| fused_defaults | 4 | 70 | 70 | 0.400 | 0.457 | 0.471 | 0.429 | 0 | 0.000 | 1/1.0/8/8 | 2.84 | 0 | 239.5 |
| fused_defaults | 5 | 47 | 47 | 0.340 | 0.468 | 0.511 | 0.409 | 0 | 0.000 | 1/2/8/8 | 3.36 | 0 | 238.9 |
| semantic_floor_0 | all | 199 | 197 | 0.345 | 0.431 | 0.447 | 0.388 | 0 | 0.000 | 1/2/8/8 | 3.19 | 0 | 239.5 |
| semantic_floor_0 | 1 | 32 | 32 | 0.125 | 0.250 | 0.250 | 0.177 | 0 | 0.000 | 1/3.0/8/8 | 3.91 | 0 | 239.4 |
| semantic_floor_0 | 2 | 37 | 37 | 0.486 | 0.568 | 0.568 | 0.527 | 0 | 0.000 | 1/1/8/8 | 2.19 | 0 | 239.4 |
| semantic_floor_0 | 3 | 13 | 11 | 0.182 | 0.182 | 0.182 | 0.182 | 0 | 0.000 | 1/8/8/8 | 5.54 | 0 | 239.3 |
| semantic_floor_0 | 4 | 70 | 70 | 0.400 | 0.457 | 0.471 | 0.429 | 0 | 0.000 | 1/1.0/8/8 | 2.84 | 0 | 239.5 |
| semantic_floor_0 | 5 | 47 | 47 | 0.340 | 0.468 | 0.511 | 0.409 | 0 | 0.000 | 1/2/8/8 | 3.36 | 0 | 238.9 |
| tinybert_floor_0.2 | all | 199 | 197 | 0.345 | 0.426 | 0.442 | 0.385 | 0 | 0.000 | 1/2/8/8 | 3.08 | 0 | 239.5 |
| tinybert_floor_0.2 | 1 | 32 | 32 | 0.125 | 0.219 | 0.219 | 0.161 | 0 | 0.000 | 1/2.5/8/8 | 3.59 | 0 | 239.4 |
| tinybert_floor_0.2 | 2 | 37 | 37 | 0.486 | 0.568 | 0.568 | 0.527 | 0 | 0.000 | 1/1/8/8 | 2.19 | 0 | 239.5 |
| tinybert_floor_0.2 | 3 | 13 | 11 | 0.182 | 0.182 | 0.182 | 0.182 | 0 | 0.000 | 1/8/8/8 | 5.00 | 0 | 239.3 |
| tinybert_floor_0.2 | 4 | 70 | 70 | 0.400 | 0.457 | 0.471 | 0.429 | 0 | 0.000 | 1/1.0/8/8 | 2.76 | 0 | 239.4 |
| tinybert_floor_0.2 | 5 | 47 | 47 | 0.340 | 0.468 | 0.511 | 0.409 | 0 | 0.000 | 1/2/8/8 | 3.36 | 0 | 238.9 |
| rerank_off | all | 199 | 197 | 0.203 | 0.279 | 0.482 | 0.272 | 0 | 0.000 | 8/8/8/8 | 8.00 | 0 | 215.4 |
| rerank_off | 1 | 32 | 32 | 0.062 | 0.156 | 0.281 | 0.131 | 0 | 0.000 | 8/8.0/8/8 | 8.00 | 0 | 215.3 |
| rerank_off | 2 | 37 | 37 | 0.027 | 0.135 | 0.568 | 0.139 | 0 | 0.000 | 8/8/8/8 | 8.00 | 0 | 215.4 |
| rerank_off | 3 | 13 | 11 | 0.091 | 0.091 | 0.182 | 0.114 | 0 | 0.000 | 8/8/8/8 | 8.00 | 0 | 212.8 |
| rerank_off | 4 | 70 | 70 | 0.300 | 0.357 | 0.514 | 0.354 | 0 | 0.000 | 8/8.0/8/8 | 8.00 | 0 | 215.4 |
| rerank_off | 5 | 47 | 47 | 0.319 | 0.404 | 0.574 | 0.387 | 0 | 0.000 | 8/8/8/8 | 8.00 | 0 | 214.5 |
| tinybert_floor_0.1 | all | 199 | 197 | 0.345 | 0.431 | 0.447 | 0.388 | 0 | 0.000 | 1/2/8/8 | 2.94 | 0 | 239.5 |
| tinybert_floor_0.1 | 1 | 32 | 32 | 0.125 | 0.250 | 0.250 | 0.177 | 0 | 0.000 | 1/2.0/8/8 | 3.66 | 0 | 239.3 |
| tinybert_floor_0.1 | 2 | 37 | 37 | 0.486 | 0.568 | 0.568 | 0.527 | 0 | 0.000 | 1/1/8/8 | 2.27 | 0 | 239.5 |
| tinybert_floor_0.1 | 3 | 13 | 11 | 0.182 | 0.182 | 0.182 | 0.182 | 0 | 0.000 | 1/2/8/8 | 3.92 | 0 | 239.4 |
| tinybert_floor_0.1 | 4 | 70 | 70 | 0.400 | 0.457 | 0.471 | 0.429 | 0 | 0.000 | 1/1.0/8/8 | 2.67 | 0 | 239.5 |
| tinybert_floor_0.1 | 5 | 47 | 47 | 0.340 | 0.468 | 0.511 | 0.409 | 0 | 0.000 | 1/2/8/8 | 3.13 | 0 | 238.9 |
| tinybert_floor_0_fill_to_cap | all | 199 | 197 | 0.340 | 0.457 | 0.482 | 0.397 | 0 | 0.000 | 8/8/8/8 | 8.00 | 0 | 239.5 |
| tinybert_floor_0_fill_to_cap | 1 | 32 | 32 | 0.125 | 0.281 | 0.281 | 0.188 | 0 | 0.000 | 8/8.0/8/8 | 8.00 | 0 | 239.4 |
| tinybert_floor_0_fill_to_cap | 2 | 37 | 37 | 0.486 | 0.568 | 0.568 | 0.527 | 0 | 0.000 | 8/8/8/8 | 8.00 | 0 | 239.5 |
| tinybert_floor_0_fill_to_cap | 3 | 13 | 11 | 0.182 | 0.182 | 0.182 | 0.182 | 0 | 0.000 | 8/8/8/8 | 8.00 | 0 | 239.4 |
| tinybert_floor_0_fill_to_cap | 4 | 70 | 70 | 0.386 | 0.471 | 0.514 | 0.431 | 0 | 0.000 | 8/8.0/8/8 | 8.00 | 0 | 239.5 |
| tinybert_floor_0_fill_to_cap | 5 | 47 | 47 | 0.340 | 0.532 | 0.574 | 0.438 | 0 | 0.000 | 8/8/8/8 | 8.00 | 0 | 238.9 |

## run meta

```
{
 "lme-oracle.meta.json": {
  "dataset": "lme-oracle",
  "questions": 500,
  "rows": 4500,
  "rows_scored": 4311,
  "metrics_note": "recall/MRR are valid only over rows with scored=true; empty rate and capsules over all rows",
  "exit": 0,
  "wall_s": 6316.1,
  "peak_child_rss_mib_rusage": 274.4,
  "peak_self_rss_mib": 60.1,
  "profile": "bench-sweep",
  "seed": null,
  "seed_note": "no component takes a seed; PYTHONHASHSEED pinned by runner"
 },
 "locomo-conv26.meta.json": {
  "dataset": "locomo-conv26",
  "questions": 199,
  "rows": 1791,
  "rows_scored": 1773,
  "metrics_note": "recall/MRR are valid only over rows with scored=true; empty rate and capsules over all rows",
  "exit": 0,
  "wall_s": 4251.2,
  "peak_child_rss_mib_rusage": 239.5,
  "peak_self_rss_mib": 81.4,
  "profile": "bench-sweep",
  "seed": null,
  "seed_note": "no component takes a seed; PYTHONHASHSEED pinned by runner"
 }
}
```

# capsule histogram: run1 (all rows, scored or not)

## lme-oracle

| arm | n | median | empty | caps 0 1 2 3 4 5 6 7 8 |
|---|---|---|---|---|
| lexical | 500 | 8.0 | 0 | 0 0 2 1 4 2 2 7 482 |
| vector | 500 | 8.0 | 0 | 0 0 2 1 4 2 2 7 482 |
| fused_defaults | 500 | 6.0 | 0 | 0 107 58 42 24 18 9 6 236 |
| rerank_off | 500 | 8.0 | 0 | 0 0 2 1 4 2 2 7 482 |
| tinybert_floor_0_fill_to_cap | 500 | 8.0 | 0 | 0 0 2 1 4 2 2 7 482 |
| tinybert_floor_0.1 | 500 | 4.0 | 0 | 0 111 58 57 29 19 14 6 206 |
| tinybert_floor_0.2 | 500 | 5.0 | 0 | 0 108 66 47 21 21 9 7 221 |
| semantic_floor_0 | 500 | 6.0 | 0 | 0 107 59 41 24 18 9 6 236 |
| rrf_k60 | 500 | 8.0 | 0 | 0 118 53 26 13 12 2 1 275 |

## locomo-conv26

| arm | n | median | empty | caps 0 1 2 3 4 5 6 7 8 |
|---|---|---|---|---|
| lexical | 199 | 8 | 0 | 0 0 0 0 0 0 0 0 199 |
| vector | 199 | 8 | 0 | 0 0 0 0 0 0 0 0 199 |
| fused_defaults | 199 | 2 | 0 | 0 93 36 11 7 1 2 0 49 |
| rerank_off | 199 | 8 | 0 | 0 0 0 0 0 0 0 0 199 |
| tinybert_floor_0_fill_to_cap | 199 | 8 | 0 | 0 0 0 0 0 0 0 0 199 |
| tinybert_floor_0.1 | 199 | 2 | 0 | 0 95 39 13 8 2 2 0 40 |
| tinybert_floor_0.2 | 199 | 2 | 0 | 0 93 39 12 7 1 2 0 45 |
| semantic_floor_0 | 199 | 2 | 0 | 0 93 36 11 7 1 2 0 49 |
| rrf_k60 | 199 | 2 | 0 | 0 66 49 15 14 1 5 2 47 |

## corrections (added 2026-11-20 by fuse, PR #79 review)

- the MemAvailable trace is /workspace/fuse/e0s/logs/memavailable.rerun2.log and the memcap log is /workspace/fuse/e0s/memcap.rerun2.log (the lines above name the run1 files).
- the original runner never wrote a manifest (run2 died first), so this file did not overwrite a run1 manifest.
- seed line above edited in PR #80 to note that determinism-diff.md found 1099/6291 changed rows.
