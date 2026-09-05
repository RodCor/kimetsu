# Local inference thread control

`KIMETSU_INTRA_THREADS=4` opts the process into a shared ONNX Runtime pool with four intra-operation threads, one inter-operation thread and idle spinning disabled. Set it before the first embedding or reranking model loads. Valid values are integers from 1 to 1024. Unset preserves the existing backend default; changing the environment after first load requires a new process. This controls local embedding/reranking inference, not host model generation.

The configured pool is shared by both models, including user-defined ONNX rerankers. Startup reports the applied setting to stderr. If another embedding application already configured ONNX Runtime, initialization reports that this requested setting cannot take effect rather than claiming success. Existing model-loader fallback behavior still applies, so inspect stderr when diagnosing a disabled semantic path.

FastEmbed 5.13.4 sets per-session threads to available logical CPUs. The pinned ort 2.0.0-rc.12 session builder disables per-session pools when the environment provides a global pool, making this control effective without vendoring FastEmbed. The direct ort dependency intentionally matches FastEmbed's runtime instance.

Choose thread counts empirically on the deployment machine. Compare 1, 4, 8 and the unset default with identical cached models, corpus, query order and repetitions. Record quality, p50/p95 latency, total wall time and memory. Shared CPU workloads and idle power matter alongside isolated throughput. No count is declared universally optimal.

Sources: [ONNX Runtime thread management](https://onnxruntime.ai/docs/performance/tune-performance/threading.html), [ort environment API](https://docs.rs/ort/2.0.0-rc.12/ort/environment/struct.EnvironmentBuilder.html). The dependency's local source was also checked for automatic DisablePerSessionThreads in session builder pre_commit.
