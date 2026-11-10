#!/usr/bin/env bash
# Wait for free RAM, build release with embeddings, run both passes, diff.
#
# Portable E0 runner. Point it at fixtures from scripts/prepare_e0_data.py.
# It does not wait on other processes and does not set a C++ or OpenSSL toolchain.
#
# Usage:
#   scripts/experiments/e0/run_e0.sh --data-dir DIR [--out DIR] [--bin PATH]
#                                    [--jobs N] [--limit N] [--dry-run]
#
# DIR contains locomo-conv26.json and lme-oracle.json.
# Default output directory is ./e0-results.
#
# Environment:
#   MIN_AVAIL_MIB   free RAM required before the build and each dataset (default 6144)
#   POLL_S          seconds between memory checks (default 300)
#   RAM_WAIT_MAX_S  give up after this many seconds below the threshold (default 14400)
#   PYTHON          python interpreter (default python3)
#   CARGO           cargo binary (default cargo)
#
# Exit: 0 both passes finished (read determinism-diff.md for the verdict),
#       2 preflight failed, 3 build failed, 4 eval failed, 5 already running.
set -euo pipefail

DRY=0
DATA=""
OUT=""
BIN_ARG=""
JOBS=4
LIMIT=0

usage() {
  sed -n '2,22p' "$0"
}

die() {
  echo "[$(date '+%Y-%m-%d %H:%M:%S %Z')] FAIL: $2" >&2
  exit "$1"
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --dry-run) DRY=1; shift ;;
    --data-dir) DATA=${2:-}; shift 2 ;;
    --out) OUT=${2:-}; shift 2 ;;
    --bin) BIN_ARG=${2:-}; shift 2 ;;
    --jobs) JOBS=${2:-}; shift 2 ;;
    --limit) LIMIT=${2:-}; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) die 2 "unknown argument: $1" ;;
  esac
done

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO=$(cd "$SCRIPT_DIR/../../.." && pwd)
PY=${PYTHON:-python3}
CARGO=${CARGO:-cargo}
OUT=${OUT:-$PWD/e0-results}
DATA_DISP=${DATA:-"<data-dir>"}
MIN_AVAIL_MIB=${MIN_AVAIL_MIB:-6144}
POLL_S=${POLL_S:-300}
RAM_WAIT_MAX_S=${RAM_WAIT_MAX_S:-14400}
MIN_AVAIL_KIB=$((MIN_AVAIL_MIB * 1024))
DATASETS=(locomo-conv26 lme-oracle)
RUNS=(run1 run2)
LOGS=$OUT/logs
CACHE=$OUT/model-cache

ts() { date '+%Y-%m-%d %H:%M:%S %Z'; }
log() { echo "[$(ts)] $*"; }
show() { printf '  $'; printf ' %q' "$@"; printf '\n'; }

while read -r var; do
  unset "$var"
done < <(compgen -e | grep '^KIMETSU_' || true)

avail_kib() { awk '/^MemAvailable:/{print $2}' /proc/meminfo; }
wait_ram() {
  local waited=0
  local avail
  avail=$(avail_kib)
  [[ -n $avail ]] || die 2 "MemAvailable is not readable from /proc/meminfo"
  while (( avail < MIN_AVAIL_KIB )); do
    (( waited >= RAM_WAIT_MAX_S )) && die 2 "MemAvailable ${avail} KiB < ${MIN_AVAIL_MIB} MiB after ${waited}s"
    log "MemAvailable $((avail / 1024)) MiB < ${MIN_AVAIL_MIB} MiB; rechecking in ${POLL_S}s"
    sleep "$POLL_S"
    waited=$((waited + POLL_S))
    avail=$(avail_kib)
  done
  log "MemAvailable $((avail / 1024)) MiB >= ${MIN_AVAIL_MIB} MiB"
}

build_cmd=("$CARGO" build -p kimetsu-cli --bin kimetsu --release --locked --features embeddings -j "$JOBS")
eval_env=(
  env
  PYTHONHASHSEED=0
  KIMETSU_INTRA_THREADS=1
  OMP_NUM_THREADS=1
  OPENBLAS_NUM_THREADS=1
  FASTEMBED_CACHE_DIR="$CACHE"
  HF_HOME="$CACHE"
  KIMETSU_USER_BRAIN=0
  KIMETSU_BRAIN_AMBIENT=off
  KIMETSU_BRAIN_LOG_RETRIEVAL=0
)

if [[ -n $BIN_ARG ]]; then
  BIN=$BIN_ARG
else
  BIN=$OUT/bin/kimetsu
fi

e0_argv() { # $1 run, $2 dataset
  E0_ARGV=(
    nice -n 10 "$PY" -u "$REPO/scripts/e0_eval.py"
    --bin "$BIN" --profile bench --name "$2"
    --dataset "$DATA_DISP/$2.json" --out "$OUT/$1/$2.jsonl"
  )
  if (( LIMIT > 0 )); then
    E0_ARGV+=(--limit "$LIMIT")
  fi
}

log "E0 runner, dry_run=$DRY"
[[ -f $REPO/Cargo.toml ]] || die 2 "missing $REPO/Cargo.toml"
[[ -f $REPO/scripts/e0_eval.py ]] || die 2 "missing scripts/e0_eval.py"
"$PY" -m py_compile \
  "$REPO/scripts/e0_eval.py" \
  "$REPO/scripts/prepare_e0_data.py" \
  "$REPO/scripts/retrieval_eval.py" \
  "$REPO/scripts/retrieval_eval_profiles.py" \
  "$SCRIPT_DIR/summarize_e0.py"
"$PY" "$REPO/scripts/retrieval_eval.py" --self-test >/dev/null || die 2 "retrieval_eval --self-test failed"

if [[ -n $DATA ]]; then
  for ds in "${DATASETS[@]}"; do
    [[ -f $DATA/$ds.json ]] || die 2 "missing $DATA/$ds.json"
    "$PY" "$REPO/scripts/e0_eval.py" --bin "$BIN" --dataset "$DATA/$ds.json" --name "$ds" --out "$OUT/$ds.jsonl" --dry-run \
      | "$PY" -c 'import json,sys; d=json.load(sys.stdin); print("  data %s: %d questions, %d scored, arms=%s" % (d["name"], d["questions"], d["scored"], d["arms"]))'
  done
elif (( DRY == 0 )); then
  die 2 "--data-dir is required"
else
  log "no --data-dir; skipping dataset dry-run"
fi

if (( DRY == 0 )); then
  command -v nice >/dev/null || die 2 "nice not found"
  command -v flock >/dev/null || die 2 "flock not found"
  command -v sha256sum >/dev/null || die 2 "sha256sum not found"
  if [[ -z $BIN_ARG ]]; then
    command -v "$CARGO" >/dev/null || die 2 "cargo not found"
  fi
fi

if (( DRY )); then
  echo
  log "DRY RUN: wait for free RAM, build release with embeddings, run both passes, diff"
  echo "  # MemAvailable >= ${MIN_AVAIL_MIB} MiB (poll every ${POLL_S}s, give up after ${RAM_WAIT_MAX_S}s)"
  if [[ -n $BIN_ARG ]]; then
    echo "  # reuse binary:"
    show "$BIN" --version
  else
    echo "  # release build with embeddings:"
    echo "  cd $REPO && \\"
    show nice -n 10 "${build_cmd[@]}"
    echo "    > $LOGS/build.log 2>&1"
    show cp "$REPO/target/release/kimetsu" "$BIN"
    show "$BIN" --version
  fi
  echo "  # two passes, four binary arms plus the script vector arm per question:"
  for run in "${RUNS[@]}"; do
    for ds in "${DATASETS[@]}"; do
      e0_argv "$run" "$ds"
      echo "  TMPDIR=$OUT/tmp/$run \\"
      show "${eval_env[@]}" "${E0_ARGV[@]}"
      echo "    > $OUT/$run/$ds.stdout.log 2>&1"
    done
    show "$PY" "$SCRIPT_DIR/summarize_e0.py" summary "$OUT/$run"
  done
  show "$PY" "$SCRIPT_DIR/summarize_e0.py" diff "$OUT/run1" "$OUT/run2" "$OUT/determinism-diff.md"
  echo "  write $OUT/resolved-config-per-arm.json and $OUT/MANIFEST.md"
  exit 0
fi

mkdir -p "$OUT" "$LOGS"
exec 9>"$OUT/run_e0.lock"
flock -n 9 || die 5 "another run holds $OUT/run_e0.lock"
START=$(ts)
GIT_SHA=$(git -C "$REPO" rev-parse HEAD 2>/dev/null || echo unknown)
if [[ -n $(git -C "$REPO" status --porcelain 2>/dev/null || true) ]]; then
  log "worktree has local changes; the manifest records the status"
fi

wait_ram
( while :; do echo "$(date +%s) $(avail_kib)"; sleep 30; done ) > "$LOGS/memavailable.log" &
MEMLOG=$!
trap 'kill $MEMLOG 2>/dev/null || true' EXIT

mkdir -p "$CACHE" "$OUT/bin"
if [[ -z $BIN_ARG ]]; then
  log "building release (embeddings, -j$JOBS) -> $LOGS/build.log"
  ( cd "$REPO" && nice -n 10 "${build_cmd[@]}" ) > "$LOGS/build.log" 2>&1 \
    || die 3 "cargo build failed; tail $LOGS/build.log"
  cp "$REPO/target/release/kimetsu" "$BIN"
fi
VERSION=$("$BIN" --version)
printf '%s\n' "$VERSION" | grep -q embeddings || die 3 "binary is not an embeddings build: $VERSION"
BIN_SHA=$(sha256sum "$BIN" | cut -d' ' -f1)
log "using $VERSION sha256=$BIN_SHA"

if [[ -d $CACHE ]]; then
  ( cd "$CACHE" && find . \( -name '*.onnx' -o -name 'tokenizer.json' \) | sort | xargs -r sha256sum ) > "$LOGS/model-sha256.before" || true
fi

for run in "${RUNS[@]}"; do
  mkdir -p "$OUT/$run" "$OUT/tmp/$run"
  for ds in "${DATASETS[@]}"; do
    meta=$OUT/$run/$ds.meta.json
    if [[ -f $meta ]] && "$PY" -c 'import json,sys; sys.exit(0 if json.load(open(sys.argv[1])).get("exit")==0 else 1)' "$meta"; then
      log "$run/$ds already complete, skipping"
      continue
    fi
    wait_ram
    log "$run/$ds start"
    e0_argv "$run" "$ds"
    TMPDIR=$OUT/tmp/$run "${eval_env[@]}" "${E0_ARGV[@]}" > "$OUT/$run/$ds.stdout.log" 2>&1 \
      || die 4 "$run/$ds failed; see $OUT/$run/$ds.stdout.log"
    log "$run/$ds done: $(tail -1 "$OUT/$run/$ds.stdout.log")"
  done
  "$PY" "$SCRIPT_DIR/summarize_e0.py" summary "$OUT/$run" > /dev/null
done

if [[ -d $CACHE ]]; then
  ( cd "$CACHE" && find . \( -name '*.onnx' -o -name 'tokenizer.json' \) | sort | xargs -r sha256sum ) > "$LOGS/model-sha256.after" || true
  if [[ -f $LOGS/model-sha256.before ]] && ! cmp -s "$LOGS/model-sha256.before" "$LOGS/model-sha256.after"; then
    log "WARNING: model files changed during the run"
  fi
fi

set +e
"$PY" "$SCRIPT_DIR/summarize_e0.py" diff "$OUT/run1" "$OUT/run2" "$OUT/determinism-diff.md"
DET=$?
set -e

"$PY" - "$OUT" <<'PY'
import json, sys
from pathlib import Path
res = Path(sys.argv[1])
out = {}
for ds in ("locomo-conv26", "lme-oracle"):
    a = json.loads((res / "run1" / f"resolved-config-{ds}.json").read_text())
    b = json.loads((res / "run2" / f"resolved-config-{ds}.json").read_text())
    def strip(doc):
        return {arm: {k: v for k, v in arm_doc.items() if k != "env"} for arm, arm_doc in doc["arms"].items()}
    out[ds] = {
        "arms": {arm: dict(arm_doc["resolved"], knob=arm_doc["knob"], patch=arm_doc["patch"]) for arm, arm_doc in a["arms"].items()},
        "mcp_args": a["mcp_args"],
        "base_patch": a["base_patch"],
        "identical_run1_run2": strip(a) == strip(b),
        "full_dumps": [str(res / run / f"resolved-config-{ds}.json") for run in ("run1", "run2")],
    }
(res / "resolved-config-per-arm.json").write_text(json.dumps(out, indent=2))
PY

END=$(ts)
{
  echo "# E0 manifest"
  echo
  echo "- started: $START"
  echo "- finished: $END"
  echo "- git: $GIT_SHA ($REPO)"
  echo "- binary: $BIN"
  echo "- version: $VERSION"
  echo "- sha256: $BIN_SHA"
  echo "- build log: $LOGS/build.log"
  if [[ -z $BIN_ARG ]]; then
    echo "- build: (cd $REPO && nice -n 10 ${build_cmd[*]})"
  else
    echo "- build: reused $BIN_ARG"
  fi
  echo "- datasets: $DATA/locomo-conv26.json and $DATA/lme-oracle.json"
  echo "- profile: bench via $REPO/scripts/e0_eval.py"
  echo "- summarizer: $SCRIPT_DIR/summarize_e0.py"
  echo "- seed: none. PYTHONHASHSEED=0"
  echo "- threads: KIMETSU_INTRA_THREADS=1 OMP_NUM_THREADS=1 OPENBLAS_NUM_THREADS=1"
  echo "- model cache: $CACHE"
  echo "- eval env: ${eval_env[*]}"
  echo "- commands:"
  for run in "${RUNS[@]}"; do
    for ds in "${DATASETS[@]}"; do
      e0_argv "$run" "$ds"
      echo "  - TMPDIR=$OUT/tmp/$run ${eval_env[*]} ${E0_ARGV[*]}"
    done
  done
  echo "- results: $OUT/run1 $OUT/run2"
  echo "- determinism diff exit: $DET ($OUT/determinism-diff.md)"
  echo "- resolved config: $OUT/resolved-config-per-arm.json"
  echo "- MemAvailable trace: $LOGS/memavailable.log"
  echo
  echo "## run1 summary"
  echo
  cat "$OUT/run1/summary.md"
} > "$OUT/MANIFEST.md"
log "E0 complete. determinism diff exit=$DET ($OUT/determinism-diff.md). manifest $OUT/MANIFEST.md"
exit 0
