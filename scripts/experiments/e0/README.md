# Experiment E0

`prepare_data.py`, `e0_eval.py`, and `summarize_e0.py` in this directory are the files from the run prepared against main `19b178741c95083d77b520d64fbd0496fdf1aa0a`. sha256:

```
4e1736abfdc822687e3fe094168e8b994ee67d497dc3ea210bab9bdf9b27b24f  prepare_data.py
9c68b8dcbb8ad09f815ccd746d5d4230b36f89cb3108965465c3ff363d25382d  e0_eval.py
8d19cc49247ab28e6fc1e527a3ad706161c49c89d8fa2d9e95de8c0aef9cdb09  summarize_e0.py
```

`prepare_data.py` and `e0_eval.py` read a fixed worktree path. The entry points that take paths are:

| File | Role |
| --- | --- |
| `scripts/prepare_e0_data.py` | LoCoMo conv-26 and LongMemEval oracle adapter |
| `scripts/e0_eval.py` | Runs `scripts/retrieval_eval.py` under `--profile bench` |
| `scripts/retrieval_eval_profiles.py` | Bench and script profile definitions |
| `scripts/experiments/e0/summarize_e0.py` | Summary tables and the run1/run2 diff (this copy; it already takes paths) |
| `scripts/experiments/e0/run_e0.sh` | Wait for free RAM, build release with embeddings, run both passes, diff |

`run_e0.sh` is the portable runner. It has no process ids, no machine paths, and no C++ or OpenSSL toolchain. How to point it at data is in the repository README, section "Experiment E0".
