#!/usr/bin/env bash
# E0+sweep run2 rerun (#68), gauge ML room 2026-11-20. Derived from run_e0s.sh (sha256 4b4155f1):
# same eval env/commands/data/binary; run1 kept as-is; no rebuild (reuses bin/kimetsu-e0s, sha checked).
# Original run2 died at conv-26 q198 (box reboot); its partial output is in results/run2.died-20261117.
# Based on /workspace/fuse/e0/run_e0.sh, WITHOUT the wait-for-llama-server gate
# (free RAM is logged only). Runs under memcap.py (5 GiB tree RSS cap) at nice 19.
# Exit: 0 done (see results/determinism-diff.md), 2 preflight, 3 build, 4 eval, 5 already running.
set -euo pipefail

S=/workspace/fuse/e0s
WT=$S/kimetsu
MAIN_SHA=19112dae937fb3b1842983943a0def6fad75f4b1     # origin/main, fetched 2026-11-13 13:13 -03
SHA=61dc9f01a60999d18d8614e0337272f893643fa0          # cursor/e0-sweep-profile = main + bench-sweep profile (scripts only)
PROFILE=bench-sweep
PY=/usr/bin/python3
BENCH_E=/workspace/kimetsu-memory-bench/embeddings    # read-only source of toolchain bits (copied, never written)
DATA=/workspace/fuse/e0/data                          # prepared E0 data, reused
TC=$S/toolchain; TARGET=$S/target; BIN=$S/bin/kimetsu-e0s
CACHE=$S/model-cache; RES=$S/results; LOGS=$S/logs
DATASETS=(locomo-conv26 lme-oracle); RUNS=(run1 run2)

ts() { date '+%Y-%m-%d %H:%M:%S %Z'; }
log() { echo "[$(ts)] $*"; }
die() { log "FAIL: $2"; exit "$1"; }
avail_mib() { awk '/^MemAvailable:/{print int($2/1024)}' /proc/meminfo; }

build_cmd=(nice -n 19 /home/box/.cargo/bin/cargo rustc -p kimetsu-cli --bin kimetsu --release --locked
           --features embeddings -j 2 --
           -C link-arg=-L$TC/lib -C link-arg=-lc++ -C link-arg=-lc++abi -C link-arg=-lunwind)
build_env=(env PATH=/home/box/.cargo/bin:$TC/bin:/usr/local/bin:/usr/bin:/bin CXX=$TC/zig-cxx
           OPENSSL_INCLUDE_DIR=/usr/include OPENSSL_LIB_DIR=/usr/lib/x86_64-linux-gnu LIBRARY_PATH=$TC/lib
           CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=$TARGET)
eval_env=(env PYTHONHASHSEED=0 KIMETSU_INTRA_THREADS=1 OMP_NUM_THREADS=1 OPENBLAS_NUM_THREADS=1
          FASTEMBED_CACHE_DIR=$CACHE HF_HOME=$CACHE KIMETSU_USER_BRAIN=0 KIMETSU_BRAIN_AMBIENT=off
          KIMETSU_BRAIN_LOG_RETRIEVAL=0 PATH=/usr/local/bin:/usr/bin:/bin)
eval_cmd() { # run dataset
  echo nice -n 19 "$PY" -u "$WT/scripts/e0_eval.py" --bin "$BIN" --profile "$PROFILE" --name "$2" \
       --dataset "$DATA/$2.json" --out "$RES/$1/$2.jsonl"
}

while read -r v; do unset "$v"; done < <(compgen -e | grep '^KIMETSU_' || true)

exec 9>"$S/run_e0s.lock"; flock -n 9 || die 5 "another run_e0s.sh holds $S/run_e0s.lock"
log "E0+sweep runner pid $$, profile $PROFILE, nice $(nice)"
HEAD_SHA=$(git -C "$WT" rev-parse HEAD)
git -C "$WT" diff --quiet "$SHA" HEAD -- . ":!scripts/experiments/e0s" || die 2 "worktree $HEAD_SHA differs from $SHA outside scripts/experiments/e0s"
(cd "$S" && sha256sum -c --quiet <(grep -v "^#" wrapper-sha256.txt)) || die 2 "pinned hashes in wrapper-sha256.txt do not match"
[[ -z "$(git -C "$WT" status --porcelain)" ]] || die 2 "worktree has local changes"
git -C "$WT" diff --quiet "$MAIN_SHA" "$SHA" -- crates Cargo.toml Cargo.lock || die 2 "branch changes Rust vs main"
for f in "$DATA/locomo-conv26.json" "$DATA/lme-oracle.json" "$CACHE" "$BIN"; do   # build toolchain not needed (no rebuild)
  [[ -e "$f" ]] || die 2 "missing $f"
done
"$PY" "$WT/scripts/e0_eval.py" --self-test >/dev/null || die 2 "e0_eval --self-test failed"
"$PY" "$WT/scripts/retrieval_eval.py" --self-test >/dev/null || die 2 "retrieval_eval --self-test failed"
for ds in "${DATASETS[@]}"; do
  "$PY" "$WT/scripts/e0_eval.py" --bin /nonexistent --profile "$PROFILE" --dataset "$DATA/$ds.json" --name "$ds" --out /dev/null --dry-run \
    | "$PY" -c 'import json,sys;d=json.load(sys.stdin);print("  data %s: %d questions, %d scored, arms=%s" % (d["name"], d["questions"], d["scored"], d["arms"]))'
done
for p in 407151 1430040; do
  if kill -0 "$p" 2>/dev/null; then log "info: llama-server pid $p alive (not waited on, not touched)"; fi
done
log "MemAvailable $(avail_mib) MiB (logged only; cap is the 5 GiB memcap watchdog)"
mkdir -p "$LOGS" "$RES"; START=$(ts)
( while :; do echo "$(date +%s) $(awk '/^MemAvailable:/{print $2}' /proc/meminfo)"; sleep 30; done ) > "$LOGS/memavailable.rerun2.log" & MEMLOG=$!
trap 'kill $MEMLOG 2>/dev/null || true' EXIT

EXPECT_BIN_SHA=262f17def1cfd67f07aaec325a2f2d23aa058d393adefc463d4bfc4c33391985   # run1 binary (run_e0s.log 2026-11-13 13:22)
VERSION=$("$BIN" --version); BIN_SHA=$(sha256sum "$BIN" | cut -d' ' -f1)
[[ "$BIN_SHA" == "$EXPECT_BIN_SHA" ]] || die 3 "binary sha $BIN_SHA != run1 $EXPECT_BIN_SHA"
log "reusing run1 binary $VERSION sha256=$BIN_SHA (no rebuild)"
(cd "$CACHE" && find . -name '*.onnx' -o -name 'tokenizer.json' | sort | xargs -r sha256sum) > "$LOGS/model-sha256.rerun2.before"
cmp -s "$LOGS/model-sha256.before" "$LOGS/model-sha256.rerun2.before" || die 2 "model files differ from run1's model-sha256.before"
for r in run2; do
  mkdir -p "$RES/$r" "$S/tmp/$r"
  for ds in "${DATASETS[@]}"; do
    meta=$RES/$r/$ds.meta.json
    if [[ -f $meta ]] && "$PY" -c "import json,sys;sys.exit(json.load(open('$meta'))['exit'])"; then
      log "$r/$ds already complete, skipping"; continue
    fi
    log "$r/$ds start (MemAvailable $(avail_mib) MiB)"
    TMPDIR=$S/tmp/$r "${eval_env[@]}" $(eval_cmd "$r" "$ds") > "$RES/$r/$ds.stdout.log" 2>&1 \
      || die 4 "$r/$ds failed; see $RES/$r/$ds.stdout.log"
    log "$r/$ds done: $(tail -1 "$RES/$r/$ds.stdout.log")"
  done
  "$PY" "$WT/scripts/experiments/e0/summarize_e0.py" summary "$RES/$r" > /dev/null
  "$PY" "$S/sweep_hist.py" "$RES/$r"
done
(cd "$CACHE" && find . -name '*.onnx' -o -name 'tokenizer.json' | sort | xargs -r sha256sum) > "$LOGS/model-sha256.rerun2.after"
cmp -s "$LOGS/model-sha256.before" "$LOGS/model-sha256.rerun2.after" || log "WARNING: model files changed during the run"

set +e; "$PY" "$WT/scripts/experiments/e0/summarize_e0.py" diff "$RES/run1" "$RES/run2" "$RES/determinism-diff.md"; DET=$?; set -e
"$PY" - "$RES" <<'PYEOF'
import json, sys
from pathlib import Path
res = Path(sys.argv[1]); out = {}
for ds in ('locomo-conv26', 'lme-oracle'):
    a = json.loads((res / 'run1' / f'resolved-config-{ds}.json').read_text())
    b = json.loads((res / 'run2' / f'resolved-config-{ds}.json').read_text())
    strip = lambda d: {arm: {k: v for k, v in x.items() if k != 'env'} for arm, x in d['arms'].items()}
    out[ds] = {'arms': {arm: x['resolved'] | {'knob': x['knob'], 'patch': x['patch']} for arm, x in a['arms'].items()},
               'mcp_args': a['mcp_args'], 'base_patch': a['base_patch'],
               'semantic_floor_note': 'fused_defaults semantic floor is inferred: config -1 -> auto 0.35 for bge (project.rs:389-391)',
               'identical_run1_run2': strip(a) == strip(b),
               'full_dumps': [str(res / r / f'resolved-config-{ds}.json') for r in ('run1', 'run2')]}
(res / 'resolved-config-per-arm.json').write_text(json.dumps(out, indent=2))
PYEOF
END=$(ts)
{
  echo "# E0+sweep manifest (run1 2026-11-13; run2 rerun 2026-11-20 via $S/rerun_run2.sh, worktree HEAD $HEAD_SHA)"; echo
  echo "- spec: gauge, ML room 2026-11-13 (E0+sweep, #68). runner: $S/run_e0s.sh under $S/memcap.py (5 GiB tree RSS cap), nice 19"
  echo "- run2 rerun started: $START; finished: $END. run1 log: $S/run_e0s.log"
  echo "- code: RodCor/kimetsu main @ $MAIN_SHA; worktree $WT @ $SHA (branch cursor/e0-sweep-profile, scripts-only diff, not pushed)"
  echo "- binary: $BIN, $VERSION, sha256 $BIN_SHA; build log $LOGS/build.log"
  echo "- build (run1 only; run2 reused the binary): (cd $WT && ${build_env[*]} ${build_cmd[*]})"
  echo "- data: $DATA/locomo-conv26.json (199 q / 197 scored), $DATA/lme-oracle.json (500 q / 479 scored)"
  echo "- scorer: $WT/scripts/retrieval_eval.py metrics via scripts/e0_eval.py --profile $PROFILE; retrieval only, no reader, NOT an official judge"
  echo "- seed: none (deterministic); PYTHONHASHSEED=0. threads: 1 ONNX thread"
  echo "- latency/RSS: NOT comparable; do not report"
  echo "- rrf: only k=60 (fusion.rs:188 Fusion::Rrf => rrf_fuse(lists, DEFAULT_RRF_K), DEFAULT_RRF_K=60.0 at fusion.rs:64; no config knob)"
  echo "- eval env: ${eval_env[*]}"
  echo "- eval commands:"
  for r in "${RUNS[@]}"; do for ds in "${DATASETS[@]}"; do echo "  - TMPDIR=$S/tmp/$r $(eval_cmd "$r" "$ds")"; done; done
  echo "- results: $RES/run{1,2} (JSONL, meta, stdout, summary.md/json, capsule-hist.md/json), $RES/determinism-diff.md (exit $DET), $RES/resolved-config-per-arm.json"
  echo "- hashes: $S/wrapper-sha256.txt. memcap log: $S/memcap.log. MemAvailable trace: $LOGS/memavailable.log"
  echo; echo "## run1 summary"; echo; cat "$RES/run1/summary.md"
  echo; cat "$RES/run1/capsule-hist.md"
} > "$RES/MANIFEST.md"
log "E0+sweep complete. determinism diff exit=$DET. manifest $RES/MANIFEST.md"
exit 0
