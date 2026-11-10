#!/usr/bin/env bash
# E0 runner (#68 baseline after #67). Prepared by fuse; spec by gauge.
# Usage: run_e0.sh [--dry-run]
# Exit: 0 both runs complete (read results/determinism-diff.md for the verdict),
#       2 preflight failed, 3 build failed, 4 eval failed, 5 already running.
set -euo pipefail

DRY=0; [[ "${1:-}" == "--dry-run" ]] && DRY=1
E0=/workspace/fuse/e0
WT=$E0/kimetsu
SHA=19b178741c95083d77b520d64fbd0496fdf1aa0a          # origin/main at prep time (2026-11-10)
WAIT_PIDS=(407151 1430040)                            # llama-server :18083 / :18082 bench runs
POLL_S=300
MIN_AVAIL_KIB=$((6 * 1024 * 1024))                    # 6 GiB MemAvailable
RAM_WAIT_MAX_S=${RAM_WAIT_MAX_S:-14400}               # give up after 4 h of low RAM
PY=/usr/bin/python3
BENCH_E=/workspace/kimetsu-memory-bench/embeddings    # read-only source of toolchain bits
TC=$E0/toolchain
TARGET=$E0/target
BIN=$E0/bin/kimetsu-e0
CACHE=$E0/model-cache
RES=$E0/results
LOGS=$E0/logs
DATASETS=(locomo-conv26 lme-oracle)
RUNS=(run1 run2)

ts() { date '+%Y-%m-%d %H:%M:%S %Z'; }
log() { echo "[$(ts)] $*"; }
show() { printf '  $'; printf ' %q' "$@"; printf '\n'; }
run() { if (( DRY )); then show "$@"; else "$@"; fi; }
die() { log "FAIL: $2"; exit "$1"; }

build_cmd=(nice -n 10 /home/box/.cargo/bin/cargo rustc -p kimetsu-cli --bin kimetsu --release --locked
           --features embeddings -j 4 --
           -C link-arg=-L$TC/lib -C link-arg=-lc++ -C link-arg=-lc++abi -C link-arg=-lunwind)
build_env=(env PATH=/home/box/.cargo/bin:$TC/bin:/usr/local/bin:/usr/bin:/bin CXX=$TC/zig-cxx
           OPENSSL_INCLUDE_DIR=/usr/include OPENSSL_LIB_DIR=/usr/lib/x86_64-linux-gnu LIBRARY_PATH=$TC/lib
           CARGO_BUILD_JOBS=4 CARGO_TARGET_DIR=$TARGET)
eval_env=(env PYTHONHASHSEED=0 KIMETSU_INTRA_THREADS=1 OMP_NUM_THREADS=1 OPENBLAS_NUM_THREADS=1
          FASTEMBED_CACHE_DIR=$CACHE HF_HOME=$CACHE KIMETSU_USER_BRAIN=0 KIMETSU_BRAIN_AMBIENT=off
          KIMETSU_BRAIN_LOG_RETRIEVAL=0 PATH=/usr/local/bin:/usr/bin:/bin)
eval_cmd() { # run dataset
  echo nice -n 10 "$PY" -u "$E0/e0_eval.py" --bin "$BIN" --profile bench --name "$2" \
       --dataset "$E0/data/$2.json" --out "$RES/$1/$2.jsonl"
}

alive() { # pid still the llama-server we were told about (kill -0 only checks; sends no signal)
  kill -0 "$1" 2>/dev/null && tr '\0' ' ' < "/proc/$1/cmdline" 2>/dev/null | grep -q llama-server
}
avail_kib() { awk '/^MemAvailable:/{print $2}' /proc/meminfo; }
wait_ram() {
  local waited=0
  while (( $(avail_kib) < MIN_AVAIL_KIB )); do
    (( waited >= RAM_WAIT_MAX_S )) && die 2 "MemAvailable $(avail_kib) KiB < 6 GiB after ${waited}s"
    log "MemAvailable $(( $(avail_kib) / 1024 )) MiB < 6144 MiB; rechecking in ${POLL_S}s"
    sleep "$POLL_S"; waited=$((waited + POLL_S))
  done
  log "MemAvailable $(( $(avail_kib) / 1024 )) MiB >= 6144 MiB"
}

# no inherited KIMETSU_* knobs may leak into children (child_env sets the ones E0 needs)
while read -r v; do unset "$v"; done < <(compgen -e | grep '^KIMETSU_' || true)

# ---------- preflight (read-only; also what --dry-run checks) ----------
log "E0 runner, dry_run=$DRY"
[[ "$(git -C "$WT" rev-parse HEAD)" == "$SHA" ]] || die 2 "worktree HEAD != $SHA"
[[ -z "$(git -C "$WT" status --porcelain)" ]] || die 2 "worktree has local changes"
for f in "$WT/scripts/retrieval_eval.py" "$E0/e0_eval.py" "$E0/prepare_data.py" "$E0/summarize_e0.py" \
         "$E0/data/locomo-conv26.json" "$E0/data/lme-oracle.json" "$E0/data/labels-report.json" \
         "$BENCH_E/zig-cxx" "$BENCH_E/lib/libc++.a" /home/box/.local/zig/env/bin/python /workspace/fuse/model-cache; do
  [[ -e "$f" ]] || die 2 "missing $f"
done
"$PY" -m py_compile "$E0/e0_eval.py" "$E0/prepare_data.py" "$E0/summarize_e0.py" "$WT/scripts/retrieval_eval.py"
"$PY" "$WT/scripts/retrieval_eval.py" --self-test >/dev/null || die 2 "retrieval_eval --self-test failed"
for ds in "${DATASETS[@]}"; do
  "$PY" "$E0/e0_eval.py" --bin /nonexistent --dataset "$E0/data/$ds.json" --name "$ds" --out /dev/null --dry-run \
    | "$PY" -c 'import json,sys;d=json.load(sys.stdin);print("  data %s: %d questions, %d scored, arms=%s" % (d["name"], d["questions"], d["scored"], d["arms"]))'
done
for p in "${WAIT_PIDS[@]}"; do
  if alive "$p"; then log "pid $p alive: $(tr '\0' ' ' < /proc/$p/cmdline | cut -c1-90)"; else log "pid $p gone"; fi
done
log "MemAvailable now $(( $(avail_kib) / 1024 )) MiB (need 6144)"

if (( DRY )); then
  echo; log "DRY RUN: commands that a real run executes, in order"
  echo "  # (a) wait, polling every ${POLL_S}s with kill -0 (no signal sent):"
  for p in "${WAIT_PIDS[@]}"; do echo "  while kill -0 $p && /proc/$p/cmdline~llama-server; do sleep $POLL_S; done"; done
  echo "  # (b) MemAvailable >= 6 GiB (re-polled every ${POLL_S}s, give up after ${RAM_WAIT_MAX_S}s)"
  echo "  # (c) toolchain + model cache copies, then release build:"
  show mkdir -p "$TC/bin" "$E0/bin" "$LOGS"
  show cp -n "$BENCH_E/zig-cxx" "$TC/zig-cxx"; show cp -rn "$BENCH_E/lib" "$TC/lib"
  show cp -n /home/box/.local/bin/c++ /home/box/.local/bin/g++ "$TC/bin/"
  show cp -rn /workspace/fuse/model-cache "$CACHE"
  echo "  cd $WT && \\"; show "${build_env[@]}" "${build_cmd[@]}"; echo "    > $LOGS/build.log 2>&1"
  show cp "$TARGET/release/kimetsu" "$BIN"; show "$BIN" --version
  echo "  # (d) per run: RAM check, then each dataset (4 binary arms + script vector arm per question):"
  for r in "${RUNS[@]}"; do for ds in "${DATASETS[@]}"; do
    echo "  TMPDIR=$E0/tmp/$r \\"; show "${eval_env[@]}" $(eval_cmd "$r" "$ds"); echo "    > $RES/$r/$ds.stdout.log 2>&1"
  done; show "$PY" "$E0/summarize_e0.py" summary "$RES/$r"; done
  echo "  # (e) determinism + manifest:"
  show "$PY" "$E0/summarize_e0.py" diff "$RES/run1" "$RES/run2" "$RES/determinism-diff.md"
  echo "  write $RES/resolved-config-per-arm.json, $RES/MANIFEST.md"
  exit 0
fi

exec 9>"$E0/run_e0.lock"; flock -n 9 || die 5 "another run_e0.sh holds $E0/run_e0.lock"
mkdir -p "$LOGS" "$RES"
START=$(ts)

# ---------- (a) wait for the two bench llama-servers ----------
for p in "${WAIT_PIDS[@]}"; do
  while alive "$p"; do log "waiting: pid $p still running; next check in ${POLL_S}s"; sleep "$POLL_S"; done
  log "pid $p has exited"
done

# ---------- (b) RAM ----------
wait_ram
( while :; do echo "$(date +%s) $(avail_kib)"; sleep 30; done ) > "$LOGS/memavailable.log" & MEMLOG=$!
trap 'kill $MEMLOG 2>/dev/null || true' EXIT

# ---------- (c) toolchain, model cache, release build ----------
mkdir -p "$TC/bin" "$E0/bin"
[[ -e $TC/zig-cxx ]] || cp "$BENCH_E/zig-cxx" "$TC/zig-cxx"
[[ -d $TC/lib ]] || cp -r "$BENCH_E/lib" "$TC/lib"
for c in c++ g++; do [[ -e $TC/bin/$c ]] || cp /home/box/.local/bin/$c "$TC/bin/$c"; done
[[ -d $CACHE ]] || cp -r /workspace/fuse/model-cache "$CACHE"
(cd "$CACHE" && find . -name '*.onnx' -o -name 'tokenizer.json' | sort | xargs -r sha256sum) > "$LOGS/model-sha256.before"
log "building release at $SHA (nice 10, -j4) -> $LOGS/build.log"
( cd "$WT" && "${build_env[@]}" "${build_cmd[@]}" ) > "$LOGS/build.log" 2>&1 || die 3 "cargo build failed; tail $LOGS/build.log"
cp "$TARGET/release/kimetsu" "$BIN"
VERSION=$("$BIN" --version); BIN_SHA=$(sha256sum "$BIN" | cut -d' ' -f1)
log "built $VERSION sha256=$BIN_SHA"
grep -q embeddings <<<"$VERSION" || die 3 "binary is not an embeddings build: $VERSION"
[[ -z "$(git -C "$WT" status --porcelain)" ]] || die 3 "worktree dirty after build"

# ---------- (d) two full passes ----------
for r in "${RUNS[@]}"; do
  mkdir -p "$RES/$r" "$E0/tmp/$r"
  for ds in "${DATASETS[@]}"; do
    meta=$RES/$r/$ds.meta.json
    if [[ -f $meta ]] && "$PY" -c "import json,sys;sys.exit(json.load(open('$meta'))['exit'])"; then
      log "$r/$ds already complete, skipping"; continue
    fi
    wait_ram
    log "$r/$ds start"
    TMPDIR=$E0/tmp/$r "${eval_env[@]}" $(eval_cmd "$r" "$ds") > "$RES/$r/$ds.stdout.log" 2>&1 \
      || die 4 "$r/$ds failed; see $RES/$r/$ds.stdout.log"
    log "$r/$ds done: $(tail -1 "$RES/$r/$ds.stdout.log")"
  done
  "$PY" "$E0/summarize_e0.py" summary "$RES/$r" > /dev/null
done
(cd "$CACHE" && find . -name '*.onnx' -o -name 'tokenizer.json' | sort | xargs -r sha256sum) > "$LOGS/model-sha256.after"
cmp -s "$LOGS/model-sha256.before" "$LOGS/model-sha256.after" || log "WARNING: model files changed during the run"

# ---------- (e) determinism, resolved config, manifest ----------
set +e; "$PY" "$E0/summarize_e0.py" diff "$RES/run1" "$RES/run2" "$RES/determinism-diff.md"; DET=$?; set -e
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
               'identical_run1_run2': strip(a) == strip(b),
               'full_dumps': [str(res / r / f'resolved-config-{ds}.json') for r in ('run1', 'run2')]}
(res / 'resolved-config-per-arm.json').write_text(json.dumps(out, indent=2))
PYEOF
END=$(ts)
{
  echo "# E0 manifest"; echo
  echo "- spec: gauge, ML room, exp 0 (#68 baseline after #67). runner: $E0/run_e0.sh"
  echo "- started: $START; finished: $END"
  echo "- code: RodCor/kimetsu main @ $SHA (worktree $WT, clean before and after build)"
  echo "- binary: $BIN, $VERSION, sha256 $BIN_SHA; build log $LOGS/build.log"
  echo "- build: (cd $WT && ${build_env[*]} ${build_cmd[*]})"
  echo "- datasets (adapter $E0/prepare_data.py, counts $E0/data/labels-report.json):"
  echo "  - locomo-conv26: $E0/data/locomo-conv26.json (copy of full-locomo/full-dataset.json conv-26 + locomo10.json)"
  echo "  - lme-oracle: $E0/data/lme-oracle.json (copy of data/longmemeval_oracle.json)"
  echo "- scorer: $WT/scripts/retrieval_eval.py (PR #67) via $E0/e0_eval.py --profile bench; retrieval only, no reader, NOT an official judge"
  echo "- seed: none (retrieval and embedding are deterministic; no component takes a seed). PYTHONHASHSEED=0"
  echo "- threads: KIMETSU_INTRA_THREADS=1 OMP_NUM_THREADS=1 (one ONNX thread)"
  echo "- model cache: $CACHE (sha256 $LOGS/model-sha256.before / .after)"
  echo "- eval env: ${eval_env[*]}"
  echo "- eval commands:"
  for r in "${RUNS[@]}"; do for ds in "${DATASETS[@]}"; do echo "  - TMPDIR=$E0/tmp/$r $(eval_cmd "$r" "$ds")"; done; done
  echo "- results: $RES/run1, $RES/run2 (JSONL, *.meta.json, *.stdout.log, summary.md/json), $RES/determinism-diff.md (exit $DET), $RES/resolved-config-per-arm.json"
  echo "- MemAvailable trace: $LOGS/memavailable.log"
  echo; echo "## run1 summary"; echo; cat "$RES/run1/summary.md"
} > "$RES/MANIFEST.md"
log "E0 complete. determinism diff exit=$DET ($RES/determinism-diff.md). manifest $RES/MANIFEST.md"
exit 0
