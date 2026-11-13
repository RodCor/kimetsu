#!/usr/bin/env bash
# E0 sweep runner (#68).
#
# The 2026-11-13 run used SHA 61dc9f01a60999d18d8614e0337272f893643fa0
# and /workspace/fuse paths (worktree /workspace/fuse/e0s/kimetsu, data
# /workspace/fuse/e0/data, output /workspace/fuse/e0s, toolchain
# /workspace/kimetsu-memory-bench/embeddings, model cache
# /workspace/fuse/model-cache, cargo /home/box/.cargo/bin/cargo).
# This script takes those from flags or the environment. Defaults are
# repo-relative, and the commit defaults to HEAD.
#
# Usage:
#   scripts/experiments/e0s/run_e0s.sh [--data-dir DIR] [--out DIR] [--bin PATH]
#                                      [--sha SHA] [--jobs N] [--dry-run]
#
# Environment:
#   E0S_DATA E0S_OUT E0S_BIN E0S_SHA E0S_MAIN_SHA PYTHON CARGO
#   E0S_CXX E0S_TOOLCHAIN_LIB E0S_MODEL_CACHE
#
# The 5 GiB cap is external:
#   python3 scripts/experiments/e0s/memcap.py --limit-mib 5120 --interval 2 \
#     --log memcap.log -- scripts/experiments/e0s/run_e0s.sh --data-dir DIR
#
# Exit: 0 deterministic, 1 determinism diff failed, 2 preflight, 3 build,
#       4 eval, 5 already running.
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
SCRIPT_PATH=$SCRIPT_DIR/$(basename "${BASH_SOURCE[0]}")
REPO=$(cd "$SCRIPT_DIR/../../.." && pwd)

if [[ ${1:-} == --self-test ]]; then
  exec env -u E0S_DATA -u E0S_OUT -u E0S_BIN -u E0S_SHA -u E0S_MAIN_SHA \
    -u E0S_TOOLCHAIN_LIB -u E0S_CXX -u E0S_MODEL_CACHE \
    bash "$SCRIPT_PATH" --self-test-defaults
fi

PROFILE=bench-sweep
DATASETS=(locomo-conv26 lme-oracle)
RUNS=(run1 run2)
RECORDED_MAIN=19112dae937fb3b1842983943a0def6fad75f4b1

usage() { sed -n '2,25p' "$SCRIPT_PATH"; }
die() { echo "[$(date '+%Y-%m-%d %H:%M:%S %Z')] FAIL: $2" >&2; exit "$1"; }
ts() { date '+%Y-%m-%d %H:%M:%S %Z'; }
log() { echo "[$(ts)] $*"; }
show_config() {
  printf 'repo=%s\nsha=%s\nmain=%s\ndata=%s\nout=%s\nbin=%s\nprofile=%s\n' \
    "$REPO" "$SHA" "$MAIN_SHA" "$DATA" "$OUT" "$BIN" "$PROFILE"
}

DRY=0
SHOW=0
SELF_TEST=0
DATA=${E0S_DATA:-}
OUT=${E0S_OUT:-}
BIN_ARG=${E0S_BIN:-}
SHA_ARG=${E0S_SHA:-}
JOBS=2

while [[ $# -gt 0 ]]; do
  case "$1" in
    --dry-run) DRY=1; shift ;;
    --show-config) SHOW=1; shift ;;
    --self-test-defaults) SELF_TEST=1; shift ;;
    --data-dir)
      [[ $# -ge 2 ]] || die 2 "--data-dir needs a directory"
      DATA=$2; shift 2 ;;
    --out)
      [[ $# -ge 2 ]] || die 2 "--out needs a directory"
      OUT=$2; shift 2 ;;
    --bin)
      [[ $# -ge 2 ]] || die 2 "--bin needs a path"
      BIN_ARG=$2; shift 2 ;;
    --sha)
      [[ $# -ge 2 ]] || die 2 "--sha needs a commit"
      SHA_ARG=$2; shift 2 ;;
    --jobs)
      [[ $# -ge 2 ]] || die 2 "--jobs needs a number"
      JOBS=$2; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) die 2 "unknown argument: $1" ;;
  esac
done

OUT=${OUT:-$REPO/e0s-results}
DATA=${DATA:-$REPO/e0-data}
PY=${PYTHON:-python3}
CARGO=${CARGO:-cargo}
SHA=${SHA_ARG:-$(git -C "$REPO" rev-parse HEAD)}
MAIN_SHA=${E0S_MAIN_SHA:-$RECORDED_MAIN}
if [[ -n $BIN_ARG ]]; then
  BIN=$BIN_ARG
else
  BIN=$OUT/bin/kimetsu
fi
LOGS=$OUT/logs
CACHE=$OUT/model-cache
TARGET=$OUT/target

if (( SELF_TEST )); then
  head=$(git -C "$REPO" rev-parse HEAD)
  [[ $SHA == "$head" ]] || die 2 "default SHA is $SHA, HEAD is $head"
  [[ $OUT == "$REPO/e0s-results" ]] || die 2 "default out is $OUT"
  [[ $DATA == "$REPO/e0-data" ]] || die 2 "default data is $DATA"
  [[ $BIN == "$OUT/bin/kimetsu" ]] || die 2 "default bin is $BIN"
  [[ -f $REPO/Cargo.toml && -f $REPO/scripts/e0_eval.py ]] || die 2 "repo root $REPO is not a checkout"
  [[ $MAIN_SHA == "$RECORDED_MAIN" ]] || die 2 "default main sha is $MAIN_SHA"
  # Split so this check does not match its own source line.
  needle_ws=$(printf '%s%s' /work space)
  needle_home=$(printf '%s%s%s' /home / box)
  for f in "$SCRIPT_PATH" "$SCRIPT_DIR/memcap.py" "$SCRIPT_DIR/sweep_hist.py"; do
    if grep -vE '^[[:space:]]*#' "$f" | grep -q "$needle_ws\|$needle_home"; then
      die 2 "$f has a hardcoded box path outside a comment"
    fi
  done
  cfg=$(bash "$SCRIPT_PATH" --show-config --sha deadbeef --out /tmp/e0s-out --data-dir /tmp/e0s-data --bin /tmp/kimetsu)
  [[ $cfg == *sha=deadbeef* ]] || die 2 "sha flag ignored: $cfg"
  [[ $cfg == *out=/tmp/e0s-out* ]] || die 2 "out flag ignored: $cfg"
  [[ $cfg == *data=/tmp/e0s-data* ]] || die 2 "data flag ignored: $cfg"
  [[ $cfg == *bin=/tmp/kimetsu* ]] || die 2 "bin flag ignored: $cfg"
  cfg=$(E0S_SHA=abc E0S_OUT=/tmp/from-env E0S_DATA=/tmp/data-env E0S_BIN=/tmp/bin-env bash "$SCRIPT_PATH" --show-config)
  [[ $cfg == *sha=abc* && $cfg == *out=/tmp/from-env* && $cfg == *data=/tmp/data-env* && $cfg == *bin=/tmp/bin-env* ]] \
    || die 2 "env overrides ignored: $cfg"
  counts=$(mktemp -d)
  cp "$REPO/scripts/fixtures/retrieval_eval.json" "$counts/locomo-conv26.json"
  cp "$REPO/scripts/fixtures/retrieval_eval.json" "$counts/lme-oracle.json"
  dry=$(bash "$SCRIPT_PATH" --dry-run --data-dir "$counts" --out "$counts/out") || die 2 "dry-run failed"
  rm -rf "$counts"
  [[ $dry == *"data locomo-conv26: 3 questions, 3 scored"* ]] \
    || die 2 "dry-run did not report locomo counts: $dry"
  [[ $dry == *"data lme-oracle: 3 questions, 3 scored"* ]] \
    || die 2 "dry-run did not report lme counts: $dry"
  echo "run_e0s self-test ok"
  exit 0
fi

if (( SHOW )); then
  show_config
  exit 0
fi

log "E0 sweep runner pid $$, profile $PROFILE, sha $SHA, repo $REPO"
log "data $DATA out $OUT bin $BIN"

[[ -f $REPO/Cargo.toml ]] || die 2 "missing $REPO/Cargo.toml"
[[ -f $REPO/scripts/e0_eval.py ]] || die 2 "missing scripts/e0_eval.py"
"$PY" "$REPO/scripts/e0_eval.py" --self-test >/dev/null || die 2 "e0_eval --self-test failed"
"$PY" "$REPO/scripts/retrieval_eval.py" --self-test >/dev/null || die 2 "retrieval_eval --self-test failed"

COUNT_TEXT=""
if [[ -d $DATA ]]; then
  for ds in "${DATASETS[@]}"; do
    [[ -f $DATA/$ds.json ]] || die 2 "missing $DATA/$ds.json"
    plan=$("$PY" "$REPO/scripts/e0_eval.py" --bin "$BIN" --profile "$PROFILE" \
      --dataset "$DATA/$ds.json" --name "$ds" --out /dev/null --dry-run) || die 2 "dry-run failed for $ds"
    line=$(printf '%s\n' "$plan" | "$PY" -c 'import json,sys; d=json.load(sys.stdin); print("%s %d %d" % (d["name"], d["questions"], d["scored"]))') \
      || die 2 "could not read dry-run for $ds"
    read -r name q scored <<<"$line"
    COUNT_TEXT+="$DATA/$name.json ($q q / $scored scored), "
    log "data $name: $q questions, $scored scored"
  done
  COUNT_TEXT=${COUNT_TEXT%, }
elif (( DRY == 0 )); then
  die 2 "missing data dir $DATA (pass --data-dir or set E0S_DATA)"
else
  log "no data dir $DATA; skipping dataset dry-run"
fi

build_env=(env CARGO_BUILD_JOBS="$JOBS" CARGO_TARGET_DIR="$TARGET")
if [[ -n ${E0S_CXX:-} ]]; then
  build_env+=(CXX="$E0S_CXX")
fi
if [[ -n ${E0S_TOOLCHAIN_LIB:-} ]]; then
  build_env+=(LIBRARY_PATH="$E0S_TOOLCHAIN_LIB")
  build_cmd=(
    nice -n 19 "$CARGO" rustc -p kimetsu-cli --bin kimetsu --release --locked
    --features embeddings -j "$JOBS" --
    -C "link-arg=-L$E0S_TOOLCHAIN_LIB" -C link-arg=-lc++ -C link-arg=-lc++abi -C link-arg=-lunwind
  )
else
  build_cmd=(
    nice -n 19 "$CARGO" build -p kimetsu-cli --bin kimetsu --release --locked
    --features embeddings -j "$JOBS"
  )
fi
eval_env=(
  env PYTHONHASHSEED=0 KIMETSU_INTRA_THREADS=1 OMP_NUM_THREADS=1 OPENBLAS_NUM_THREADS=1
  FASTEMBED_CACHE_DIR="$CACHE" HF_HOME="$CACHE" KIMETSU_USER_BRAIN=0 KIMETSU_BRAIN_AMBIENT=off
  KIMETSU_BRAIN_LOG_RETRIEVAL=0
)
eval_argv() { # $1 run, $2 dataset
  EVAL_ARGV=(
    nice -n 19 "$PY" -u "$REPO/scripts/e0_eval.py"
    --bin "$BIN" --profile "$PROFILE" --name "$2"
    --dataset "$DATA/$2.json" --out "$OUT/$1/$2.jsonl"
  )
}

if (( DRY )); then
  echo
  log "DRY RUN: build release with embeddings, run both passes, diff"
  if [[ -n $BIN_ARG ]]; then
    echo "  # reuse binary $BIN"
  else
    echo "  cd $REPO && \\"
    printf '  '; printf '%q ' "${build_env[@]}" "${build_cmd[@]}"; printf '> %q 2>&1\n' "$LOGS/build.log"
    echo "  cp $TARGET/release/kimetsu $BIN"
  fi
  for r in "${RUNS[@]}"; do
    for ds in "${DATASETS[@]}"; do
      eval_argv "$r" "$ds"
      echo "  TMPDIR=$OUT/tmp/$r ${eval_env[*]} ${EVAL_ARGV[*]} > $OUT/$r/$ds.stdout.log"
    done
    echo "  $PY $REPO/scripts/experiments/e0/summarize_e0.py summary $OUT/$r"
    echo "  $PY $SCRIPT_DIR/sweep_hist.py $OUT/$r"
  done
  exit 0
fi

command -v "$PY" >/dev/null || die 2 "python not found: $PY"
command -v nice >/dev/null || die 2 "nice not found"
command -v flock >/dev/null || die 2 "flock not found"
command -v sha256sum >/dev/null || die 2 "sha256sum not found"
if [[ -z $BIN_ARG ]]; then
  command -v "$CARGO" >/dev/null || die 2 "cargo not found: $CARGO"
fi
[[ $(git -C "$REPO" rev-parse HEAD) == "$SHA" ]] || die 2 "HEAD != $SHA"
[[ -z $(git -C "$REPO" status --porcelain) ]] || die 2 "worktree has local changes"
git -C "$REPO" diff --quiet "$MAIN_SHA" "$SHA" -- crates Cargo.toml Cargo.lock \
  || die 2 "checkout changes Rust vs $MAIN_SHA"
if [[ -n ${E0S_MODEL_CACHE:-} && ! -d ${E0S_MODEL_CACHE} ]]; then
  die 2 "missing E0S_MODEL_CACHE $E0S_MODEL_CACHE"
fi

while read -r var; do unset "$var"; done < <(compgen -e | grep '^KIMETSU_' || true)

mkdir -p "$OUT" "$LOGS"
exec 9>"$OUT/run_e0s.lock"
flock -n 9 || die 5 "another run_e0s.sh holds $OUT/run_e0s.lock"
START=$(ts)
MEMLOG=""
cleanup() {
  if [[ -n ${MEMLOG} ]]; then
    kill "$MEMLOG" 2>/dev/null || true
    wait "$MEMLOG" 2>/dev/null || true
  fi
}
trap cleanup EXIT
( while :; do echo "$(date +%s) $(awk '/^MemAvailable:/{print $2}' /proc/meminfo)"; sleep 30; done ) > "$LOGS/memavailable.log" &
MEMLOG=$!

mkdir -p "$CACHE" "$OUT/bin" "$OUT/tmp"
if [[ -n ${E0S_MODEL_CACHE:-} && -z $(ls -A "$CACHE" 2>/dev/null || true) ]]; then
  cp -a "$E0S_MODEL_CACHE"/. "$CACHE"/
fi
(cd "$CACHE" && find . \( -name '*.onnx' -o -name 'tokenizer.json' \) | sort | xargs -r sha256sum) > "$LOGS/model-sha256.before" || true

if [[ -n $BIN_ARG ]]; then
  [[ -f $BIN ]] || die 2 "missing binary $BIN"
  log "reusing $BIN"
else
  log "building release at $SHA (Rust vs $MAIN_SHA, nice 19, -j$JOBS) -> $LOGS/build.log"
  ( cd "$REPO" && "${build_env[@]}" "${build_cmd[@]}" ) > "$LOGS/build.log" 2>&1 \
    || die 3 "cargo build failed; tail $LOGS/build.log"
  cp "$TARGET/release/kimetsu" "$BIN"
fi
VERSION=$("$BIN" --version)
BIN_SHA=$(sha256sum "$BIN" | cut -d' ' -f1)
log "built $VERSION sha256=$BIN_SHA"
grep -q embeddings <<<"$VERSION" || die 3 "binary is not an embeddings build: $VERSION"
[[ -z $(git -C "$REPO" status --porcelain) ]] || die 3 "worktree dirty after build"

for r in "${RUNS[@]}"; do
  mkdir -p "$OUT/$r" "$OUT/tmp/$r"
  for ds in "${DATASETS[@]}"; do
    meta=$OUT/$r/$ds.meta.json
    if [[ -f $meta ]] && "$PY" -c 'import json,sys; sys.exit(0 if json.load(open(sys.argv[1])).get("exit")==0 else 1)' "$meta"; then
      log "$r/$ds already complete, skipping"
      continue
    fi
    log "$r/$ds start"
    eval_argv "$r" "$ds"
    TMPDIR=$OUT/tmp/$r "${eval_env[@]}" "${EVAL_ARGV[@]}" > "$OUT/$r/$ds.stdout.log" 2>&1 \
      || die 4 "$r/$ds failed; see $OUT/$r/$ds.stdout.log"
    log "$r/$ds done: $(tail -1 "$OUT/$r/$ds.stdout.log")"
  done
  "$PY" "$REPO/scripts/experiments/e0/summarize_e0.py" summary "$OUT/$r" > /dev/null
  "$PY" "$SCRIPT_DIR/sweep_hist.py" "$OUT/$r"
done

(cd "$CACHE" && find . \( -name '*.onnx' -o -name 'tokenizer.json' \) | sort | xargs -r sha256sum) > "$LOGS/model-sha256.after" || true
if [[ -f $LOGS/model-sha256.before ]] && ! cmp -s "$LOGS/model-sha256.before" "$LOGS/model-sha256.after"; then
  log "WARNING: model files changed during the run"
fi

set +e
"$PY" "$REPO/scripts/experiments/e0/summarize_e0.py" diff "$OUT/run1" "$OUT/run2" "$OUT/determinism-diff.md"
DET=$?
set -e

"$PY" - "$OUT" "${DATASETS[@]}" <<'PY'
import json, sys
from pathlib import Path
res = Path(sys.argv[1])
datasets = sys.argv[2:]
out = {}
for ds in datasets:
    a = json.loads((res / "run1" / f"resolved-config-{ds}.json").read_text())
    b = json.loads((res / "run2" / f"resolved-config-{ds}.json").read_text())
    def strip(doc):
        return {arm: {k: v for k, v in arm_doc.items() if k != "env"} for arm, arm_doc in doc["arms"].items()}
    out[ds] = {
        "arms": {
            arm: dict(arm_doc["resolved"], knob=arm_doc["knob"], patch=arm_doc["patch"])
            for arm, arm_doc in a["arms"].items()
        },
        "mcp_args": a["mcp_args"],
        "base_patch": a["base_patch"],
        "semantic_floor_note": "fused_defaults semantic floor is inferred: config -1 -> auto 0.35 for bge (project.rs:389-391)",
        "identical_run1_run2": strip(a) == strip(b),
        "full_dumps": [str(res / run / f"resolved-config-{ds}.json") for run in ("run1", "run2")],
    }
(res / "resolved-config-per-arm.json").write_text(json.dumps(out, indent=2) + "\n")
PY

END=$(ts)
{
  echo "# E0 sweep manifest"
  echo
  echo "- spec: E0 sweep (#68). runner: $SCRIPT_PATH. nice 19. RSS cap is external (memcap.py)."
  echo "- started: $START; finished: $END"
  echo "- code: $REPO @ $SHA (baseline $MAIN_SHA)"
  echo "- binary: $BIN, $VERSION, sha256 $BIN_SHA; build log $LOGS/build.log"
  if [[ -z $BIN_ARG ]]; then
    echo "- build: (cd $REPO && ${build_env[*]} ${build_cmd[*]})"
  else
    echo "- build: reused $BIN"
  fi
  echo "- data: $COUNT_TEXT"
  echo "- scorer: $REPO/scripts/retrieval_eval.py metrics via scripts/e0_eval.py --profile $PROFILE; retrieval only, no reader, not an official judge"
  echo "- seed: none (deterministic); PYTHONHASHSEED=0. threads: 1 ONNX thread"
  echo "- rrf: only k=60 (fusion.rs:188 Fusion::Rrf => rrf_fuse(lists, DEFAULT_RRF_K), DEFAULT_RRF_K=60.0 at fusion.rs:64; no config knob)"
  echo "- eval env: ${eval_env[*]}"
  echo "- eval commands:"
  for r in "${RUNS[@]}"; do
    for ds in "${DATASETS[@]}"; do
      eval_argv "$r" "$ds"
      echo "  - TMPDIR=$OUT/tmp/$r ${eval_env[*]} ${EVAL_ARGV[*]}"
    done
  done
  echo "- results: $OUT/run{1,2} (JSONL, meta, stdout, summary.md/json, capsule-hist.md/json), $OUT/determinism-diff.md (exit $DET), $OUT/resolved-config-per-arm.json"
  echo "- memcap log: external. MemAvailable trace: $LOGS/memavailable.log"
  echo "- script sha256 (this checkout):"
  (cd "$SCRIPT_DIR" && sha256sum run_e0s.sh memcap.py sweep_hist.py) | sed 's/^/  /'
  echo
  echo "## run1 summary"
  echo
  cat "$OUT/run1/summary.md"
  echo
  cat "$OUT/run1/capsule-hist.md"
} > "$OUT/MANIFEST.md"
log "E0 sweep complete. determinism diff exit=$DET. manifest $OUT/MANIFEST.md"
exit "$DET"
