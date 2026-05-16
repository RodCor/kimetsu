#!/bin/bash
# MP-15c — `build-pov-ray` brain-leg variance check.
#
# Background: in MP-14d, kimetsu-no-brain WON build-pov-ray and
# kimetsu-brain LOST it (the only brain regression vs no-brain on
# the curated 16-task slice). Either the memory pool actively
# biased the model wrong on this task, or it's run-to-run
# variance at n=1. This script re-runs *only* the build-pov-ray
# task in brain mode three times so we can tell.
#
# Decision rule:
#   3 wins, 0 losses  -> MP-14d was variance, no action.
#   2 wins, 1 loss    -> still consistent with variance; defer.
#   <=1 win           -> memory pool actively biasing wrong;
#                        inspect /home/kimetsu/kimetsu-bench-project/
#                        for capsules that pull attention off the
#                        build flow (likely "redirect to /tmp/build.log"
#                        guidance overfitted).
#
# Usage (inside WSL Ubuntu-24.04, as user kimetsu, with
# CLAUDE_CODE_OAUTH_TOKEN exported):
#
#   bash kimetsu_harbor/povray-variance.sh
#
# Results land in /home/kimetsu/harbor-jobs/jobs/povray-variance-N/
# for N in {1,2,3}, plus a one-line summary written to
# /home/kimetsu/povray-variance-summary.txt.

set -uo pipefail

REPO=/mnt/e/Kimetsu
JOBS_DIR=/home/kimetsu/harbor-jobs/jobs
BRAIN_PROJECT=/home/kimetsu/kimetsu-bench-project
SUMMARY=/home/kimetsu/povray-variance-summary.txt
N_RUNS=3
TASK_GLOB='build-pov-ray*'

source /home/kimetsu/.cargo/env 2>/dev/null || true

if [[ -z "${CLAUDE_CODE_OAUTH_TOKEN:-}" ]]; then
    echo "CLAUDE_CODE_OAUTH_TOKEN missing — export it first." >&2
    exit 2
fi

cd "$REPO"
echo "[$(date -u +%H:%M:%SZ)] building..."
cargo build -p kimetsu-cli --release >/dev/null 2>&1 || {
    echo "build failed; aborting" >&2
    exit 3
}

export KIMETSU_BIN="$REPO/target/release/kimetsu"
export KIMETSU_HARBOR_MODEL="claude-opus-4-7"
export KIMETSU_HARBOR_PROJECT="$BRAIN_PROJECT"

: > "$SUMMARY"
echo "povray-variance run @ $(date -u +%Y-%m-%dT%H:%M:%SZ)" >> "$SUMMARY"

wins=0
zeros=0
errors=0
for i in $(seq 1 "$N_RUNS"); do
    job="povray-variance-$i"
    dir="$JOBS_DIR/$job"
    rm -rf "$dir"
    echo "[$(date -u +%H:%M:%SZ)] run $i/$N_RUNS ($job)..."
    if ! harbor run \
        -d terminal-bench/terminal-bench-2 \
        --agent-import-path kimetsu_harbor.kimetsu_agent:KimetsuAgent \
        -i "$TASK_GLOB" \
        -n 1 -k 1 \
        --job-name "$job" \
        -o "$JOBS_DIR" \
        -y >>"$SUMMARY" 2>&1; then
        echo "  run $i: harbor exited non-zero" >> "$SUMMARY"
    fi

    # Pull this run's reward.
    reward=$(python3 - "$dir" <<'PY'
import json, sys, glob, os
d = sys.argv[1]
try:
    r = json.load(open(os.path.join(d, "result.json")))
    ev = list(r["stats"]["evals"].values())[0]
    rs = ev["reward_stats"]["reward"]
    if rs.get("1.0"):
        print("1.0")
    elif rs.get("0.0"):
        print("0.0")
    else:
        print("error")
except Exception as e:
    print(f"parse-err:{e}")
PY
)
    echo "  run $i reward=$reward" | tee -a "$SUMMARY"
    case "$reward" in
        1.0) wins=$((wins+1)) ;;
        0.0) zeros=$((zeros+1)) ;;
        *)   errors=$((errors+1)) ;;
    esac
done

echo "" >> "$SUMMARY"
echo "=== povray-variance verdict ===" | tee -a "$SUMMARY"
echo "wins=$wins zeros=$zeros errors=$errors (of $N_RUNS)" | tee -a "$SUMMARY"
if [[ "$wins" -ge 2 ]]; then
    echo "verdict: MP-14d brain loss was variance; no action needed." | tee -a "$SUMMARY"
elif [[ "$wins" -eq 1 ]]; then
    echo "verdict: borderline; one more pair of runs would help." | tee -a "$SUMMARY"
else
    echo "verdict: brain consistently regresses on build-pov-ray;" | tee -a "$SUMMARY"
    echo "         inspect $BRAIN_PROJECT for biasing capsules." | tee -a "$SUMMARY"
fi
