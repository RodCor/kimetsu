# Retrieval comparison artifacts

Production source: `41ff583`; benchmark source: `3c275f6` (includes effective-cutoff readback and dated/compressed memory matching).

`development-scores.json` and `development-sweep.json` are diagnostic development evidence. Run `python sweep.py` to reproduce the score sweep; this does not rerun rendering or measure runtime. `retrieval_probe.rs` is the throwaway diagnostic source, retained for audit and intentionally excluded from normal Cargo builds.

`validation-frozen.json` is the untouched assistant-authored synthetic validation set. Its SHA256 is `1a09270e09a9521c38f9dca14dfebe4349fed0e10b1537a92857eb18d3f6bb6f`. The final candidate threshold is 0.55, chosen before inference on validation. An earlier 0.90 development campaign was interrupted after identifying the text-matching bug; it was never used to tune against validation.

Run `run-comparisons.ps1` with explicit `-Baseline`, `-Candidate`, `-Harness`, `-ModelCache`, `-HfHome`, and a new `-OutputRoot`. The baseline is the preceding hardened binary; candidate uses the optional multilingual alias and cutoff 0.55. ModelCache holds BGE-small; HfHome holds the TinyBERT and pinned mMARCO caches. Keep builds and other inference out of the timing window. The script runs all three fixtures with three alternating repetitions, checks frozen fixture identity, and rejects scenario errors or unpaired results.

The model weights are not committed. `model-manifest.json` records their source revision, sizes and hashes. Both model options run locally without generation API calls. Injected memories still consume agent context.

Final production comparisons and all per-query reports are in `contract/`, `development/`, and `validation/`. Run `python summarize.py` to reproduce `summary.json`. `strict-contract-earlier-harness.json` preserves the earlier 0.90 contract-only experiment; it is not part of the corrected 0.55 campaign. Delivered capsule text is synthetic/development fixture content and is preserved to audit matching, compression and ordering. Latency measurements stop before artifact serialization. Repeated quality measurements are averaged per query, not treated as independent samples.
