#!/bin/bash
# MP-15d — daily stability gauntlet for the v0.2 gate-3 evidence.
#
# Designed to run unattended from cron. Each daily firing:
#   1. Rebuilds the kimetsu Linux binary (incremental cargo).
#   2. Runs the 16-task no-brain leg.
#   3. Runs the 16-task brain leg with the curated memory pool.
#   4. Logs to /home/kimetsu/stability-logs/stability-YYYY-MM-DD.log
#   5. Drops per-leg results into
#      /home/kimetsu/harbor-jobs/jobs/stability-YYYY-MM-DD-{no-brain,brain}/
#
# Token handling: cron has no interactive session, so the OAuth
# token must live in a file. Create /home/kimetsu/.claude-oauth.env
# once (chmod 600) with:
#
#   export CLAUDE_CODE_OAUTH_TOKEN=<your-token-here>
#
# Install (run once, as user kimetsu):
#
#   chmod +x /mnt/e/Kimetsu/kimetsu_harbor/stability-cron.sh
#   ( crontab -l 2>/dev/null; echo "0 3 * * * /mnt/e/Kimetsu/kimetsu_harbor/stability-cron.sh" ) | crontab -
#
# Uninstall:
#
#   crontab -l | grep -v stability-cron.sh | crontab -

set -uo pipefail

DAY=$(date -u +%Y-%m-%d)
REPO=/mnt/e/Kimetsu
JOBS_DIR=/home/kimetsu/harbor-jobs/jobs
BRAIN_PROJECT=/home/kimetsu/kimetsu-bench-project
TOKEN_FILE=/home/kimetsu/.claude-oauth.env
LOG_DIR=/home/kimetsu/stability-logs
LOG="$LOG_DIR/stability-$DAY.log"

mkdir -p "$LOG_DIR" "$JOBS_DIR"
ts() { date -u +'%Y-%m-%dT%H:%M:%SZ'; }
log() { echo "[$(ts)] $*" | tee -a "$LOG"; }

log "===== stability run $DAY ====="

# Cron PATH is minimal — wire cargo / harbor explicitly.
if [[ -f /home/kimetsu/.cargo/env ]]; then
    source /home/kimetsu/.cargo/env
fi
export PATH="/usr/local/bin:/usr/bin:/bin:/home/kimetsu/.cargo/bin:/home/kimetsu/.local/bin:$PATH"

# Source the OAuth token. Fail loudly if missing — we want the log
# to make the cause obvious instead of failing silently per trial.
if [[ ! -f "$TOKEN_FILE" ]]; then
    log "FATAL: token file $TOKEN_FILE missing. Create it (chmod 600) with: export CLAUDE_CODE_OAUTH_TOKEN=..."
    exit 2
fi
# shellcheck disable=SC1090
source "$TOKEN_FILE"
if [[ -z "${CLAUDE_CODE_OAUTH_TOKEN:-}" ]]; then
    log "FATAL: $TOKEN_FILE did not export CLAUDE_CODE_OAUTH_TOKEN"
    exit 3
fi
log "token: present (len=${#CLAUDE_CODE_OAUTH_TOKEN})"

# MP-15a: stability runs use the bumped default 1500s timeout. The
# env override stays available if a single bad day needs retuning.
: "${KIMETSU_HARBOR_PROVIDER_TIMEOUT_SECS:=1500}"
export KIMETSU_HARBOR_PROVIDER_TIMEOUT_SECS

# Build.
cd "$REPO"
log "build..."
if ! cargo build -p kimetsu-cli --release >>"$LOG" 2>&1; then
    log "FATAL: cargo build failed"
    exit 4
fi
export KIMETSU_BIN="$REPO/target/release/kimetsu"
export KIMETSU_HARBOR_MODEL="claude-opus-4-7"
log "binary: $KIMETSU_BIN ($(stat -c %s "$KIMETSU_BIN") bytes)"

run_leg() {
    local leg="$1"
    local job="stability-$DAY-$leg"
    local dir="$JOBS_DIR/$job"

    if [[ "$leg" == "brain" ]]; then
        export KIMETSU_HARBOR_PROJECT="$BRAIN_PROJECT"
    else
        unset KIMETSU_HARBOR_PROJECT
    fi
    rm -rf "$dir"

    log "===== leg=$leg ====="
    log "KIMETSU_HARBOR_PROJECT=${KIMETSU_HARBOR_PROJECT:-<unset>}"

    harbor run \
        -d terminal-bench/terminal-bench-2 \
        --agent-import-path kimetsu_harbor.kimetsu_agent:KimetsuAgent \
        -n 2 -k 1 -l 16 \
        --job-name "$job" \
        -o "$JOBS_DIR" \
        -y >>"$LOG" 2>&1 || log "WARN: $leg harbor returned non-zero"

    if [[ -f "$dir/result.json" ]]; then
        local mean
        mean=$(python3 -c "
import json
d = json.load(open('$dir/result.json'))
print(list(d['stats']['evals'].values())[0]['metrics'][0]['mean'])
" 2>/dev/null || echo "?")
        log "leg=$leg mean=$mean"
    else
        log "leg=$leg: no result.json at $dir"
    fi
}

run_leg "no-brain"
run_leg "brain"

log "===== done ====="
