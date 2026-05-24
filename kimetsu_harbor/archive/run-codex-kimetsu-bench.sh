#!/usr/bin/env bash
set -euo pipefail

# Run Harbor's built-in Codex agent against Terminal-Bench with Kimetsu MCP
# mounted into each task container. Intended to be launched inside WSL2.

KIMETSU_REPO="${KIMETSU_REPO:-/mnt/e/Kimetsu}"
HARBOR_DATASET="${HARBOR_DATASET:-terminal-bench/terminal-bench-2}"
CODEX_MODEL="${CODEX_MODEL:-gpt-5.5}"
CODEX_VERSION="${CODEX_VERSION:-0.128.0}"
TASK_LIMIT="${TASK_LIMIT:-16}"
N_CONCURRENT="${N_CONCURRENT:-2}"
N_ATTEMPTS="${N_ATTEMPTS:-1}"
JOB_NAME="${JOB_NAME:-codex-kimetsu-mcp}"
JOBS_DIR="${JOBS_DIR:-/root/harbor-jobs/jobs}"
CODEX_AUTH_JSON_PATH="${CODEX_AUTH_JSON_PATH:-/mnt/c/Users/rodri/.codex/auth.json}"
KIMETSU_MOUNT_READ_ONLY="${KIMETSU_MOUNT_READ_ONLY:-1}"
KIMETSU_BRAIN_PREP="${KIMETSU_BRAIN_PREP:-auto}"
KIMETSU_MCP_MODE="${KIMETSU_MCP_MODE:-optional}"
KIMETSU_CODEX_ENFORCE_BRAIN="${KIMETSU_CODEX_ENFORCE_BRAIN:-auto}"
KIMETSU_CONTAINER_TARGET="${KIMETSU_CONTAINER_TARGET:-x86_64-unknown-linux-musl}"
KIMETSU_CONTAINER_BINARY="${KIMETSU_CONTAINER_BINARY:-$KIMETSU_REPO/target/$KIMETSU_CONTAINER_TARGET/release/kimetsu}"
KIMETSU_CONTAINER_BINARY_BUILD="${KIMETSU_CONTAINER_BINARY_BUILD:-auto}"

case "$KIMETSU_MCP_MODE" in
  optional|required|none) ;;
  *)
    echo "KIMETSU_MCP_MODE must be 'optional', 'required', or 'none' (got '$KIMETSU_MCP_MODE')" >&2
    exit 1
    ;;
esac

case "$KIMETSU_CODEX_ENFORCE_BRAIN" in
  auto|0|1|false|true|no|yes) ;;
  *)
    echo "KIMETSU_CODEX_ENFORCE_BRAIN must be auto, 1, or 0 (got '$KIMETSU_CODEX_ENFORCE_BRAIN')" >&2
    exit 1
    ;;
esac

cd "$KIMETSU_REPO"

kimetsu_enabled=1
if [[ "$KIMETSU_MCP_MODE" == "none" ]]; then
  kimetsu_enabled=0
fi

codex_brain_enforced=0
if [[ "$kimetsu_enabled" == "1" ]]; then
  case "$KIMETSU_CODEX_ENFORCE_BRAIN" in
    1|true|yes) codex_brain_enforced=1 ;;
    auto)
      if [[ "$KIMETSU_MCP_MODE" == "required" ]]; then
        codex_brain_enforced=1
      fi
      ;;
  esac
fi

if [[ "$kimetsu_enabled" == "1" ]]; then
needs_rebuild=0
if [[ ! -x "$KIMETSU_REPO/target/release/kimetsu" ]] ||
   ! "$KIMETSU_REPO/target/release/kimetsu" mcp serve --help >/dev/null 2>&1; then
  needs_rebuild=1
elif find "$KIMETSU_REPO/crates" "$KIMETSU_REPO/Cargo.toml" "$KIMETSU_REPO/Cargo.lock" \
  -type f -newer "$KIMETSU_REPO/target/release/kimetsu" -print -quit | grep -q .; then
  needs_rebuild=1
fi

if [[ "$needs_rebuild" == "1" ]]; then
  if [[ -f "$HOME/.cargo/env" ]]; then
    # shellcheck disable=SC1091
    source "$HOME/.cargo/env"
  fi
  cargo build --release -p kimetsu-cli
fi

container_needs_rebuild=0
if [[ "$KIMETSU_CONTAINER_BINARY_BUILD" != "0" ]]; then
  if [[ ! -x "$KIMETSU_CONTAINER_BINARY" ]]; then
    container_needs_rebuild=1
  elif find "$KIMETSU_REPO/crates" "$KIMETSU_REPO/Cargo.toml" "$KIMETSU_REPO/Cargo.lock" \
    -type f -newer "$KIMETSU_CONTAINER_BINARY" -print -quit | grep -q .; then
    container_needs_rebuild=1
  fi

  if [[ "$container_needs_rebuild" == "1" ]]; then
    if [[ -f "$HOME/.cargo/env" ]]; then
      # shellcheck disable=SC1091
      source "$HOME/.cargo/env"
    fi
    if ! rustup target list --installed | grep -qx "$KIMETSU_CONTAINER_TARGET"; then
      rustup target add "$KIMETSU_CONTAINER_TARGET"
    fi
    if [[ "$KIMETSU_CONTAINER_TARGET" == "x86_64-unknown-linux-musl" ]] &&
       ! command -v x86_64-linux-musl-gcc >/dev/null 2>&1; then
      echo "musl compiler missing; install it in WSL before running Harbor tasks:" >&2
      echo "  apt-get update && apt-get install -y musl-tools" >&2
      exit 1
    fi
    cargo build --release -p kimetsu-cli --target "$KIMETSU_CONTAINER_TARGET"
  fi
fi

if [[ ! -x "$KIMETSU_CONTAINER_BINARY" ]]; then
  echo "Kimetsu container-compatible MCP binary missing: $KIMETSU_CONTAINER_BINARY" >&2
  echo "Leave KIMETSU_CONTAINER_BINARY_BUILD=auto or build it before launching Harbor." >&2
  exit 1
fi

if [[ "$KIMETSU_BRAIN_PREP" != "0" ]]; then
  if [[ ! -f "$KIMETSU_REPO/.kimetsu/project.toml" ]]; then
    echo "Kimetsu brain project missing; initializing $KIMETSU_REPO" >&2
    "$KIMETSU_REPO/target/release/kimetsu" init >/dev/null
  fi

  repo_root="$(pwd -P)"
  indexed_files=0
  if [[ -f "$KIMETSU_REPO/.kimetsu/brain.db" ]]; then
    indexed_files="$(
      python3 - "$KIMETSU_REPO/.kimetsu/brain.db" "$repo_root" <<'PY'
import sqlite3
import sys

try:
    conn = sqlite3.connect(sys.argv[1])
    row = conn.execute(
        "SELECT COUNT(*) FROM repo_files WHERE repo_root = ?",
        (sys.argv[2],),
    ).fetchone()
    print(row[0] if row else 0)
except Exception:
    print(0)
PY
    )"
  fi

  if [[ "$KIMETSU_BRAIN_PREP" == "1" || "$indexed_files" == "0" ]]; then
    echo "Preparing Kimetsu brain repo index for $repo_root" >&2
    "$KIMETSU_REPO/target/release/kimetsu" brain ingest-repo "$KIMETSU_REPO" >/dev/null
  fi
fi
fi

if [[ ! -f "$CODEX_AUTH_JSON_PATH" ]]; then
  echo "Codex auth JSON not found: $CODEX_AUTH_JSON_PATH" >&2
  echo "Set CODEX_AUTH_JSON_PATH or run Codex login on the host first." >&2
  exit 1
fi

kimetsu_mode_instruction=""
kimetsu_mcp_wrapper=""
kimetsu_brain_helper=""
kimetsu_mcp_probe=""
mounts_json=""
mcp_config=""
if [[ "$kimetsu_enabled" == "1" ]]; then
if [[ ! -d "$KIMETSU_REPO/.codex/skills/kimetsu-bridge" ]]; then
  echo "Kimetsu bridge skill missing at $KIMETSU_REPO/.codex/skills/kimetsu-bridge" >&2
  echo "Run: $KIMETSU_REPO/target/release/kimetsu plugin install codex --workspace $KIMETSU_REPO" >&2
  exit 1
fi

kimetsu_mode_instruction="$KIMETSU_REPO/kimetsu_harbor/kimetsu-mcp-${KIMETSU_MCP_MODE}.md"
if [[ ! -f "$kimetsu_mode_instruction" ]]; then
  echo "Kimetsu MCP mode instruction file missing: $kimetsu_mode_instruction" >&2
  exit 1
fi

kimetsu_mcp_wrapper="$KIMETSU_REPO/kimetsu_harbor/kimetsu-mcp-stdio.sh"
if [[ ! -f "$kimetsu_mcp_wrapper" ]]; then
  echo "Kimetsu MCP wrapper missing: $kimetsu_mcp_wrapper" >&2
  exit 1
fi
kimetsu_brain_helper="$KIMETSU_REPO/kimetsu_harbor/kimetsu-brain-context.sh"
if [[ ! -f "$kimetsu_brain_helper" ]]; then
  echo "Kimetsu brain context helper missing: $kimetsu_brain_helper" >&2
  exit 1
fi
chmod +x "$kimetsu_mcp_wrapper" "$kimetsu_brain_helper" 2>/dev/null || true
if [[ ! -x "$kimetsu_mcp_wrapper" || ! -x "$kimetsu_brain_helper" ]]; then
  echo "Kimetsu MCP helper scripts are not executable inside WSL." >&2
  echo "  wrapper: $kimetsu_mcp_wrapper" >&2
  echo "  brain helper: $kimetsu_brain_helper" >&2
  exit 1
fi
fi

if [[ "${PREFLIGHT_ONLY:-0}" == "0" && "$TASK_LIMIT" =~ ^[0-9]+$ && "$TASK_LIMIT" -lt 1 ]]; then
  echo "TASK_LIMIT must be >= 1 for a Harbor run. Use PREFLIGHT_ONLY=1 for a no-model check." >&2
  exit 1
fi

docker version >/dev/null
harbor --version >/dev/null

if [[ "$kimetsu_enabled" == "1" ]]; then
kimetsu_mcp_probe="$(
  printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}' |
    docker run --rm -i \
      -v "$KIMETSU_REPO:$KIMETSU_REPO:ro" \
      debian:bookworm \
      "$kimetsu_mcp_wrapper"
)"
if ! grep -q "kimetsu_brain_context" <<<"$kimetsu_mcp_probe"; then
  echo "Kimetsu MCP probe did not expose kimetsu_brain_context inside a benchmark-like container." >&2
  printf '%s\n' "$kimetsu_mcp_probe" | head -c 4000 >&2
  echo >&2
  exit 1
fi
if ! grep -q "kimetsu_benchmark_context" <<<"$kimetsu_mcp_probe"; then
  echo "Kimetsu MCP probe did not expose kimetsu_benchmark_context inside a benchmark-like container." >&2
  printf '%s\n' "$kimetsu_mcp_probe" | head -c 4000 >&2
  echo >&2
  exit 1
fi

mount_read_only=false
if [[ "$KIMETSU_MOUNT_READ_ONLY" != "0" ]]; then
  mount_read_only=true
fi

mounts_json='[{"type":"bind","source":"'"$KIMETSU_REPO"'","target":"'"$KIMETSU_REPO"'","read_only":'"$mount_read_only"'}]'
mcp_config="$(mktemp /tmp/codex-kimetsu-mcp.XXXXXX.json)"
trap 'rm -f "$mcp_config"' EXIT
cat >"$mcp_config" <<JSON
{
  "mcpServers": {
    "kimetsu": {
      "transport": "stdio",
      "command": "$kimetsu_mcp_wrapper",
      "args": []
    }
  }
}
JSON
fi

mkdir -p "$JOBS_DIR"

if [[ "${PREFLIGHT_ONLY:-0}" != "0" ]]; then
  echo "Harbor: $(harbor --version)"
  echo "Dataset: $HARBOR_DATASET"
  echo "Codex model: $CODEX_MODEL"
  echo "Codex CLI version pin: $CODEX_VERSION"
  echo "Kimetsu MCP mode: $KIMETSU_MCP_MODE"
  echo "Kimetsu Codex brain enforcement: $codex_brain_enforced"
  if [[ "$kimetsu_enabled" == "1" ]]; then
    echo "Kimetsu mode instruction: $kimetsu_mode_instruction"
    echo "Kimetsu MCP wrapper: $kimetsu_mcp_wrapper"
    echo "Kimetsu brain helper: $kimetsu_brain_helper"
    echo "Kimetsu container binary: $KIMETSU_CONTAINER_BINARY"
    echo "Kimetsu MCP config: $mcp_config"
    printf '%s\n' "$kimetsu_mcp_probe" | head -c 4000
    echo
  else
    echo "Kimetsu MCP disabled: no MCP config, bridge skill, extra instruction, or repo mount will be passed."
  fi
  exit 0
fi

if [[ "$kimetsu_enabled" == "1" ]]; then
if [[ "$codex_brain_enforced" == "1" ]]; then
harbor run \
  -d "$HARBOR_DATASET" \
  --agent-import-path "kimetsu_harbor.codex_kimetsu_agent:CodexKimetsuRequired" \
  -m "$CODEX_MODEL" \
  -l "$TASK_LIMIT" \
  -n "$N_CONCURRENT" \
  -k "$N_ATTEMPTS" \
  --yes \
  --job-name "$JOB_NAME" \
  --jobs-dir "$JOBS_DIR" \
  --agent-setup-timeout-multiplier 3 \
  --mcp-config "$mcp_config" \
  --skill "$KIMETSU_REPO/.codex/skills/kimetsu-bridge" \
  --extra-instruction-path "$kimetsu_mode_instruction" \
  --mounts "$mounts_json" \
  --ak "version=$CODEX_VERSION" \
  --ae "CODEX_AUTH_JSON_PATH=$CODEX_AUTH_JSON_PATH" \
  --ae "KIMETSU_BRAIN_CONTEXT_HELPER=$kimetsu_brain_helper" \
  --ae "KIMETSU_BRAIN_BUDGET_TOKENS=${KIMETSU_BRAIN_BUDGET_TOKENS:-2500}" \
  --ae "KIMETSU_BENCHMARK_DATASET=$HARBOR_DATASET" \
  --ae "KIMETSU_BRAIN_WARM_POLICY=${KIMETSU_BRAIN_WARM_POLICY:-full_warm}" \
  --ae "KIMETSU_MAX_CAPSULES=${KIMETSU_MAX_CAPSULES:-8}" \
  --ae "KIMETSU_REQUIRE_NONEMPTY_BRAIN=${KIMETSU_REQUIRE_NONEMPTY_BRAIN:-0}" \
  --ae "KIMETSU_REQUIRE_BENCHMARK_MEMORY=${KIMETSU_REQUIRE_BENCHMARK_MEMORY:-0}"
else
harbor run \
  -d "$HARBOR_DATASET" \
  -a codex \
  -m "$CODEX_MODEL" \
  -l "$TASK_LIMIT" \
  -n "$N_CONCURRENT" \
  -k "$N_ATTEMPTS" \
  --yes \
  --job-name "$JOB_NAME" \
  --jobs-dir "$JOBS_DIR" \
  --agent-setup-timeout-multiplier 3 \
  --mcp-config "$mcp_config" \
  --skill "$KIMETSU_REPO/.codex/skills/kimetsu-bridge" \
  --extra-instruction-path "$kimetsu_mode_instruction" \
  --mounts "$mounts_json" \
  --ak "version=$CODEX_VERSION" \
  --ae "CODEX_AUTH_JSON_PATH=$CODEX_AUTH_JSON_PATH"
fi
else
harbor run \
  -d "$HARBOR_DATASET" \
  -a codex \
  -m "$CODEX_MODEL" \
  -l "$TASK_LIMIT" \
  -n "$N_CONCURRENT" \
  -k "$N_ATTEMPTS" \
  --yes \
  --job-name "$JOB_NAME" \
  --jobs-dir "$JOBS_DIR" \
  --agent-setup-timeout-multiplier 3 \
  --ak "version=$CODEX_VERSION" \
  --ae "CODEX_AUTH_JSON_PATH=$CODEX_AUTH_JSON_PATH"
fi
