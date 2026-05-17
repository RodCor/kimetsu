#!/bin/bash
# MP-17b — refresh the kimetsu brain pool for the v0.2 20-tool surface.
#
# Background: the curated pool was seeded pre-MP-12 with shell-era
# guidance (sed -i, redirect to /tmp/build.log, find . -maxdepth 2).
# After MP-14 / MP-16 those memories actively contradict the typed
# tool surface (edit_file, shell_background, list_files, glob, ...).
# The brain retrieves by similarity, so wrong-era memories surface
# on the exact tasks where the new tools should be used.
#
# This script:
#   1. Invalidates the 3 memories whose text contradicts the new
#      surface, with a --reason note so the audit trail explains why.
#   2. Adds 12 tool-proficiency capsules that teach the model to
#      reach for edit_file / shell_background / multi_read / plan /
#      think / parallel tool_calls in the right contexts.
#
# Idempotency: invalidates are by-pattern, safe to re-run. Adds
# create new memory IDs; running this twice creates duplicates,
# which won't break the model but will waste retrieval slots.
# Wipe .kimetsu/brain.db to start fully fresh.
#
# Usage (as user kimetsu inside WSL):
#   bash /mnt/e/Kimetsu/kimetsu_harbor/seed-tool-memories.sh

set -uo pipefail

PROJECT="${KIMETSU_HARBOR_PROJECT:-/home/kimetsu/kimetsu-bench-project}"
BIN="${KIMETSU_BIN:-/mnt/e/Kimetsu/target/release/kimetsu}"

if [[ ! -d "$PROJECT/.kimetsu" ]]; then
    echo "PROJECT=$PROJECT has no .kimetsu/ subdir; aborting" >&2
    exit 1
fi
if [[ ! -x "$BIN" ]]; then
    echo "kimetsu binary not found at $BIN; build it or set KIMETSU_BIN" >&2
    exit 1
fi

cd "$PROJECT"
echo "[$(date -u +%H:%M:%SZ)] seeding pool at $PROJECT (binary: $BIN)"
echo

# --- step 1: (MP-17j) DO NOT invalidate pre-MP-12 shell-workflow
# capsules. ---
#
# Original MP-17b retired 3 memories ("redirect to /tmp/build.log",
# "sed -i for in-place edits", "ls + find -maxdepth orientation")
# on the theory that they contradicted the new typed tool surface.
# The MP-17 brain-only validation at 21:32:43Z proved this wrong:
# brain dropped from MP-14d's 7/16 to 5/16, and the 3 newly-lost
# tasks (overfull-hbox, compile-compcert, log-summary-date-ranges)
# were exactly the shell-workflow patterns those capsules covered.
#
# Those memories complement (don't contradict) the typed-tool
# capsules below: typed-tool capsules answer "WHEN to pick which
# tool"; shell-workflow capsules answer "HOW to actually do the
# work in the right shape".
#
# If you ran an OLDER seed-tool-memories.sh that DID invalidate
# those rows, run kimetsu_harbor/restore-shell-memories.sh to add
# fresh copies back into the active pool.
echo "(MP-17j: no invalidations — pre-MP-12 capsules are complementary, not contradictory)"


# --- step 2: add tool-proficiency capsules ---

add() {
    local kind="$1"
    local text="$2"
    local short="${text:0:80}"
    echo "  + $kind: ${short}..."
    "$BIN" brain memory add --scope global_user --kind "$kind" "$text" 2>&1 | sed 's/^/    /' | head -2
}

# 1. shell_background for long-running commands.
add convention \
"For any command expected to take >60s (make, cargo build --release, pip install of large packages, training scripts, ray tracers, long test suites), use shell_background instead of shell_command. The wrapper kills foreground subprocess calls at ~1500s wall-clock. Pattern: shell_background {program,args} -> {handle}, then poll shell_status / shell_output between turns. Insert think between polls to plan, don't tight-loop."

# 2. edit_file for small in-place edits.
add convention \
"When changing a few lines in an existing file, use edit_file {path, old_string, new_string, replace_all?}. old_string must occur exactly once unless replace_all is set. For multiple edits in one call, pass {path, edits:[{old_string,new_string,replace_all?}]}. Hash-checked and ~50x cheaper than write_file. Never use write_file to change a few lines."

# 3. apply_patch for multi-file changes (both diff formats).
add convention \
"For coordinated changes across multiple files, use apply_patch with a diff. Accepts BOTH standard unified diff (--- a/X / +++ b/X / @@ hunks) AND Codex starred-patch format (*** Begin Patch / *** Update File: <path> / *** Add File: <path> / *** Delete File: <path> / *** End Patch). One tool call lands the whole change-set; failures come back as hunk-level errors with rejects[] list."

# 4. read_file slice for big files.
add convention \
"When reading a file you only need part of, pass {offset, limit} to read_file. A 2000-line source dump costs ~10k input tokens; reading lines 400-450 costs ~50. Slice generously — input tokens are the dominant per-turn cost lever."

# 5. multi_read for batched reads.
add convention \
"To read 2+ files at once, use multi_read {paths:[...]} (default slice) or {files:[{path,offset?,limit?}]} (per-file slice). Cheaper than N separate read_file calls because the shell round-trip is paid once. Caps at 25 files per call."

# 6. glob vs search_files.
add convention \
"Use glob {pattern, path?, max_entries?} to find files by NAME pattern ('**/*.rs', 'src/**/test_*.py'). Use search_files {pattern} (grep CONTENT) only when you need code-by-content. glob is cheaper; pick it when you only need filenames. Both available."

# 7. plan for multi-step tasks.
add convention \
"For tasks with >=3 distinct steps, call plan first with the breakdown {todos:[{content,status,activeForm?}]}. status must be pending|in_progress|completed. Update statuses as you progress. The plan persists in conversation history across turns — re-read it to stay oriented on complex tasks."

# 8. parallel tool_calls form.
add convention \
"To batch INDEPENDENT tool calls in one model turn, use parallel form: {thought, tool_calls:[{name,input},{name,input}]}. Saves one model round-trip per extra call. DON'T batch dependent calls (e.g. read after edit must be serialized). Good for orientation (list_files + multi_read), parallel reads, status + diff together."

# 9. think for reasoning slots.
add convention \
"Use think {thought} when you need to reason without burning a real tool call. Pure echo, no I/O, no state change. Useful between background-process polls (so you don't tight-loop), or before a big edit when you need to work out the right old_string to target."

# 10. Workspace orientation pattern.
add convention \
"First action on any task: orient with parallel batch — list_files {path:'.',max_depth:1} + read_file (with offset+limit) of the task instruction file (TASK.md / README.md / INSTRUCTIONS.md / PROBLEM.md). Then decide tool path. Don't blindly explore; the workspace usually has a marker file."

# 11. background polling cadence.
add convention \
"When polling a shell_background process: shell_status returns cheap progress (runtime_sec + byte counts), shell_output {tail_bytes:4096} gives latest stdout/stderr. Don't poll faster than ~10s; insert a think call between polls so you spend the turn planning, not on no-op probes."

# 12. write_file is rare; default to edit_file.
add failure_pattern \
"NEVER use write_file to change a few lines in an existing file. It costs ~50x more tokens than edit_file because you re-emit the entire file, and risks corrupting unrelated content. Use edit_file for partial changes; write_file ONLY for brand-new files or full intentional rewrites."

echo
echo "[$(date -u +%H:%M:%SZ)] seed complete; current pool:"
"$BIN" brain memory list 2>&1 | head -40
echo
total=$("$BIN" brain memory list 2>&1 | grep -cE '^[A-Z0-9]{20,}')
echo "total active memories: $total"
