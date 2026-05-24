#!/usr/bin/env bash
set -euo pipefail

script_dir="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"

if [[ "$#" -lt 1 ]]; then
  echo "usage: $0 <task query>" >&2
  exit 2
fi

query="$*"
tool_name="${KIMETSU_TOOL_NAME:-kimetsu_brain_context}"
stage="${KIMETSU_STAGE:-localization}"
budget_tokens="${KIMETSU_BUDGET_TOKENS:-2500}"
dataset="${KIMETSU_DATASET:-${KIMETSU_BENCHMARK_DATASET:-terminal-bench/terminal-bench-2}}"
task_slug="${KIMETSU_TASK_SLUG:-${KIMETSU_BENCHMARK_TASK_SLUG:-}}"
warm_policy="${KIMETSU_BRAIN_WARM_POLICY:-${KIMETSU_BENCHMARK_WARM_POLICY:-full_warm}}"
require_benchmark_memory="${KIMETSU_REQUIRE_BENCHMARK_MEMORY:-0}"
max_capsules="${KIMETSU_MAX_CAPSULES:-8}"
benchmark_mode="${KIMETSU_BENCHMARK_MODE:-${KIMETSU_MODE:-unknown}}"
benchmark_passed="${KIMETSU_BENCHMARK_PASSED:-}"
benchmark_score="${KIMETSU_BENCHMARK_SCORE:-}"
benchmark_error="${KIMETSU_BENCHMARK_ERROR:-}"
benchmark_summary="${KIMETSU_BENCHMARK_SUMMARY:-}"
benchmark_commands="${KIMETSU_BENCHMARK_COMMANDS:-}"
benchmark_pitfalls="${KIMETSU_BENCHMARK_PITFALLS:-}"
benchmark_verify="${KIMETSU_BENCHMARK_VERIFY:-}"
benchmark_cost_usd="${KIMETSU_BENCHMARK_COST_USD:-}"
benchmark_duration_seconds="${KIMETSU_BENCHMARK_DURATION_SECONDS:-}"
generalized_memory="${KIMETSU_GENERALIZED_MEMORY:-${KIMETSU_BENCHMARK_GENERALIZED_MEMORY:-}}"
memory_role="${KIMETSU_MEMORY_ROLE:-${KIMETSU_BENCHMARK_MEMORY_ROLE:-}}"
task_family="${KIMETSU_TASK_FAMILY:-${KIMETSU_BENCHMARK_TASK_FAMILY:-}}"
applies_to="${KIMETSU_APPLIES_TO:-${KIMETSU_BENCHMARK_APPLIES_TO:-}}"
does_not_apply_to="${KIMETSU_DOES_NOT_APPLY_TO:-${KIMETSU_BENCHMARK_DOES_NOT_APPLY_TO:-}}"
evidence_for="${KIMETSU_EVIDENCE_FOR:-${KIMETSU_BENCHMARK_EVIDENCE_FOR:-}}"
evidence_against="${KIMETSU_EVIDENCE_AGAINST:-${KIMETSU_BENCHMARK_EVIDENCE_AGAINST:-}}"
generalization_rationale="${KIMETSU_GENERALIZATION_RATIONALE:-${KIMETSU_BENCHMARK_GENERALIZATION_RATIONALE:-}}"
generalization_confidence="${KIMETSU_GENERALIZATION_CONFIDENCE:-${KIMETSU_BENCHMARK_GENERALIZATION_CONFIDENCE:-}}"

json_escape() {
  local value="$1"
  value="${value//\\/\\\\}"
  value="${value//\"/\\\"}"
  value="${value//$'\n'/\\n}"
  value="${value//$'\r'/\\r}"
  value="${value//$'\t'/\\t}"
  printf '%s' "$value"
}

if command -v node >/dev/null 2>&1; then
  request="$(
    KIMETSU_QUERY="$query" \
    KIMETSU_TOOL_VALUE="$tool_name" \
    KIMETSU_STAGE_VALUE="$stage" \
    KIMETSU_BUDGET_VALUE="$budget_tokens" \
    KIMETSU_DATASET_VALUE="$dataset" \
    KIMETSU_TASK_SLUG_VALUE="$task_slug" \
    KIMETSU_WARM_POLICY_VALUE="$warm_policy" \
    KIMETSU_REQUIRE_BENCHMARK_MEMORY_VALUE="$require_benchmark_memory" \
    KIMETSU_MAX_CAPSULES_VALUE="$max_capsules" \
    KIMETSU_BENCHMARK_MODE_VALUE="$benchmark_mode" \
    KIMETSU_BENCHMARK_PASSED_VALUE="$benchmark_passed" \
    KIMETSU_BENCHMARK_SCORE_VALUE="$benchmark_score" \
    KIMETSU_BENCHMARK_ERROR_VALUE="$benchmark_error" \
    KIMETSU_BENCHMARK_SUMMARY_VALUE="$benchmark_summary" \
    KIMETSU_BENCHMARK_COMMANDS_VALUE="$benchmark_commands" \
    KIMETSU_BENCHMARK_PITFALLS_VALUE="$benchmark_pitfalls" \
    KIMETSU_BENCHMARK_VERIFY_VALUE="$benchmark_verify" \
    KIMETSU_BENCHMARK_COST_USD_VALUE="$benchmark_cost_usd" \
    KIMETSU_BENCHMARK_DURATION_SECONDS_VALUE="$benchmark_duration_seconds" \
    KIMETSU_GENERALIZED_MEMORY_VALUE="$generalized_memory" \
    KIMETSU_MEMORY_ROLE_VALUE="$memory_role" \
    KIMETSU_TASK_FAMILY_VALUE="$task_family" \
    KIMETSU_APPLIES_TO_VALUE="$applies_to" \
    KIMETSU_DOES_NOT_APPLY_TO_VALUE="$does_not_apply_to" \
    KIMETSU_EVIDENCE_FOR_VALUE="$evidence_for" \
    KIMETSU_EVIDENCE_AGAINST_VALUE="$evidence_against" \
    KIMETSU_GENERALIZATION_RATIONALE_VALUE="$generalization_rationale" \
    KIMETSU_GENERALIZATION_CONFIDENCE_VALUE="$generalization_confidence" \
    node -e '
const budget = Number.parseInt(process.env.KIMETSU_BUDGET_VALUE || "2500", 10);
const maxCapsules = Number.parseInt(process.env.KIMETSU_MAX_CAPSULES_VALUE || "8", 10);
const truthy = (value) => ["1", "true", "yes", "on"].includes(String(value || "").toLowerCase());
const falsy = (value) => ["0", "false", "no", "off"].includes(String(value || "").toLowerCase());
const listValue = (value) => String(value || "").split(/\r?\n/).map((item) => item.trim()).filter(Boolean);
const addString = (args, name, value) => {
  const text = String(value || "").trim();
  if (text) args[name] = text;
};
const addNumber = (args, name, value) => {
  const number = Number.parseFloat(String(value || ""));
  if (Number.isFinite(number)) args[name] = number;
};
const addList = (args, name, value) => {
  const list = listValue(value);
  if (list.length) args[name] = list;
};
const toolName = process.env.KIMETSU_TOOL_VALUE || "kimetsu_brain_context";
const query = process.env.KIMETSU_QUERY || "";
const stage = process.env.KIMETSU_STAGE_VALUE || "localization";
let args;
if (toolName === "kimetsu_benchmark_context") {
  args = {
    task: query,
    dataset: process.env.KIMETSU_DATASET_VALUE || "terminal-bench/terminal-bench-2",
    warm_policy: process.env.KIMETSU_WARM_POLICY_VALUE || "full_warm",
    stage,
    budget_tokens: Number.isFinite(budget) ? budget : 2500,
    max_capsules: Number.isFinite(maxCapsules) ? maxCapsules : 8,
    require_benchmark_memory: truthy(process.env.KIMETSU_REQUIRE_BENCHMARK_MEMORY_VALUE),
  };
  addString(args, "task_slug", process.env.KIMETSU_TASK_SLUG_VALUE);
} else if (toolName === "kimetsu_benchmark_record_outcome") {
  args = {
    task: query,
    dataset: process.env.KIMETSU_DATASET_VALUE || "terminal-bench/terminal-bench-2",
    warm_policy: process.env.KIMETSU_WARM_POLICY_VALUE || "full_warm",
    mode: process.env.KIMETSU_BENCHMARK_MODE_VALUE || "unknown",
  };
  addString(args, "task_slug", process.env.KIMETSU_TASK_SLUG_VALUE);
  if (truthy(process.env.KIMETSU_BENCHMARK_PASSED_VALUE)) args.passed = true;
  if (falsy(process.env.KIMETSU_BENCHMARK_PASSED_VALUE)) args.passed = false;
  addNumber(args, "score", process.env.KIMETSU_BENCHMARK_SCORE_VALUE);
  addString(args, "error", process.env.KIMETSU_BENCHMARK_ERROR_VALUE);
  addString(args, "summary", process.env.KIMETSU_BENCHMARK_SUMMARY_VALUE);
  addList(args, "commands", process.env.KIMETSU_BENCHMARK_COMMANDS_VALUE);
  addList(args, "pitfalls", process.env.KIMETSU_BENCHMARK_PITFALLS_VALUE);
  addList(args, "verify", process.env.KIMETSU_BENCHMARK_VERIFY_VALUE);
  addNumber(args, "cost_usd", process.env.KIMETSU_BENCHMARK_COST_USD_VALUE);
  addNumber(args, "duration_seconds", process.env.KIMETSU_BENCHMARK_DURATION_SECONDS_VALUE);
  addString(args, "generalized_memory", process.env.KIMETSU_GENERALIZED_MEMORY_VALUE);
  addString(args, "memory_role", process.env.KIMETSU_MEMORY_ROLE_VALUE);
  addString(args, "task_family", process.env.KIMETSU_TASK_FAMILY_VALUE);
  addList(args, "applies_to", process.env.KIMETSU_APPLIES_TO_VALUE);
  addList(args, "does_not_apply_to", process.env.KIMETSU_DOES_NOT_APPLY_TO_VALUE);
  addList(args, "evidence_for", process.env.KIMETSU_EVIDENCE_FOR_VALUE);
  addList(args, "evidence_against", process.env.KIMETSU_EVIDENCE_AGAINST_VALUE);
  addString(args, "generalization_rationale", process.env.KIMETSU_GENERALIZATION_RATIONALE_VALUE);
  addNumber(args, "generalization_confidence", process.env.KIMETSU_GENERALIZATION_CONFIDENCE_VALUE);
} else {
  args = {
    query,
    stage,
    budget_tokens: Number.isFinite(budget) ? budget : 2500,
  };
}
const request = {
  jsonrpc: "2.0",
  id: 1,
  method: "tools/call",
  params: {
    name: toolName,
    arguments: args,
  },
};
process.stdout.write(JSON.stringify(request));
'
  )"
elif command -v python3 >/dev/null 2>&1; then
  request="$(
    KIMETSU_QUERY="$query" \
    KIMETSU_TOOL_VALUE="$tool_name" \
    KIMETSU_STAGE_VALUE="$stage" \
    KIMETSU_BUDGET_VALUE="$budget_tokens" \
    KIMETSU_DATASET_VALUE="$dataset" \
    KIMETSU_TASK_SLUG_VALUE="$task_slug" \
    KIMETSU_WARM_POLICY_VALUE="$warm_policy" \
    KIMETSU_REQUIRE_BENCHMARK_MEMORY_VALUE="$require_benchmark_memory" \
    KIMETSU_MAX_CAPSULES_VALUE="$max_capsules" \
    KIMETSU_BENCHMARK_MODE_VALUE="$benchmark_mode" \
    KIMETSU_BENCHMARK_PASSED_VALUE="$benchmark_passed" \
    KIMETSU_BENCHMARK_SCORE_VALUE="$benchmark_score" \
    KIMETSU_BENCHMARK_ERROR_VALUE="$benchmark_error" \
    KIMETSU_BENCHMARK_SUMMARY_VALUE="$benchmark_summary" \
    KIMETSU_BENCHMARK_COMMANDS_VALUE="$benchmark_commands" \
    KIMETSU_BENCHMARK_PITFALLS_VALUE="$benchmark_pitfalls" \
    KIMETSU_BENCHMARK_VERIFY_VALUE="$benchmark_verify" \
    KIMETSU_BENCHMARK_COST_USD_VALUE="$benchmark_cost_usd" \
    KIMETSU_BENCHMARK_DURATION_SECONDS_VALUE="$benchmark_duration_seconds" \
    KIMETSU_GENERALIZED_MEMORY_VALUE="$generalized_memory" \
    KIMETSU_MEMORY_ROLE_VALUE="$memory_role" \
    KIMETSU_TASK_FAMILY_VALUE="$task_family" \
    KIMETSU_APPLIES_TO_VALUE="$applies_to" \
    KIMETSU_DOES_NOT_APPLY_TO_VALUE="$does_not_apply_to" \
    KIMETSU_EVIDENCE_FOR_VALUE="$evidence_for" \
    KIMETSU_EVIDENCE_AGAINST_VALUE="$evidence_against" \
    KIMETSU_GENERALIZATION_RATIONALE_VALUE="$generalization_rationale" \
    KIMETSU_GENERALIZATION_CONFIDENCE_VALUE="$generalization_confidence" \
    python3 - <<'PY'
import json
import os

try:
    budget = int(os.environ.get("KIMETSU_BUDGET_VALUE", "2500"))
except ValueError:
    budget = 2500
try:
    max_capsules = int(os.environ.get("KIMETSU_MAX_CAPSULES_VALUE", "8"))
except ValueError:
    max_capsules = 8

tool_name = os.environ.get("KIMETSU_TOOL_VALUE", "kimetsu_brain_context")
query = os.environ.get("KIMETSU_QUERY", "")
stage = os.environ.get("KIMETSU_STAGE_VALUE", "localization")
truthy = os.environ.get("KIMETSU_REQUIRE_BENCHMARK_MEMORY_VALUE", "").lower() in {
    "1",
    "true",
    "yes",
    "on",
}

def add_string(args, name, value):
    value = (value or "").strip()
    if value:
        args[name] = value

def add_float(args, name, value):
    try:
        args[name] = float(value)
    except (TypeError, ValueError):
        pass

def add_list(args, name, value):
    values = [item.strip() for item in (value or "").splitlines() if item.strip()]
    if values:
        args[name] = values

if tool_name == "kimetsu_benchmark_context":
    args = {
        "task": query,
        "dataset": os.environ.get("KIMETSU_DATASET_VALUE", "terminal-bench/terminal-bench-2"),
        "warm_policy": os.environ.get("KIMETSU_WARM_POLICY_VALUE", "full_warm"),
        "stage": stage,
        "budget_tokens": budget,
        "max_capsules": max_capsules,
        "require_benchmark_memory": truthy,
    }
    task_slug = os.environ.get("KIMETSU_TASK_SLUG_VALUE", "")
    if task_slug:
        args["task_slug"] = task_slug
elif tool_name == "kimetsu_benchmark_record_outcome":
    args = {
        "task": query,
        "dataset": os.environ.get("KIMETSU_DATASET_VALUE", "terminal-bench/terminal-bench-2"),
        "warm_policy": os.environ.get("KIMETSU_WARM_POLICY_VALUE", "full_warm"),
        "mode": os.environ.get("KIMETSU_BENCHMARK_MODE_VALUE", "unknown"),
    }
    add_string(args, "task_slug", os.environ.get("KIMETSU_TASK_SLUG_VALUE"))
    passed = os.environ.get("KIMETSU_BENCHMARK_PASSED_VALUE", "").strip().lower()
    if passed in {"1", "true", "yes", "on"}:
        args["passed"] = True
    elif passed in {"0", "false", "no", "off"}:
        args["passed"] = False
    add_float(args, "score", os.environ.get("KIMETSU_BENCHMARK_SCORE_VALUE"))
    add_string(args, "error", os.environ.get("KIMETSU_BENCHMARK_ERROR_VALUE"))
    add_string(args, "summary", os.environ.get("KIMETSU_BENCHMARK_SUMMARY_VALUE"))
    add_list(args, "commands", os.environ.get("KIMETSU_BENCHMARK_COMMANDS_VALUE"))
    add_list(args, "pitfalls", os.environ.get("KIMETSU_BENCHMARK_PITFALLS_VALUE"))
    add_list(args, "verify", os.environ.get("KIMETSU_BENCHMARK_VERIFY_VALUE"))
    add_float(args, "cost_usd", os.environ.get("KIMETSU_BENCHMARK_COST_USD_VALUE"))
    add_float(args, "duration_seconds", os.environ.get("KIMETSU_BENCHMARK_DURATION_SECONDS_VALUE"))
    add_string(args, "generalized_memory", os.environ.get("KIMETSU_GENERALIZED_MEMORY_VALUE"))
    add_string(args, "memory_role", os.environ.get("KIMETSU_MEMORY_ROLE_VALUE"))
    add_string(args, "task_family", os.environ.get("KIMETSU_TASK_FAMILY_VALUE"))
    add_list(args, "applies_to", os.environ.get("KIMETSU_APPLIES_TO_VALUE"))
    add_list(args, "does_not_apply_to", os.environ.get("KIMETSU_DOES_NOT_APPLY_TO_VALUE"))
    add_list(args, "evidence_for", os.environ.get("KIMETSU_EVIDENCE_FOR_VALUE"))
    add_list(args, "evidence_against", os.environ.get("KIMETSU_EVIDENCE_AGAINST_VALUE"))
    add_string(args, "generalization_rationale", os.environ.get("KIMETSU_GENERALIZATION_RATIONALE_VALUE"))
    add_float(args, "generalization_confidence", os.environ.get("KIMETSU_GENERALIZATION_CONFIDENCE_VALUE"))
else:
    args = {
        "query": query,
        "stage": stage,
        "budget_tokens": budget,
    }

print(json.dumps({
    "jsonrpc": "2.0",
    "id": 1,
    "method": "tools/call",
    "params": {
        "name": tool_name,
        "arguments": args,
    },
}))
PY
  )"
else
  if [[ ! "$budget_tokens" =~ ^[0-9]+$ ]]; then
    budget_tokens=2500
  fi
  if [[ ! "$max_capsules" =~ ^[0-9]+$ ]]; then
    max_capsules=8
  fi
  require_json=false
  case "${require_benchmark_memory,,}" in
    1|true|yes|on) require_json=true ;;
  esac
  tool_json="$(json_escape "$tool_name")"
  query_json="$(json_escape "$query")"
  stage_json="$(json_escape "$stage")"
  if [[ "$tool_name" == "kimetsu_benchmark_context" ]]; then
    dataset_json="$(json_escape "$dataset")"
    task_slug_json="$(json_escape "$task_slug")"
    warm_policy_json="$(json_escape "$warm_policy")"
    request='{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"'"$tool_json"'","arguments":{"task":"'"$query_json"'","dataset":"'"$dataset_json"'","task_slug":"'"$task_slug_json"'","warm_policy":"'"$warm_policy_json"'","stage":"'"$stage_json"'","budget_tokens":'"$budget_tokens"',"max_capsules":'"$max_capsules"',"require_benchmark_memory":'"$require_json"'}}}'
  elif [[ "$tool_name" == "kimetsu_benchmark_record_outcome" ]]; then
    dataset_json="$(json_escape "$dataset")"
    task_slug_json="$(json_escape "$task_slug")"
    warm_policy_json="$(json_escape "$warm_policy")"
    mode_json="$(json_escape "$benchmark_mode")"
    summary_json="$(json_escape "$benchmark_summary")"
    generalized_memory_json="$(json_escape "$generalized_memory")"
    memory_role_json="$(json_escape "$memory_role")"
    request='{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"'"$tool_json"'","arguments":{"task":"'"$query_json"'","dataset":"'"$dataset_json"'","task_slug":"'"$task_slug_json"'","warm_policy":"'"$warm_policy_json"'","mode":"'"$mode_json"'"'
    case "${benchmark_passed,,}" in
      1|true|yes|on) request="$request"', "passed": true' ;;
      0|false|no|off) request="$request"', "passed": false' ;;
    esac
    if [[ -n "$summary_json" ]]; then
      request="$request"', "summary": "'"$summary_json"'"'
    fi
    if [[ -n "$generalized_memory_json" ]]; then
      request="$request"', "generalized_memory": "'"$generalized_memory_json"'"'
    fi
    if [[ -n "$memory_role_json" ]]; then
      request="$request"', "memory_role": "'"$memory_role_json"'"'
    fi
    request="$request"'}}}'
  else
    request='{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"'"$tool_json"'","arguments":{"query":"'"$query_json"'","stage":"'"$stage_json"'","budget_tokens":'"$budget_tokens"'}}}'
  fi
fi

printf '%s\n' "$request" | "$script_dir/kimetsu-mcp-stdio.sh"
