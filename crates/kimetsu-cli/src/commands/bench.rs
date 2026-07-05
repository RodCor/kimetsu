//! kbench-style eval + brainbench commands.
//! Split out of main.rs (v2.5.1); implementations only — the clap
//! surface stays in main.rs.

#![allow(unused_imports)]
use std::env;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use kimetsu_brain::project;
use kimetsu_core::KimetsuResult;
use kimetsu_core::memory::{MemoryKind, MemoryScope};

use crate::*;

pub(crate) fn brain_eval(args: EvalArgs) -> KimetsuResult<()> {
    #[cfg(feature = "embeddings")]
    {
        brain_eval_inner(args)
    }
    #[cfg(not(feature = "embeddings"))]
    {
        let _ = args;
        println!("kimetsu brain eval requires an embeddings build.");
        println!("Rebuild with: cargo build -p kimetsu-cli --features embeddings");
        Ok(())
    }
}

#[cfg(feature = "embeddings")]
pub(crate) fn brain_eval_inner(args: EvalArgs) -> KimetsuResult<()> {
    use kimetsu_brain::context::{ContextRequest, rerank_capsules};
    use kimetsu_brain::embeddings::{
        NoopEmbedder, open_embedder_for_model, open_reranker_for_model,
    };
    use kimetsu_brain::eval::{EvalFixture, mean, mrr, recall_at_k};
    use kimetsu_brain::project::{BrainSession, add_memory, init_project};
    use kimetsu_core::memory::{MemoryKind, MemoryScope};
    use kimetsu_core::paths::git_init_boundary;
    use std::collections::HashMap;
    use std::time::Instant;

    // Disable the user brain for this process — we work in a hermetic temp dir.
    // SAFETY: this is a one-shot CLI command; no other threads have started yet.
    unsafe {
        std::env::set_var("KIMETSU_USER_BRAIN", "0");
    }

    // ── 1. Load and validate fixture ─────────────────────────────────────────
    let fixture_path = &args.fixture;
    let fixture_text = std::fs::read_to_string(fixture_path)
        .map_err(|e| format!("cannot read fixture {}: {e}", fixture_path.display()))?;
    let fixture: EvalFixture = serde_json::from_str(&fixture_text)
        .map_err(|e| format!("invalid fixture JSON in {}: {e}", fixture_path.display()))?;

    // Validate: every relevant key must exist in memories.
    let all_keys: std::collections::HashSet<&str> =
        fixture.memories.iter().map(|m| m.key.as_str()).collect();
    for case in &fixture.cases {
        for rel in &case.relevant {
            if !all_keys.contains(rel.as_str()) {
                return Err(format!(
                    "fixture validation error: relevant key {:?} in query {:?} does not exist in memories",
                    rel, case.query
                )
                .into());
            }
        }
    }

    println!(
        "eval fixture: {} memories, {} cases",
        fixture.memories.len(),
        fixture.cases.len()
    );

    // ── 2. Set up a hermetic temp brain ──────────────────────────────────────
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let tmp_root = std::env::temp_dir().join(format!("kimetsu-eval-{ts}"));
    std::fs::create_dir_all(&tmp_root)?;
    git_init_boundary(&tmp_root);

    // Init the project brain.
    init_project(&tmp_root, true).map_err(|e| format!("init_project: {e}"))?;

    // Add all corpus memories and track key → memory_id mapping.
    println!(
        "adding {} memories to temp brain...",
        fixture.memories.len()
    );
    let mut key_to_id: HashMap<String, String> = HashMap::new();
    for mem in &fixture.memories {
        let memory_id = add_memory(&tmp_root, MemoryScope::Project, MemoryKind::Fact, &mem.text)
            .map_err(|e| format!("add_memory {:?}: {e}", mem.key))?;
        key_to_id.insert(mem.key.clone(), memory_id);
    }

    // Build key → id lookup from the map (for ranking back to keys).
    let id_to_key: HashMap<String, String> = key_to_id
        .iter()
        .map(|(k, v)| (v.clone(), k.clone()))
        .collect();

    // #1a HyDE: pre-expand each case query ONCE (shared across all retrieval
    // modes) so the embedding matches a hypothetical answer rather than the
    // question. Reranking still uses the original query. The semantic query
    // used for retrieval is `original + hypothetical`.
    let retrieval_queries: Vec<String> = if args.hyde {
        let cfg = tmp_root.join(".kimetsu").join("project.toml");
        if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(&cfg) {
            use std::io::Write;
            let _ = writeln!(
                f,
                "\n[cheap_model]\nenabled = true\nprovider = \"ollama\"\nmodel = \"qwen2.5:3b\""
            );
        }
        println!(
            "HyDE: expanding {} queries via the cheap model (one model call each)...",
            fixture.cases.len()
        );
        fixture
            .cases
            .iter()
            .map(|c| hyde_augment_query(&tmp_root, &c.query))
            .collect()
    } else {
        fixture.cases.iter().map(|c| c.query.clone()).collect()
    };

    // ── 3. Helper: run one mode, return ranked key list per case ─────────────
    let run_mode = |mode_label: &str,
                    embedder: &dyn kimetsu_brain::embeddings::Embedder,
                    reranker: Option<&dyn kimetsu_brain::embeddings::Reranker>,
                    pool: usize,
                    rerank_floor: f32,
                    rerank_cap: usize|
     -> KimetsuResult<(Vec<Vec<String>>, u128)> {
        let session = BrainSession::open_readonly(&tmp_root)
            .map_err(|e| format!("{mode_label} open_readonly: {e}"))?;

        let t0 = Instant::now();
        let mut per_case_ranked: Vec<Vec<String>> = Vec::new();

        for (ci, case) in fixture.cases.iter().enumerate() {
            let fetch_cap = pool;
            let request = ContextRequest {
                stage: "localization".to_string(),
                query: retrieval_queries[ci].clone(),
                budget_tokens: 6000,
                max_capsules: fetch_cap,
                min_semantic_score: 0.0,   // disable floor for eval recall
                min_lexical_coverage: 0.0, // disable floor for eval recall
                ..Default::default()
            };
            let mut bundle = session
                .retrieve_context_with_injected_embedder(request, embedder)
                .map_err(|e| format!("{mode_label} retrieve: {e}"))?;

            // Apply reranker when present.
            if let Some(rr) = reranker {
                bundle.capsules =
                    rerank_capsules(&case.query, bundle.capsules, rr, rerank_floor, rerank_cap);
            }

            // Map capsule expansion_handle "memory:<id>" → fixture key.
            let ranked_keys: Vec<String> = bundle
                .capsules
                .iter()
                .filter_map(|c| {
                    c.expansion_handle
                        .strip_prefix("memory:")
                        .and_then(|id| id_to_key.get(id))
                        .cloned()
                })
                .collect();

            per_case_ranked.push(ranked_keys);
        }

        let elapsed = t0.elapsed().as_millis();
        Ok((per_case_ranked, elapsed))
    };

    // ── 4. Run the three modes ────────────────────────────────────────────────
    // Pool mirrors the daemon's RERANK_POOL by default; --pool overrides it
    // for pool-size experiments.
    let pool = args.pool.max(1);
    let rerank_floor = 0.30f32;
    let rerank_cap = 4usize;

    print!("running fts mode...");
    let (fts_ranked, fts_ms) = run_mode("fts", &NoopEmbedder, None, pool, 0.0, 0)?;
    println!(" done ({fts_ms} ms)");

    print!("running semantic mode (loading embedder)...");
    let semantic_embedder = open_embedder_for_model("bge-small-en-v1.5");
    let (sem_ranked, sem_ms) =
        run_mode("semantic", semantic_embedder.as_ref(), None, pool, 0.0, 0)?;
    println!(" done ({sem_ms} ms)");

    print!("running semantic+rerank mode (loading reranker)...");
    let reranker_opt = open_reranker_for_model("jina-reranker-v1-turbo-en");
    let reranker_ref: Option<&dyn kimetsu_brain::embeddings::Reranker> = reranker_opt.as_deref();
    let (rr_ranked, rr_ms) = run_mode(
        "semantic+rerank",
        semantic_embedder.as_ref(),
        reranker_ref,
        pool,
        rerank_floor,
        rerank_cap,
    )?;
    println!(" done ({rr_ms} ms)");

    // ── 5. Compute metrics ────────────────────────────────────────────────────
    let eval_cases = &fixture.cases;
    let n = eval_cases.len();

    // Separate cases with relevant items from noise cases.
    let signal_indices: Vec<usize> = (0..n)
        .filter(|&i| !eval_cases[i].relevant.is_empty())
        .collect();
    let noise_indices: Vec<usize> = (0..n)
        .filter(|&i| eval_cases[i].relevant.is_empty())
        .collect();

    let compute_metrics = |ranked: &[Vec<String>]| -> (f64, f64, f64, f64) {
        // recall@2, recall@4, MRR over signal cases
        let r2: Vec<f64> = signal_indices
            .iter()
            .map(|&i| recall_at_k(&ranked[i], &eval_cases[i].relevant, 2))
            .collect();
        let r4: Vec<f64> = signal_indices
            .iter()
            .map(|&i| recall_at_k(&ranked[i], &eval_cases[i].relevant, 4))
            .collect();
        let mrr_vals: Vec<f64> = signal_indices
            .iter()
            .map(|&i| mrr(&ranked[i], &eval_cases[i].relevant))
            .collect();
        // Average noise capsule count for irrelevant cases.
        let noise_avg = if noise_indices.is_empty() {
            0.0
        } else {
            noise_indices
                .iter()
                .map(|&i| ranked[i].len() as f64)
                .sum::<f64>()
                / noise_indices.len() as f64
        };
        (mean(&r2), mean(&r4), mean(&mrr_vals), noise_avg)
    };

    let (fts_r2, fts_r4, fts_mrr, fts_noise) = compute_metrics(&fts_ranked);
    let (sem_r2, sem_r4, sem_mrr, sem_noise) = compute_metrics(&sem_ranked);
    let (rr_r2, rr_r4, rr_mrr, rr_noise) = compute_metrics(&rr_ranked);

    // ── 6. Print table ────────────────────────────────────────────────────────
    println!();
    println!(
        "{:<22} {:>10} {:>10} {:>10} {:>22} {:>10}",
        "mode", "recall@2", "recall@4", "MRR", "noise-capsules(irrelevant)", "elapsed_ms"
    );
    println!("{}", "-".repeat(90));
    println!(
        "{:<22} {:>10.3} {:>10.3} {:>10.3} {:>22.1} {:>10}",
        "fts", fts_r2, fts_r4, fts_mrr, fts_noise, fts_ms
    );
    println!(
        "{:<22} {:>10.3} {:>10.3} {:>10.3} {:>22.1} {:>10}",
        "semantic", sem_r2, sem_r4, sem_mrr, sem_noise, sem_ms
    );
    println!(
        "{:<22} {:>10.3} {:>10.3} {:>10.3} {:>22.1} {:>10}",
        "semantic+rerank", rr_r2, rr_r4, rr_mrr, rr_noise, rr_ms
    );
    println!();
    println!(
        "signal cases: {}  |  noise (empty-relevant) cases: {}",
        signal_indices.len(),
        noise_indices.len()
    );

    // ── 7. Optional per-reranker benchmark ───────────────────────────────────
    let reranker_ids: Vec<&str> = args
        .rerankers
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();

    if !reranker_ids.is_empty() {
        // Struct to hold benchmark results for one reranker.
        struct RankerBenchRow {
            label: String,
            load_ms: u128,
            rerank_mean_ms: f64,
            rerank_max_ms: u128,
            r2: f64,
            r4: f64,
            mrr: f64,
            noise: f64,
            onnx_kb: Option<u64>,
        }

        // Helper: run the signal cases and time only the rerank step per query.
        let run_reranker_bench = |rr_id: &str| -> KimetsuResult<RankerBenchRow> {
            use kimetsu_brain::context::rerank_capsules;

            print!("  loading {rr_id}...");
            let _ = std::io::Write::flush(&mut std::io::stdout());
            let load_start = Instant::now();
            let reranker_box = open_reranker_for_model(rr_id);
            let load_ms = load_start.elapsed().as_millis();

            let reranker_ref: Option<&dyn kimetsu_brain::embeddings::Reranker> =
                reranker_box.as_deref();

            if reranker_ref.is_none() {
                println!(" SKIPPED (loader returned None)");
                return Err(format!("reranker {rr_id} failed to load").into());
            }
            println!(" loaded ({load_ms} ms)");

            let session = kimetsu_brain::project::BrainSession::open_readonly(&tmp_root)
                .map_err(|e| format!("{rr_id} open_readonly: {e}"))?;
            let rr = reranker_ref.unwrap();

            let mut per_case_ranked: Vec<Vec<String>> = Vec::new();
            let mut rerank_times_ms: Vec<u128> = Vec::new();

            for case in fixture.cases.iter() {
                let request = kimetsu_brain::context::ContextRequest {
                    stage: "localization".to_string(),
                    query: case.query.clone(),
                    budget_tokens: 6000,
                    max_capsules: pool,
                    min_semantic_score: 0.0,
                    min_lexical_coverage: 0.0,
                    ..Default::default()
                };
                let mut bundle = session
                    .retrieve_context_with_injected_embedder(request, semantic_embedder.as_ref())
                    .map_err(|e| format!("{rr_id} retrieve: {e}"))?;

                // Time only the rerank step.
                let rr_start = Instant::now();
                if !eval_cases[per_case_ranked.len()].relevant.is_empty() {
                    bundle.capsules =
                        rerank_capsules(&case.query, bundle.capsules, rr, rerank_floor, rerank_cap);
                    rerank_times_ms.push(rr_start.elapsed().as_millis());
                } else {
                    // Noise case: still rerank so we get noise metric.
                    bundle.capsules =
                        rerank_capsules(&case.query, bundle.capsules, rr, rerank_floor, rerank_cap);
                }

                let ranked_keys: Vec<String> = bundle
                    .capsules
                    .iter()
                    .filter_map(|c| {
                        c.expansion_handle
                            .strip_prefix("memory:")
                            .and_then(|id| id_to_key.get(id))
                            .cloned()
                    })
                    .collect();
                per_case_ranked.push(ranked_keys);
            }

            let (r2, r4, mrr_val, noise) = compute_metrics(&per_case_ranked);

            let rerank_mean_ms = if rerank_times_ms.is_empty() {
                0.0
            } else {
                rerank_times_ms.iter().sum::<u128>() as f64 / rerank_times_ms.len() as f64
            };
            let rerank_max_ms = rerank_times_ms.into_iter().max().unwrap_or(0);

            // Try to find the ONNX file size on disk (best-effort, no panic on miss).
            let onnx_kb: Option<u64> = {
                let low = rr_id.trim().to_ascii_lowercase();
                // Map alias → HF repo id for cache-path lookup.
                let repo_id: &str = match low.as_str() {
                    "jina-reranker-v1-tiny-en" => "jinaai/jina-reranker-v1-tiny-en",
                    "ms-marco-tinybert-l-2-v2" => "Xenova/ms-marco-TinyBERT-L-2-v2",
                    "ms-marco-minilm-l-4-v2" => "Xenova/ms-marco-MiniLM-L-4-v2",
                    "jina-reranker-v1-turbo-en" => "jinaai/jina-reranker-v1-turbo-en",
                    other => other,
                };
                // hf-hub default cache: ~/.cache/huggingface/hub/models--<org>--<name>/snapshots/...
                let home_cache = std::env::var("HF_HOME")
                    .ok()
                    .map(std::path::PathBuf::from)
                    .or_else(|| {
                        std::env::var("HOME")
                            .ok()
                            .or_else(|| std::env::var("USERPROFILE").ok())
                            .map(|h| {
                                std::path::PathBuf::from(h)
                                    .join(".cache")
                                    .join("huggingface")
                                    .join("hub")
                            })
                    });
                home_cache.and_then(|cache_root| {
                    let safe_name = repo_id.replace('/', "--");
                    let snap_dir = cache_root
                        .join(format!("models--{safe_name}"))
                        .join("snapshots");
                    let mut best: Option<u64> = None;
                    if let Ok(snaps) = std::fs::read_dir(&snap_dir) {
                        'snap: for snap in snaps.flatten() {
                            for candidate in ["onnx/model.onnx", "model.onnx"] {
                                let p = snap.path().join(candidate);
                                if let Ok(meta) = std::fs::metadata(&p) {
                                    best = Some(meta.len() / 1024);
                                    break 'snap;
                                }
                            }
                        }
                    }
                    best
                })
            };

            Ok(RankerBenchRow {
                label: rr_id.to_string(),
                load_ms,
                rerank_mean_ms,
                rerank_max_ms,
                r2,
                r4,
                mrr: mrr_val,
                noise,
                onnx_kb,
            })
        };

        println!();
        println!("=== Reranker benchmark (semantic base + per-reranker) ===");
        println!();

        // Print the semantic-only baseline row for comparison.
        let col_w = 28usize;
        println!(
            "{:<col_w$} {:>9} {:>14} {:>13} {:>10} {:>10} {:>10} {:>8} {:>10}",
            "reranker",
            "load_ms",
            "rerank_mean_ms",
            "rerank_max_ms",
            "recall@2",
            "recall@4",
            "MRR",
            "noise",
            "onnx_kb",
        );
        println!("{}", "-".repeat(118));
        println!(
            "{:<col_w$} {:>9} {:>14} {:>13} {:>10.3} {:>10.3} {:>10.3} {:>8.1} {:>10}",
            "(semantic, no rerank)", "-", "-", "-", sem_r2, sem_r4, sem_mrr, sem_noise, "-",
        );

        let mut bench_rows: Vec<RankerBenchRow> = Vec::new();
        for rr_id in &reranker_ids {
            match run_reranker_bench(rr_id) {
                Ok(row) => bench_rows.push(row),
                Err(e) => eprintln!("  {rr_id}: skipped — {e}"),
            }
        }

        for row in &bench_rows {
            let onnx_str = row
                .onnx_kb
                .map(|kb| format!("{kb}"))
                .unwrap_or_else(|| "-".to_string());
            println!(
                "{:<col_w$} {:>9} {:>14.1} {:>13} {:>10.3} {:>10.3} {:>10.3} {:>8.1} {:>10}",
                row.label,
                row.load_ms,
                row.rerank_mean_ms,
                row.rerank_max_ms,
                row.r2,
                row.r4,
                row.mrr,
                row.noise,
                onnx_str,
            );
        }
        println!();
    }

    // ── 8. Clean up temp dir (best-effort) ────────────────────────────────────
    let _ = std::fs::remove_dir_all(&tmp_root);

    Ok(())
}

// ─── kimetsu brain bench ──────────────────────────────────────────────────────

pub(crate) fn brain_bench(args: BrainBenchArgs) -> KimetsuResult<()> {
    #[cfg(feature = "embeddings")]
    {
        brain_bench_inner(args)
    }
    #[cfg(not(feature = "embeddings"))]
    {
        let _ = args;
        println!("kimetsu brain bench requires an embeddings build.");
        println!("Rebuild with: cargo build -p kimetsu-cli --features embeddings");
        Ok(())
    }
}

/// RSS helper (Windows only; returns None on other platforms or on failure).
#[cfg(feature = "embeddings")]
pub(crate) fn rss_mb() -> Option<f64> {
    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::System::ProcessStatus::{
            K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        unsafe {
            let handle = GetCurrentProcess();
            let mut pmc = std::mem::zeroed::<PROCESS_MEMORY_COUNTERS>();
            pmc.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
            if K32GetProcessMemoryInfo(handle, &mut pmc, pmc.cb) != 0 {
                return Some(pmc.WorkingSetSize as f64 / (1024.0 * 1024.0));
            }
        }
        None
    }
    #[cfg(not(target_os = "windows"))]
    {
        None
    }
}

#[cfg(feature = "embeddings")]
pub(crate) fn peak_rss_mb() -> Option<f64> {
    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::System::ProcessStatus::{
            K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        unsafe {
            let handle = GetCurrentProcess();
            let mut pmc = std::mem::zeroed::<PROCESS_MEMORY_COUNTERS>();
            pmc.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
            if K32GetProcessMemoryInfo(handle, &mut pmc, pmc.cb) != 0 {
                return Some(pmc.PeakWorkingSetSize as f64 / (1024.0 * 1024.0));
            }
        }
        None
    }
    #[cfg(not(target_os = "windows"))]
    {
        None
    }
}

#[cfg(feature = "embeddings")]
pub(crate) fn brain_bench_inner(args: BrainBenchArgs) -> KimetsuResult<()> {
    if args.remote {
        brain_bench_remote(args)
    } else if args.single {
        brain_bench_single(args)
    } else {
        brain_bench_orchestrate(args)
    }
}

/// Orchestrator: spawn one child per embedder×reranker combo, wait for all,
/// read per-combo JSON files, print + write summary.
#[cfg(feature = "embeddings")]
pub(crate) fn brain_bench_orchestrate(args: BrainBenchArgs) -> KimetsuResult<()> {
    use std::time::Instant;

    let embedders: Vec<&str> = args
        .embedders
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let rerankers: Vec<&str> = args
        .rerankers
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();

    let dataset = args.dataset.clone();
    let out_dir = args.out.clone();
    let pool = args.pool;
    let cap = args.cap;

    std::fs::create_dir_all(&out_dir)?;

    let current_exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let dataset_str = dataset.to_string_lossy().to_string();
    let out_str = out_dir.to_string_lossy().to_string();

    let total = embedders.len() * rerankers.len();
    println!(
        "brain bench: {} embedder(s) × {} reranker(s) = {} combos",
        embedders.len(),
        rerankers.len(),
        total
    );
    println!("dataset: {}", dataset.display());
    println!("output:  {}", out_dir.display());
    println!();

    let mut combo_idx = 0usize;
    for &embedder in &embedders {
        for &reranker in &rerankers {
            combo_idx += 1;
            print!("[{combo_idx}/{total}] {embedder} × {reranker} ... ");
            let _ = std::io::Write::flush(&mut std::io::stdout());

            let t0 = Instant::now();
            let status = std::process::Command::new(&current_exe)
                .arg("brain")
                .arg("bench")
                .arg("--dataset")
                .arg(&dataset_str)
                .arg("--embedders")
                .arg(embedder)
                .arg("--rerankers")
                .arg(reranker)
                .arg("--pool")
                .arg(pool.to_string())
                .arg("--cap")
                .arg(cap.to_string())
                .arg("--out")
                .arg(&out_str)
                .arg("--single")
                .status()
                .map_err(|e| format!("spawn child for {embedder}×{reranker}: {e}"))?;

            let elapsed = t0.elapsed().as_secs_f64();
            if status.success() {
                println!("done ({elapsed:.1}s)");
            } else {
                println!("FAILED (exit={status})");
            }
        }
    }

    // Read all combo JSON files and build summary rows.
    println!();
    println!("reading results...");

    #[derive(serde::Deserialize)]
    struct ComboSummary {
        recall_at_2: f64,
        recall_at_4: f64,
        mrr: f64,
        mean_latency_ms: f64,
        p95_latency_ms: f64,
        noise_capsules: f64,
        /// v1.5 (Story 2.1): mean rendered tokens per capsule after compression.
        #[serde(default)]
        rendered_tokens_mean: f64,
        /// v1.5 (Story 2.1): mean raw (uncompressed) tokens per capsule.
        #[serde(default)]
        raw_tokens_mean: f64,
        /// P0.1: mean stale-hit rate (lower is better; 0.0 = no stale in any case).
        #[serde(default)]
        stale_hit_rate: f64,
        /// P0.1: fraction of correctness cases resolved correctly (-1.0 = N/A).
        #[serde(default = "default_resolution_accuracy")]
        resolution_accuracy: f64,
    }
    fn default_resolution_accuracy() -> f64 {
        -1.0
    }
    #[derive(serde::Deserialize)]
    struct ComboResult {
        embedder: String,
        reranker: String,
        embedder_load_ms: u128,
        reranker_load_ms: u128,
        peak_rss_mb: Option<f64>,
        summary: ComboSummary,
    }

    let mut rows: Vec<ComboResult> = Vec::new();
    for &embedder in &embedders {
        for &reranker in &rerankers {
            let safe_emb = embedder.replace(['/', '.', ' '], "-");
            let safe_rr = reranker.replace(['/', '.', ' '], "-");
            let fname = format!("combo-{safe_emb}-{safe_rr}.json");
            let fpath = out_dir.join(&fname);
            match std::fs::read_to_string(&fpath) {
                Ok(text) => match serde_json::from_str::<ComboResult>(&text) {
                    Ok(r) => rows.push(r),
                    Err(e) => eprintln!("  warning: parse {fname}: {e}"),
                },
                Err(e) => eprintln!("  warning: read {fname}: {e}"),
            }
        }
    }

    // Sort by MRR desc.
    rows.sort_by(|a, b| {
        b.summary
            .mrr
            .partial_cmp(&a.summary.mrr)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Build summary table.
    let header = format!(
        "| {:<25} | {:<35} | {:>8} | {:>8} | {:>7} | {:>8} | {:>7} | {:>10} | {:>15} | {:>11} | {:>12} | {:>14} | {:>14} | {:>19} |",
        "embedder",
        "reranker",
        "recall@2",
        "recall@4",
        "MRR",
        "mean ms",
        "p95 ms",
        "noise_caps",
        "load ms (emb+rr)",
        "peak RSS MB",
        "raw_tok_mean",
        "rend_tok_mean",
        "stale_hit_rate",
        "resolution_accuracy",
    );
    let sep = format!(
        "| {:-<25} | {:-<35} | {:-<8} | {:-<8} | {:-<7} | {:-<8} | {:-<7} | {:-<10} | {:-<15} | {:-<11} | {:-<12} | {:-<14} | {:-<14} | {:-<19} |",
        "", "", "", "", "", "", "", "", "", "", "", "", "", ""
    );

    let mut table_lines: Vec<String> = vec![header, sep];
    for row in &rows {
        let load_ms = row.embedder_load_ms + row.reranker_load_ms;
        let rss_str = row
            .peak_rss_mb
            .map(|v| format!("{v:.0}"))
            .unwrap_or_else(|| "n/a".to_string());
        let res_acc_str = if row.summary.resolution_accuracy < 0.0 {
            "N/A".to_string()
        } else {
            format!("{:.3}", row.summary.resolution_accuracy)
        };
        table_lines.push(format!(
            "| {:<25} | {:<35} | {:>8.3} | {:>8.3} | {:>7.3} | {:>8.1} | {:>7.1} | {:>10.1} | {:>15} | {:>11} | {:>12.1} | {:>14.1} | {:>14.3} | {:>19} |",
            row.embedder,
            row.reranker,
            row.summary.recall_at_2,
            row.summary.recall_at_4,
            row.summary.mrr,
            row.summary.mean_latency_ms,
            row.summary.p95_latency_ms,
            row.summary.noise_capsules,
            load_ms,
            rss_str,
            row.summary.raw_tokens_mean,
            row.summary.rendered_tokens_mean,
            row.summary.stale_hit_rate,
            res_acc_str,
        ));
    }

    let summary_md = format!(
        "# Kimetsu Retrieval Benchmark — Summary\n\nSorted by MRR descending.\n\n{}\n",
        table_lines.join("\n")
    );

    let summary_path = out_dir.join("summary.md");
    std::fs::write(&summary_path, &summary_md)?;
    println!("wrote {}", summary_path.display());
    println!();
    println!("{summary_md}");

    Ok(())
}

/// RSS of an external process by PID (Windows only).
#[cfg(all(feature = "embeddings", target_os = "windows"))]
pub(crate) fn process_rss_mb(pid: u32) -> Option<f64> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ,
    };
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, 0, pid);
        if handle.is_null() {
            return None;
        }
        let mut pmc = std::mem::zeroed::<PROCESS_MEMORY_COUNTERS>();
        pmc.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        let ok = K32GetProcessMemoryInfo(handle, &mut pmc, pmc.cb) != 0;
        CloseHandle(handle);
        if ok {
            Some(pmc.WorkingSetSize as f64 / (1024.0 * 1024.0))
        } else {
            None
        }
    }
}

#[cfg(all(feature = "embeddings", not(target_os = "windows")))]
pub(crate) fn process_rss_mb(_pid: u32) -> Option<f64> {
    None
}

/// Remote bench: spawn kimetsu-remote, seed a temp brain, measure HTTP MCP retrieval.
#[cfg(feature = "embeddings")]
pub(crate) fn brain_bench_remote(args: BrainBenchArgs) -> KimetsuResult<()> {
    use kimetsu_brain::eval::EvalFixture;
    use kimetsu_brain::project::{add_memory, init_project};
    use kimetsu_core::memory::{MemoryKind, MemoryScope};
    use kimetsu_core::paths::git_init_boundary;
    use std::collections::HashMap;
    use std::net::TcpListener;
    use std::time::Instant;

    // ── 0. Locate workspace root and server binary ────────────────────────────
    // Find workspace root by walking up from current_exe.
    let current_exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    // target/release/kimetsu.exe  →  workspace root is three levels up.
    let workspace_root = current_exe
        .parent() // target/release/
        .and_then(|p| p.parent()) // target/
        .and_then(|p| p.parent()) // workspace root
        .map(|p| p.to_path_buf())
        .ok_or_else(|| "cannot derive workspace root from current_exe".to_string())?;

    #[cfg(windows)]
    let server_bin = workspace_root
        .join("target")
        .join("release")
        .join("kimetsu-remote.exe");
    #[cfg(not(windows))]
    let server_bin = workspace_root
        .join("target")
        .join("release")
        .join("kimetsu-remote");

    if !server_bin.exists() {
        return Err(format!(
            "kimetsu-remote release binary not found at {}\n\
             Build it first:\n  cargo build --release -p kimetsu-remote --features embeddings",
            server_bin.display()
        )
        .into());
    }

    // ── 1. Load fixture ───────────────────────────────────────────────────────
    let fixture_text = std::fs::read_to_string(&args.dataset)
        .map_err(|e| format!("cannot read dataset {}: {e}", args.dataset.display()))?;
    let fixture: EvalFixture =
        serde_json::from_str(&fixture_text).map_err(|e| format!("invalid dataset JSON: {e}"))?;

    let all_keys: std::collections::HashSet<&str> =
        fixture.memories.iter().map(|m| m.key.as_str()).collect();
    for case in &fixture.cases {
        for rel in &case.relevant {
            if !all_keys.contains(rel.as_str()) {
                return Err(format!(
                    "dataset validation: relevant key {:?} in query {:?} not in memories",
                    rel, case.query
                )
                .into());
            }
        }
        for stale in &case.stale {
            if !all_keys.contains(stale.as_str()) {
                return Err(format!(
                    "dataset validation: stale key {:?} in query {:?} not in memories",
                    stale, case.query
                )
                .into());
            }
        }
    }

    let embedders: Vec<&str> = args
        .embedders
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();

    println!(
        "brain bench --remote: {} embedder(s) (server reranks with --reranker default jina-tiny)",
        embedders.len()
    );
    println!(
        "NOTE: remote applies PRODUCTION floors (min_lexical_coverage 0.5, min_semantic_score 0.35)."
    );
    println!("      Quality numbers are NOT directly comparable to local floors-off results.");
    println!("dataset: {}", args.dataset.display());
    println!("output:  {}", args.out.display());
    println!("concurrency: {}", args.concurrency);
    println!();

    std::fs::create_dir_all(&args.out)?;

    #[derive(serde::Serialize)]
    struct RemoteCaseResult {
        query: String,
        expected: Vec<String>,
        obtained: Vec<String>,
        hit_at_2: bool,
        hit_at_4: bool,
        mrr: f64,
        latency_ms: u128,
        error: Option<String>,
    }

    #[derive(serde::Serialize)]
    struct RemoteComboResult {
        embedder: String,
        seed_ms: u128,
        rss_after_warm_mb: Option<f64>,
        peak_rss_mb: Option<f64>,
        cases: Vec<RemoteCaseResult>,
        summary: RemoteComboSummary,
        concurrent: RemoteConcurrentStats,
    }

    #[derive(serde::Serialize)]
    struct RemoteComboSummary {
        recall_at_2: f64,
        recall_at_4: f64,
        mrr: f64,
        mean_latency_ms: f64,
        p95_latency_ms: f64,
        noise_capsules: f64,
        error_cases: usize,
    }

    #[derive(serde::Serialize)]
    struct RemoteConcurrentStats {
        mean_ms: f64,
        p95_ms: f64,
        total_wall_ms: u128,
        throughput_rps: f64,
    }

    type SummaryRow = (
        String,
        RemoteComboSummary,
        RemoteConcurrentStats,
        Option<f64>,
        Option<f64>,
    );
    let mut summary_rows: Vec<SummaryRow> = Vec::new();

    for &embedder_id in &embedders {
        println!("[remote] embedder: {embedder_id}");

        // ── 2. Pick a free port ───────────────────────────────────────────────
        let listener =
            TcpListener::bind("127.0.0.1:0").map_err(|e| format!("bind free port: {e}"))?;
        let port = listener
            .local_addr()
            .map_err(|e| format!("local_addr: {e}"))?
            .port();
        drop(listener); // release so the server can bind it

        // ── 3. Seed temp brain ────────────────────────────────────────────────
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let safe_emb = embedder_id.replace(['/', '.', ' '], "-");
        // data dir: contains benchrepo/
        let data_dir = std::env::temp_dir().join(format!("kimetsu-remote-bench-{safe_emb}-{ts}"));
        let repo_root = data_dir.join("benchrepo");
        std::fs::create_dir_all(&repo_root)?;
        git_init_boundary(&repo_root);

        // Set env before seeding so memories use this embedder.
        unsafe {
            std::env::set_var("KIMETSU_BRAIN_EMBEDDER", embedder_id);
            std::env::set_var("KIMETSU_USER_BRAIN", "0");
        }

        let t_seed = Instant::now();
        init_project(&repo_root, false).map_err(|e| format!("init_project: {e}"))?;

        let mut key_to_id: HashMap<String, String> = HashMap::new();
        for mem in &fixture.memories {
            let id = add_memory(
                &repo_root,
                MemoryScope::Project,
                MemoryKind::Fact,
                &mem.text,
            )
            .map_err(|e| format!("add_memory {:?}: {e}", mem.key))?;
            key_to_id.insert(mem.key.clone(), id);
        }
        let seed_ms = t_seed.elapsed().as_millis();
        let id_to_key: HashMap<String, String> = key_to_id
            .iter()
            .map(|(k, v)| (v.clone(), k.clone()))
            .collect();
        println!(
            "  seeded {} memories in {seed_ms}ms",
            fixture.memories.len()
        );

        // ── 4. Spawn server ───────────────────────────────────────────────────
        let addr = format!("127.0.0.1:{port}");
        let token = "benchtoken";
        let server = std::process::Command::new(&server_bin)
            .arg("serve")
            .arg("--addr")
            .arg(&addr)
            .arg("--data")
            .arg(&data_dir)
            .arg("--token")
            .arg(token)
            .arg("--rate-limit")
            .arg("0")
            .env("KIMETSU_BRAIN_EMBEDDER", embedder_id)
            .env("KIMETSU_USER_BRAIN", "0")
            .env("KIMETSU_MCP_ENABLE_WRITE_TOOLS", "1")
            // Suppress server log noise during bench
            .env("RUST_LOG", "warn")
            .spawn()
            .map_err(|e| format!("spawn kimetsu-remote: {e}"))?;

        // Kill-on-drop guard: any `?` between here and the explicit kill
        // below would otherwise orphan a live server holding its port and
        // a lock on the temp data dir.
        struct ChildGuard(std::process::Child);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut server = ChildGuard(server);

        let server_pid = server.0.id();

        // ── 5. Poll readiness (GET /healthz, up to 60s) ───────────────────────
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .map_err(|e| format!("build reqwest client: {e}"))?;

        let health_url = format!("http://{addr}/healthz");
        let deadline = Instant::now() + std::time::Duration::from_secs(60);
        let mut ready = false;
        while Instant::now() < deadline {
            match client.get(&health_url).send() {
                Ok(r) if r.status().is_success() => {
                    ready = true;
                    break;
                }
                _ => std::thread::sleep(std::time::Duration::from_millis(200)),
            }
        }
        if !ready {
            let _ = server.0.kill();
            return Err(
                format!("kimetsu-remote did not become ready within 60s (port {port})").into(),
            );
        }
        println!("  server ready on :{port}");

        // ── 6. Record RSS after warm ──────────────────────────────────────────
        let rss_after_warm = process_rss_mb(server_pid);

        // ── 7. Sequential pass ────────────────────────────────────────────────
        let mcp_url = format!("http://{addr}/mcp/benchrepo");
        let auth_header = format!("Bearer {token}");

        // Helper: call kimetsu_brain_context over HTTP, return (obtained_keys, latency_ms, error).
        let call_context = |query: &str, id: u64| -> (Vec<String>, u128, Option<String>) {
            let body = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": {
                    "name": "kimetsu_brain_context",
                    "arguments": {
                        "query": query,
                        "budget_tokens": 6000,
                        "max_capsules": 4
                    }
                }
            });
            let t0 = Instant::now();
            let resp = client
                .post(&mcp_url)
                .header("Authorization", &auth_header)
                .header("Content-Type", "application/json")
                .json(&body)
                .send();
            let latency_ms = t0.elapsed().as_millis();

            let resp = match resp {
                Ok(r) => r,
                Err(e) => return (vec![], latency_ms, Some(format!("HTTP error: {e}"))),
            };

            let json: serde_json::Value = match resp.json() {
                Ok(v) => v,
                Err(e) => return (vec![], latency_ms, Some(format!("JSON parse error: {e}"))),
            };

            // Check for JSON-RPC error
            if let Some(err_obj) = json.get("error") {
                let msg = err_obj
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown error");
                return (vec![], latency_ms, Some(format!("RPC error: {msg}")));
            }

            // Parse the result: result.content[0].text → JSON string → capsules
            let text = json
                .get("result")
                .and_then(|r| r.get("content"))
                .and_then(|c| c.get(0))
                .and_then(|c| c.get("text"))
                .and_then(|t| t.as_str())
                .unwrap_or("");

            if text.is_empty() {
                return (vec![], latency_ms, Some("empty text in result".to_string()));
            }

            let inner: serde_json::Value = match serde_json::from_str(text) {
                Ok(v) => v,
                Err(e) => return (vec![], latency_ms, Some(format!("inner JSON parse: {e}"))),
            };

            // skipped case → no capsules (intentional, not an error)
            if inner
                .get("skipped")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                return (vec![], latency_ms, None);
            }

            let capsules = inner
                .get("capsules")
                .and_then(|c| c.as_array())
                .cloned()
                .unwrap_or_default();

            let keys: Vec<String> = capsules
                .iter()
                .filter_map(|cap| {
                    cap.get("expansion_handle")
                        .and_then(|h| h.as_str())
                        .and_then(|h| h.strip_prefix("memory:"))
                        .and_then(|id| id_to_key.get(id))
                        .cloned()
                })
                .collect();

            (keys, latency_ms, None)
        };

        let mut case_results: Vec<RemoteCaseResult> = Vec::new();
        let mut seq_latencies: Vec<u128> = Vec::new();

        for (idx, case) in fixture.cases.iter().enumerate() {
            let (obtained, latency_ms, error) = call_context(&case.query, idx as u64);
            seq_latencies.push(latency_ms);

            let hit_at_2 = if case.relevant.is_empty() {
                false
            } else {
                obtained.iter().take(2).any(|k| case.relevant.contains(k))
            };
            let hit_at_4 = if case.relevant.is_empty() {
                false
            } else {
                obtained.iter().take(4).any(|k| case.relevant.contains(k))
            };
            let mrr_val = kimetsu_brain::eval::mrr(&obtained, &case.relevant);

            case_results.push(RemoteCaseResult {
                query: case.query.clone(),
                expected: case.relevant.clone(),
                obtained,
                hit_at_2,
                hit_at_4,
                mrr: mrr_val,
                latency_ms,
                error,
            });
        }

        println!("  sequential pass done ({} cases)", case_results.len());

        // ── 8. Concurrent pass ────────────────────────────────────────────────
        let concurrency = args.concurrency.max(1);
        let cases_arc: std::sync::Arc<Vec<_>> = std::sync::Arc::new(
            fixture
                .cases
                .iter()
                .enumerate()
                .map(|(i, c)| (i, c.query.clone()))
                .collect(),
        );
        let t_conc_start = Instant::now();

        // Split cases into chunks for each worker thread.
        let chunk_size = cases_arc.len().div_ceil(concurrency);
        let mut handles = vec![];
        let client_clone = client.clone();
        let mcp_url_clone = mcp_url.clone();
        let auth_clone = auth_header.clone();
        let id_to_key_arc = std::sync::Arc::new(id_to_key.clone());

        // We collect latencies per case from concurrent workers.
        let conc_latencies_arc: std::sync::Arc<std::sync::Mutex<Vec<(usize, u128)>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

        for chunk_idx in 0..concurrency {
            let cases = std::sync::Arc::clone(&cases_arc);
            let client_t = client_clone.clone();
            let url_t = mcp_url_clone.clone();
            let auth_t = auth_clone.clone();
            let id_to_key_t = std::sync::Arc::clone(&id_to_key_arc);
            let out_t = std::sync::Arc::clone(&conc_latencies_arc);

            let start = chunk_idx * chunk_size;
            let end = (start + chunk_size).min(cases.len());
            if start >= end {
                continue;
            }

            let handle = std::thread::spawn(move || {
                for case_idx in start..end {
                    let (i, ref query) = cases[case_idx];
                    let body = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": i as u64 + 10000,
                        "method": "tools/call",
                        "params": {
                            "name": "kimetsu_brain_context",
                            "arguments": {
                                "query": query,
                                "budget_tokens": 6000,
                                "max_capsules": 4
                            }
                        }
                    });
                    let t0 = Instant::now();
                    let _ = client_t
                        .post(&url_t)
                        .header("Authorization", &auth_t)
                        .header("Content-Type", "application/json")
                        .json(&body)
                        .send();
                    let latency_ms = t0.elapsed().as_millis();
                    let _ = id_to_key_t.get(""); // suppress unused warning
                    out_t.lock().unwrap().push((i, latency_ms));
                }
            });
            handles.push(handle);
        }
        for h in handles {
            let _ = h.join();
        }
        let total_wall_ms = t_conc_start.elapsed().as_millis();
        let conc_lats_raw = conc_latencies_arc.lock().unwrap().clone();
        let mut conc_latencies: Vec<u128> = conc_lats_raw.iter().map(|(_, l)| *l).collect();
        conc_latencies.sort_unstable();

        let conc_mean_ms = if conc_latencies.is_empty() {
            0.0
        } else {
            conc_latencies.iter().sum::<u128>() as f64 / conc_latencies.len() as f64
        };
        let conc_p95_ms = if conc_latencies.is_empty() {
            0.0
        } else {
            let idx = ((conc_latencies.len() as f64 * 0.95) as usize).min(conc_latencies.len() - 1);
            conc_latencies[idx] as f64
        };
        let throughput_rps = if total_wall_ms == 0 {
            0.0
        } else {
            fixture.cases.len() as f64 / (total_wall_ms as f64 / 1000.0)
        };

        println!(
            "  concurrent pass done: mean={conc_mean_ms:.0}ms p95={conc_p95_ms:.0}ms throughput={throughput_rps:.1}rps"
        );

        // ── 9. Record peak RSS, kill server ───────────────────────────────────
        let peak_rss = process_rss_mb(server_pid);
        let _ = server.0.kill();
        let _ = server.0.wait();
        let _ = std::fs::remove_dir_all(&data_dir);

        // ── 10. Aggregate metrics ─────────────────────────────────────────────
        let signal_cases: Vec<_> = fixture
            .cases
            .iter()
            .zip(&case_results)
            .filter(|(c, _)| !c.relevant.is_empty())
            .collect();
        let noise_cases: Vec<_> = fixture
            .cases
            .iter()
            .zip(&case_results)
            .filter(|(c, _)| c.relevant.is_empty())
            .collect();

        let recall_at_2 = if signal_cases.is_empty() {
            0.0
        } else {
            signal_cases
                .iter()
                .map(|(_, r)| if r.hit_at_2 { 1.0f64 } else { 0.0 })
                .sum::<f64>()
                / signal_cases.len() as f64
        };
        let recall_at_4 = if signal_cases.is_empty() {
            0.0
        } else {
            signal_cases
                .iter()
                .map(|(_, r)| if r.hit_at_4 { 1.0f64 } else { 0.0 })
                .sum::<f64>()
                / signal_cases.len() as f64
        };
        let mrr_avg = if signal_cases.is_empty() {
            0.0
        } else {
            signal_cases.iter().map(|(_, r)| r.mrr).sum::<f64>() / signal_cases.len() as f64
        };
        let mut sorted_seq = seq_latencies.clone();
        sorted_seq.sort_unstable();
        let mean_latency_ms = if sorted_seq.is_empty() {
            0.0
        } else {
            sorted_seq.iter().sum::<u128>() as f64 / sorted_seq.len() as f64
        };
        let p95_latency_ms = if sorted_seq.is_empty() {
            0.0
        } else {
            let idx = ((sorted_seq.len() as f64 * 0.95) as usize).min(sorted_seq.len() - 1);
            sorted_seq[idx] as f64
        };
        let noise_capsules = if noise_cases.is_empty() {
            0.0
        } else {
            noise_cases
                .iter()
                .map(|(_, r)| r.obtained.len() as f64)
                .sum::<f64>()
                / noise_cases.len() as f64
        };
        let error_cases = case_results.iter().filter(|r| r.error.is_some()).count();

        let summary = RemoteComboSummary {
            recall_at_2,
            recall_at_4,
            mrr: mrr_avg,
            mean_latency_ms,
            p95_latency_ms,
            noise_capsules,
            error_cases,
        };
        let concurrent = RemoteConcurrentStats {
            mean_ms: conc_mean_ms,
            p95_ms: conc_p95_ms,
            total_wall_ms,
            throughput_rps,
        };

        println!(
            "  recall@2={:.3} recall@4={:.3} MRR={:.3} seq_mean={:.0}ms seq_p95={:.0}ms errors={}",
            summary.recall_at_2,
            summary.recall_at_4,
            summary.mrr,
            summary.mean_latency_ms,
            summary.p95_latency_ms,
            summary.error_cases,
        );

        // ── 11. Write per-embedder JSON ───────────────────────────────────────
        let combo = RemoteComboResult {
            embedder: embedder_id.to_string(),
            seed_ms,
            rss_after_warm_mb: rss_after_warm,
            peak_rss_mb: peak_rss,
            cases: case_results,
            summary: RemoteComboSummary {
                recall_at_2,
                recall_at_4,
                mrr: mrr_avg,
                mean_latency_ms,
                p95_latency_ms,
                noise_capsules,
                error_cases,
            },
            concurrent: RemoteConcurrentStats {
                mean_ms: conc_mean_ms,
                p95_ms: conc_p95_ms,
                total_wall_ms,
                throughput_rps,
            },
        };
        let fname = format!("remote-{safe_emb}.json");
        let fpath = args.out.join(&fname);
        std::fs::write(&fpath, serde_json::to_string_pretty(&combo)?)?;
        println!("  wrote {}", fpath.display());
        println!();

        summary_rows.push((
            embedder_id.to_string(),
            summary,
            concurrent,
            rss_after_warm,
            peak_rss,
        ));
    }

    // ── 12. Write summary table ───────────────────────────────────────────────
    let caveat = "\
> **NOTE — remote production floors**: the remote path applies `min_lexical_coverage = 0.5` and \
the AUTO semantic floor (0.35 on bge-family, 0.0 elsewhere — cosine scales are model-dependent). \
Quality numbers are **NOT** directly comparable to the local bench's floors-off results — noise \
cases dropped by the floors are intentional precision wins, not recall failures. The remote server \
reranks with `--reranker` (default `jina-reranker-v1-tiny-en`, operator-level, `off` disables).\n";

    let header = format!(
        "| {:<25} | {:>8} | {:>8} | {:>7} | {:>9} | {:>8} | {:>12} | {:>10} | {:>14} | {:>11} | {:>11} |",
        "embedder",
        "recall@2",
        "recall@4",
        "MRR",
        "seq mean",
        "seq p95",
        "conc mean ms",
        "conc p95",
        "throughput rps",
        "warm RSS MB",
        "peak RSS MB"
    );
    let sep = format!(
        "| {:-<25} | {:-<8} | {:-<8} | {:-<7} | {:-<9} | {:-<8} | {:-<12} | {:-<10} | {:-<14} | {:-<11} | {:-<11} |",
        "", "", "", "", "", "", "", "", "", "", ""
    );

    let mut table_lines = vec![header, sep];
    for (embedder, summary, concurrent, warm_rss, peak_rss) in &summary_rows {
        let warm_str = warm_rss
            .map(|v| format!("{v:.0}"))
            .unwrap_or_else(|| "n/a".to_string());
        let peak_str = peak_rss
            .map(|v| format!("{v:.0}"))
            .unwrap_or_else(|| "n/a".to_string());
        table_lines.push(format!(
            "| {:<25} | {:>8.3} | {:>8.3} | {:>7.3} | {:>9.1} | {:>8.1} | {:>12.1} | {:>10.1} | {:>14.1} | {:>11} | {:>11} |",
            embedder,
            summary.recall_at_2,
            summary.recall_at_4,
            summary.mrr,
            summary.mean_latency_ms,
            summary.p95_latency_ms,
            concurrent.mean_ms,
            concurrent.p95_ms,
            concurrent.throughput_rps,
            warm_str,
            peak_str,
        ));
    }

    let summary_md = format!(
        "# Kimetsu Remote Benchmark — Summary\n\n{caveat}\nSorted by embedder.\n\n{}\n",
        table_lines.join("\n")
    );

    let summary_path = args.out.join("remote-summary.md");
    std::fs::write(&summary_path, &summary_md)?;
    println!("wrote {}", summary_path.display());
    println!();
    println!("{summary_md}");

    Ok(())
}

/// Worker: run a single embedder×reranker combo in-process, write combo JSON.
#[cfg(feature = "embeddings")]
pub(crate) fn brain_bench_single(args: BrainBenchArgs) -> KimetsuResult<()> {
    use kimetsu_brain::context::{ContextRequest, rerank_capsules};
    use kimetsu_brain::embeddings::{open_embedder_for_model, open_reranker_for_model};
    use kimetsu_brain::eval::EvalFixture;
    use kimetsu_brain::project::{BrainSession, add_memory, init_project};
    use kimetsu_core::memory::{MemoryKind, MemoryScope};
    use kimetsu_core::paths::git_init_boundary;
    use std::collections::HashMap;
    use std::time::Instant;

    // Disable user brain.
    unsafe {
        std::env::set_var("KIMETSU_USER_BRAIN", "0");
    }

    let embedder_id = args
        .embedders
        .split(',')
        .map(str::trim)
        .find(|s| !s.is_empty())
        .unwrap_or("bge-small-en-v1.5")
        .to_string();
    let reranker_id = args
        .rerankers
        .split(',')
        .map(str::trim)
        .find(|s| !s.is_empty())
        .unwrap_or("off")
        .to_string();

    // ── 1. Load fixture ───────────────────────────────────────────────────────
    let fixture_text = std::fs::read_to_string(&args.dataset)
        .map_err(|e| format!("cannot read dataset {}: {e}", args.dataset.display()))?;
    let fixture: EvalFixture =
        serde_json::from_str(&fixture_text).map_err(|e| format!("invalid dataset JSON: {e}"))?;

    let all_keys: std::collections::HashSet<&str> =
        fixture.memories.iter().map(|m| m.key.as_str()).collect();
    for case in &fixture.cases {
        for rel in &case.relevant {
            if !all_keys.contains(rel.as_str()) {
                return Err(format!(
                    "dataset validation: relevant key {:?} in query {:?} not in memories",
                    rel, case.query
                )
                .into());
            }
        }
        for stale in &case.stale {
            if !all_keys.contains(stale.as_str()) {
                return Err(format!(
                    "dataset validation: stale key {:?} in query {:?} not in memories",
                    stale, case.query
                )
                .into());
            }
        }
    }

    // ── 2. Load embedder (measure RSS before/after) ───────────────────────────
    let rss_before_emb = rss_mb();
    let t_emb = Instant::now();
    // Set env so seeds use THIS embedder.
    unsafe {
        std::env::set_var("KIMETSU_BRAIN_EMBEDDER", &embedder_id);
    }
    let embedder = open_embedder_for_model(&embedder_id);
    let embedder_load_ms = t_emb.elapsed().as_millis();
    let rss_after_emb = rss_mb();

    // ── 3. Load reranker ──────────────────────────────────────────────────────
    let rss_before_rr = rss_mb();
    let t_rr = Instant::now();
    let reranker_box: Option<Box<dyn kimetsu_brain::embeddings::Reranker>> = if reranker_id == "off"
    {
        None
    } else {
        open_reranker_for_model(&reranker_id)
    };
    let reranker_load_ms = t_rr.elapsed().as_millis();
    let rss_after_rr = rss_mb();

    // ── 4. Seed temp brain ────────────────────────────────────────────────────
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let safe_emb = embedder_id.replace(['/', '.', ' '], "-");
    let safe_rr = reranker_id.replace(['/', '.', ' '], "-");
    let tmp_root = std::env::temp_dir().join(format!("kimetsu-bench-{safe_emb}-{safe_rr}-{ts}"));
    std::fs::create_dir_all(&tmp_root)?;
    git_init_boundary(&tmp_root);

    let t_seed = Instant::now();
    init_project(&tmp_root, true).map_err(|e| format!("init_project: {e}"))?;

    let mut key_to_id: HashMap<String, String> = HashMap::new();
    for mem in &fixture.memories {
        let id = add_memory(&tmp_root, MemoryScope::Project, MemoryKind::Fact, &mem.text)
            .map_err(|e| format!("add_memory {:?}: {e}", mem.key))?;
        key_to_id.insert(mem.key.clone(), id);
    }

    // ── 4b. Apply temporal validity state ────────────────────────────────────
    // Flagship 1 Pass A: for memories with `valid_to` or `superseded_by_key`,
    // stamp the temporal state so validity-aware retrieval can exclude them.
    // Memories without these fields are unchanged — existing fixtures are safe.
    {
        use kimetsu_brain::projector::mark_memory_temporal;
        use kimetsu_core::paths::ProjectPaths;

        let needs_temporal = fixture
            .memories
            .iter()
            .any(|m| m.valid_to.is_some() || m.superseded_by_key.is_some());

        if needs_temporal {
            let paths = ProjectPaths::discover(&tmp_root)
                .map_err(|e| format!("discover paths for temporal seeding: {e}"))?;
            let conn = rusqlite::Connection::open(&paths.brain_db)
                .map_err(|e| format!("open brain_db for temporal seeding: {e}"))?;
            kimetsu_brain::schema::initialize(&conn)
                .map_err(|e| format!("initialize brain for temporal seeding: {e}"))?;

            for mem in &fixture.memories {
                if mem.valid_to.is_none() && mem.superseded_by_key.is_none() {
                    continue;
                }
                let memory_id = match key_to_id.get(&mem.key) {
                    Some(id) => id.clone(),
                    None => continue,
                };

                // Stamp valid_to (expiry) via the memory.temporal event so the
                // action is event-sourced and rebuild-safe.
                if let Some(ref vt) = mem.valid_to {
                    mark_memory_temporal(&conn, &memory_id, None, Some(vt.as_str()))
                        .map_err(|e| format!("mark_memory_temporal valid_to {:?}: {e}", mem.key))?;
                }

                // Stamp superseded_by via a direct SQL update.
                // We use a direct UPDATE rather than a full memory.superseded event
                // because the bench seeder just needs the retrieval exclusion; it
                // doesn't need the full edge + citation reassignment that the
                // consolidation path does.
                if let Some(ref survivor_key) = mem.superseded_by_key {
                    if let Some(survivor_id) = key_to_id.get(survivor_key) {
                        conn.execute(
                            "UPDATE memories SET superseded_by = ?2 WHERE memory_id = ?1",
                            rusqlite::params![&memory_id, survivor_id],
                        )
                        .map_err(|e| {
                            format!(
                                "stamp superseded_by {:?} → {:?}: {e}",
                                mem.key, survivor_key
                            )
                        })?;
                        // Also remove from FTS so FTS path doesn't surface it.
                        conn.execute(
                            "DELETE FROM memories_fts WHERE memory_id = ?1",
                            rusqlite::params![&memory_id],
                        )
                        .map_err(|e| {
                            format!("delete memories_fts for superseded {:?}: {e}", mem.key)
                        })?;
                    }
                }
            }
        }
    }

    let seed_ms = t_seed.elapsed().as_millis();
    let id_to_key: HashMap<String, String> = key_to_id
        .iter()
        .map(|(k, v)| (v.clone(), k.clone()))
        .collect();

    // ── 5. Run cases ─────────────────────────────────────────────────────────
    let session =
        BrainSession::open_readonly(&tmp_root).map_err(|e| format!("open_readonly: {e}"))?;

    #[derive(serde::Serialize)]
    struct ObtainedItem {
        key: String,
        score: f32,
    }
    #[derive(serde::Serialize)]
    struct CaseResult {
        query: String,
        expected: Vec<String>,
        obtained: Vec<ObtainedItem>,
        hit_at_2: bool,
        hit_at_4: bool,
        mrr: f64,
        latency_ms: u128,
        /// v1.5 (Story 2.1): mean rendered tokens across the returned capsules
        /// after compress_for_render(3) vs raw token estimates.
        raw_tokens_mean: f64,
        rendered_tokens_mean: f64,
        /// P0.1: 1.0 if any stale key is in the top-k window, else 0.0.
        stale_hit: f64,
        /// P0.1: true if relevant outranks every stale key in ranked list.
        resolution_correct: bool,
    }

    let mut case_results: Vec<CaseResult> = Vec::new();
    let mut latencies_ms: Vec<u128> = Vec::new();

    for case in &fixture.cases {
        let t0 = Instant::now();
        let request = ContextRequest {
            stage: "localization".to_string(),
            query: case.query.clone(),
            budget_tokens: 6000,
            max_capsules: args.pool,
            min_semantic_score: 0.0,
            min_lexical_coverage: 0.0,
            ..Default::default()
        };
        let mut bundle = session
            .retrieve_context_with_injected_embedder(request, embedder.as_ref())
            .map_err(|e| format!("retrieve: {e}"))?;

        // Apply reranker or truncate.
        if let Some(ref rr) = reranker_box {
            bundle.capsules =
                rerank_capsules(&case.query, bundle.capsules, rr.as_ref(), 0.0, args.cap);
        } else {
            bundle.capsules.truncate(args.cap);
        }

        let latency_ms = t0.elapsed().as_millis();
        latencies_ms.push(latency_ms);

        // Map expansion_handle "memory:<id>" → key.
        let obtained: Vec<ObtainedItem> = bundle
            .capsules
            .iter()
            .map(|c| {
                let key = c
                    .expansion_handle
                    .strip_prefix("memory:")
                    .and_then(|id| id_to_key.get(id))
                    .cloned()
                    .unwrap_or_else(|| "?".to_string());
                ObtainedItem {
                    key,
                    score: c.score,
                }
            })
            .collect();

        let obtained_keys: Vec<String> = obtained.iter().map(|o| o.key.clone()).collect();

        // Metrics.
        let hit_at_2 = if case.relevant.is_empty() {
            false
        } else {
            obtained_keys
                .iter()
                .take(2)
                .any(|k| case.relevant.contains(k))
        };
        let hit_at_4 = if case.relevant.is_empty() {
            false
        } else {
            obtained_keys
                .iter()
                .take(4)
                .any(|k| case.relevant.contains(k))
        };

        let mrr_val = kimetsu_brain::eval::mrr(&obtained_keys, &case.relevant);

        // P0.1: correctness metrics.
        let stale_hit = kimetsu_brain::eval::stale_hit_rate(&obtained_keys, &case.stale, args.cap);
        let resolution_ok =
            kimetsu_brain::eval::resolution_correct(&obtained_keys, &case.relevant, &case.stale);

        // v1.5 (Story 2.1): token estimates — raw vs compressed — for the
        // rendered capsule set. Computed per-case, averaged in the summary.
        let (raw_tokens_mean, rendered_tokens_mean) = {
            use kimetsu_brain::context::{compress_for_render, estimate_tokens};
            let n = bundle.capsules.len();
            if n == 0 {
                (0.0, 0.0)
            } else {
                let raw: u32 = bundle
                    .capsules
                    .iter()
                    .map(|c| estimate_tokens(&c.summary))
                    .sum();
                let rendered: u32 = bundle
                    .capsules
                    .iter()
                    .map(|c| estimate_tokens(&compress_for_render(&c.summary, 3)))
                    .sum();
                (raw as f64 / n as f64, rendered as f64 / n as f64)
            }
        };

        case_results.push(CaseResult {
            query: case.query.clone(),
            expected: case.relevant.clone(),
            obtained,
            hit_at_2,
            hit_at_4,
            mrr: mrr_val,
            latency_ms,
            raw_tokens_mean,
            rendered_tokens_mean,
            stale_hit,
            resolution_correct: resolution_ok,
        });
    }

    // ── 6. Aggregate metrics ──────────────────────────────────────────────────
    let signal_cases: Vec<_> = fixture
        .cases
        .iter()
        .zip(&case_results)
        .filter(|(c, _)| !c.relevant.is_empty())
        .collect();
    let noise_cases: Vec<_> = fixture
        .cases
        .iter()
        .zip(&case_results)
        .filter(|(c, _)| c.relevant.is_empty())
        .collect();

    let recall_at_2 = if signal_cases.is_empty() {
        0.0
    } else {
        signal_cases
            .iter()
            .map(|(_, r)| if r.hit_at_2 { 1.0f64 } else { 0.0 })
            .sum::<f64>()
            / signal_cases.len() as f64
    };
    let recall_at_4 = if signal_cases.is_empty() {
        0.0
    } else {
        signal_cases
            .iter()
            .map(|(_, r)| if r.hit_at_4 { 1.0f64 } else { 0.0 })
            .sum::<f64>()
            / signal_cases.len() as f64
    };
    let mrr_avg = if signal_cases.is_empty() {
        0.0
    } else {
        signal_cases.iter().map(|(_, r)| r.mrr).sum::<f64>() / signal_cases.len() as f64
    };
    let mean_latency_ms = if latencies_ms.is_empty() {
        0.0
    } else {
        latencies_ms.iter().sum::<u128>() as f64 / latencies_ms.len() as f64
    };
    let p95_latency_ms = {
        let mut sorted = latencies_ms.clone();
        sorted.sort_unstable();
        if sorted.is_empty() {
            0.0
        } else {
            let idx = ((sorted.len() as f64 * 0.95) as usize).min(sorted.len() - 1);
            sorted[idx] as f64
        }
    };
    let noise_capsules = if noise_cases.is_empty() {
        0.0
    } else {
        noise_cases
            .iter()
            .map(|(_, r)| r.obtained.len() as f64)
            .sum::<f64>()
            / noise_cases.len() as f64
    };

    let peak = peak_rss_mb();

    // P0.1: correctness aggregates.
    // stale_hit_rate: mean over ALL cases (cases with no stale keys contribute 0).
    let agg_stale_hit_rate = if case_results.is_empty() {
        0.0
    } else {
        case_results.iter().map(|r| r.stale_hit).sum::<f64>() / case_results.len() as f64
    };

    // resolution_accuracy: mean over cases that ARE correctness cases
    // (knowledge_update, contradiction, temporal, multi_session — i.e. have stale keys).
    let correctness_cases: Vec<_> = fixture
        .cases
        .iter()
        .zip(&case_results)
        .filter(|(c, _)| !c.stale.is_empty())
        .collect();
    let resolution_accuracy = if correctness_cases.is_empty() {
        // No correctness cases → N/A, report as -1.0 sentinel (JSON null-ish).
        -1.0_f64
    } else {
        correctness_cases
            .iter()
            .map(|(_, r)| if r.resolution_correct { 1.0f64 } else { 0.0 })
            .sum::<f64>()
            / correctness_cases.len() as f64
    };

    // v1.5 (Story 2.1): aggregate rendered-token means across all cases.
    let (agg_raw_tokens_mean, agg_rendered_tokens_mean) = {
        let n = case_results.len();
        if n == 0 {
            (0.0, 0.0)
        } else {
            let raw_sum: f64 = case_results.iter().map(|r| r.raw_tokens_mean).sum();
            let rend_sum: f64 = case_results.iter().map(|r| r.rendered_tokens_mean).sum();
            (raw_sum / n as f64, rend_sum / n as f64)
        }
    };

    // ── 7. Write combo JSON ───────────────────────────────────────────────────
    let combo_json = serde_json::json!({
        "embedder": embedder_id,
        "reranker": reranker_id,
        "embedder_load_ms": embedder_load_ms,
        "reranker_load_ms": reranker_load_ms,
        "rss_before_embedder_mb": rss_before_emb,
        "rss_after_embedder_mb": rss_after_emb,
        "rss_before_reranker_mb": rss_before_rr,
        "rss_after_reranker_mb": rss_after_rr,
        "peak_rss_mb": peak,
        "seed_ms": seed_ms,
        "cases": case_results,
        "summary": {
            "recall_at_2": recall_at_2,
            "recall_at_4": recall_at_4,
            "mrr": mrr_avg,
            "mean_latency_ms": mean_latency_ms,
            "p95_latency_ms": p95_latency_ms,
            "noise_capsules": noise_capsules,
            // v1.5 (Story 2.1): token-budget intelligence
            "raw_tokens_mean": agg_raw_tokens_mean,
            "rendered_tokens_mean": agg_rendered_tokens_mean,
            // P0.1: correctness metrics
            "stale_hit_rate": agg_stale_hit_rate,
            // -1.0 = no correctness cases in this fixture (N/A)
            "resolution_accuracy": resolution_accuracy,
        }
    });

    std::fs::create_dir_all(&args.out)?;
    let fname = format!("combo-{safe_emb}-{safe_rr}.json");
    let fpath = args.out.join(&fname);
    std::fs::write(&fpath, serde_json::to_string_pretty(&combo_json)?)?;

    // ── 8. Cleanup ────────────────────────────────────────────────────────────
    let _ = std::fs::remove_dir_all(&tmp_root);

    Ok(())
}

pub(crate) fn bench(command: BenchCommand) -> KimetsuResult<()> {
    match command {
        BenchCommand::Swe(args) => {
            let results = run_swe_bench(SweBenchOptions {
                tasks: args.tasks,
                repo: args.repo,
                instance_id: args.instance_id,
                dry_run: args.dry_run,
                disable_broker: args.no_broker,
                limit: args.limit,
            })?;
            println!("instances: {}", results.len());
            for instance in results {
                println!(
                    "{} run={} dry_run={} no_broker={} trace={}",
                    instance.instance_id,
                    instance.run_id,
                    instance.dry_run,
                    instance.disable_broker,
                    instance.trace_path.display(),
                );
            }
            Ok(())
        }
        BenchCommand::Run(args) => {
            let result = run_benchmark(BenchOptions {
                repo: args.repo,
                keep_fixtures: args.keep_fixtures,
                model_backed: args.model_backed,
                limit: args.limit,
                max_cost_usd: args.max_cost_usd,
            })?;
            println!("bench_run_id: {}", result.bench_run_id);
            println!("tasks: {}", result.task_count);
            println!("model_backed: {}", result.model_backed);
            println!("total_cost_usd: {:.4}", result.total_cost_usd);
            println!("report: {}", result.report_path.display());
            println!("results: {}", result.results_path.display());
            for summary in result.summaries {
                println!(
                    "{} success={:.0}% relevant_signal={:.0}% memories={} context_loads={} irrelevant_context={} dry_runs={} avg_ms={:.2} cost_usd={:.4} plan_quality={:.2} invalid_planned={} trace_events={} model_turns={} model_skips={}",
                    summary.mode,
                    summary.success_rate * 100.0,
                    summary.relevant_signal_rate * 100.0,
                    summary.accepted_memories_used,
                    summary.context_loads,
                    summary.irrelevant_context_loaded,
                    summary.dry_runs,
                    summary.avg_duration_ms,
                    summary.total_cost_usd,
                    summary.avg_patch_plan_quality,
                    summary.invalid_planned_files,
                    summary.trace_events,
                    summary.model_turns,
                    summary.model_skips,
                );
            }
            Ok(())
        }
    }
}
