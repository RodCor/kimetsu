# BrainBenchmark measurement artifacts — September 5, 2026

Read the [results and limitations](../2026-09-04-hardening-results.md) before interpreting these comparisons.

| Artifact | Comparison |
|---|---|
| [contract-512.json](contract-512.json) | Original versus hardened binary; contract fixture; requested budget 512 |
| [contract-2048.json](contract-2048.json) | Same, requested budget 2,048 |
| [development-6000.json](development-6000.json) | Original versus hardened binary; 197 positive and 13 negative development queries; requested budget 6,000 |
| [threads-default-vs-4.json](threads-default-vs-4.json) | Hardened binary + TinyBERT on both sides; default versus four-thread runtime |
| [tinybert-vs-minilm-4threads.json](tinybert-vs-minilm-4threads.json) | Hardened binary + four-thread runtime on both sides; TinyBERT L2 versus MiniLM L4 |
| [idf-sql-comparison.json](idf-sql-comparison.json) | Isolated synthetic SQLite helper comparison, five repeats at each corpus size |

Each paired JSON records binary, harness, Python runner and fixture SHA-256 fingerprints, configuration, alternating run order, scenario pairing, summaries and raw-report filenames. All five pairs completed without errors or unpaired scenarios. There are 4,044 query observations, including repeated measurements; the fixtures are development evidence, not held-out evaluation. Raw reports/logs remain in `tmp-tests/paired-*` in the implementation worktree; those files are not embedded in these summary artifacts.

Source revisions:

- Original Kimetsu: `3ec56a8`; frozen executable SHA-256 `aba19d66742fe4c6b7d8902a52f0b6d5a0ad5c9d72ecf705a2e5d69d546a75c0`.
- Hardened Kimetsu: `b9658995609e945d18ee87163d543494c1c44713`; executable SHA-256 `5c87e542907a47917f23fad50ddff1e46789eaae14328ccc662ca748b66b5477`.
- Separate benchmark repository: `b3e0eda1641d340781eda843a477425f629aa119`.

The [contract fixture](agent-memory-contract.json) has 22 queries in six scenarios. The [development fixture](development-100.json) is a converted copy of the existing 100-memory dataset; the original dataset's SHA-256 was `7a9da27cc7c3b88c13b0fb97eee5fdb2c02c5c308c6719a992cc6dbbf98bf58f`. Conversion preserves its 197 positive and 13 negative queries. Copies here have the same bytes as the campaign inputs.

To reproduce, build the CLI at each source revision with `cargo build -p kimetsu-cli --release --features embeddings --locked --offline -j 1`, preserving each executable separately. Build the benchmark at its revision with `cargo build --release --bin kbench --locked --offline -j 1`. Rebuilding on a different toolchain/host can change binary hashes and timings. The separate benchmark repository must be available at the implementation root's `bench` directory.

Run [run-comparisons.ps1](run-comparisons.ps1) from a fresh PowerShell process, supplying the preserved executables and a cache containing BGE-small, TinyBERT L2 and MiniLM L4:

```powershell
pwsh -NoProfile -File ./docs/audits/2026-09-05-brainbench/run-comparisons.ps1 `
  -Baseline ./tmp-tests/kimetsu-baseline.exe `
  -Candidate ./tmp-tests/kimetsu-candidate.exe `
  -Harness E:/Kimetsu/bench/target/release/kbench.exe `
  -ModelCache E:/Kimetsu/.fastembed_cache `
  -OutputRoot ./tmp-tests/brainbench-rerun
```

Choose a new output directory. The runner uses only temporary benchmark brains, sets offline/local model controls, runs the five pairs sequentially with three repeats each, rejects errors/unpaired scenarios, then runs [the SQL helper](benchmark_idf_sql.py). It intentionally leaves host/global configuration unchanged. Do not run compilation, tests or other local inference concurrently. The measured campaign's original launcher remains at `tmp-tests/run-hardening-comparisons.ps1`; this parameterized wrapper reproduces its settings without hard-coding binary locations.

The SQL experiment uses Python's SQLite library and an in-memory synthetic database. It compares aligned rare-token document counts, includes the new population count, and excludes ANN, models, serialization and production-storage effects. Its results must not be described as an end-to-end retrieval speedup.
